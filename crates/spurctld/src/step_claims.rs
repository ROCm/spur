// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Two-phase typed-step dispatch: claim every node's share before any task
//! starts, so a busy node never leaves another node running while srun
//! retries the step.

use std::time::Duration;

use tonic::Status;
use tracing::warn;

use spur_proto::proto::ClaimStepRequest;

/// One step node to claim or release a share on.
#[derive(Clone)]
pub(crate) struct ClaimTarget {
    pub node_name: String,
    pub agent_addr: String,
    pub tasks_on_node: u32,
}

/// The outcome of claiming a typed step's share on every one of its nodes.
pub(crate) enum ClaimVerdict {
    /// Every target holds a claim.
    Claimed(Vec<String>),
    Busy {
        message: String,
    },
    Refused {
        failures: Vec<Status>,
    },
}

/// Pure: a refusal anywhere fails the step; otherwise a busy node makes the whole step wait.
pub(crate) fn classify(results: Vec<(String, Result<(), Status>)>) -> ClaimVerdict {
    let mut claimed = Vec::new();
    let mut busy_message = None;
    let mut failures = Vec::new();
    for (node_name, result) in results {
        match result {
            Ok(()) => claimed.push(node_name),
            Err(status) if spur_proto::is_step_share_busy(&status) => {
                busy_message.get_or_insert(status.message().to_string());
            }
            Err(status) => failures.push(status),
        }
    }
    if !failures.is_empty() {
        return ClaimVerdict::Refused { failures };
    }
    if let Some(message) = busy_message {
        return ClaimVerdict::Busy { message };
    }
    ClaimVerdict::Claimed(claimed)
}

async fn claim_one_node(
    agent_addr: String,
    request: ClaimStepRequest,
    bound: Duration,
) -> Result<(), Status> {
    let attempt = async {
        let mut client = crate::agent_client::connect(agent_addr)
            .await
            .map_err(|e| Status::unavailable(format!("connect failed: {e}")))?;
        client.claim_step(request).await.map(|_| ())
    };
    tokio::time::timeout(bound, attempt)
        .await
        .unwrap_or_else(|_| {
            Err(Status::deadline_exceeded(format!(
                "ClaimStep RPC exceeded {}s",
                bound.as_secs()
            )))
        })
}

/// Claims `base`'s step share on every target, each node getting its own
/// `tasks_on_node`. Bounded by `deadline` (the step's dispatch deadline) or
/// [`crate::scheduler_loop::CANCEL_RPC_TIMEOUT`] when none is set.
pub(crate) async fn claim_on_nodes(
    targets: &[ClaimTarget],
    base: ClaimStepRequest,
    deadline: Option<Duration>,
) -> ClaimVerdict {
    let bound = deadline.unwrap_or(crate::scheduler_loop::CANCEL_RPC_TIMEOUT);
    let mut set = tokio::task::JoinSet::new();
    for target in targets {
        let agent_addr = target.agent_addr.clone();
        let node_name = target.node_name.clone();
        let mut request = base.clone();
        request.num_tasks = target.tasks_on_node;
        set.spawn(async move { (node_name, claim_one_node(agent_addr, request, bound).await) });
    }
    let mut results = Vec::with_capacity(targets.len());
    while let Some(result) = set.join_next().await {
        match result {
            Ok(pair) => results.push(pair),
            Err(e) => results.push((
                String::new(),
                Err(Status::internal(format!("claim task panicked: {e}"))),
            )),
        }
    }
    classify(results)
}

/// Drops a step's claim on every target: a claim that timed out here may
/// still have landed.
pub(crate) async fn release_on_nodes(
    targets: &[ClaimTarget],
    job_id: u32,
    run_attempt: u32,
    step_id: u32,
) {
    let mut set = tokio::task::JoinSet::new();
    for target in targets {
        let agent_addr = target.agent_addr.clone();
        let node_name = target.node_name.clone();
        let request = ClaimStepRequest {
            job_id,
            step_id,
            run_attempt,
            release: true,
            ..Default::default()
        };
        set.spawn(async move {
            let result = claim_one_node(
                agent_addr,
                request,
                crate::scheduler_loop::CANCEL_RPC_TIMEOUT,
            )
            .await;
            (node_name, result)
        });
    }
    while let Some(result) = set.join_next().await {
        if let Ok((node_name, Err(error))) = result {
            warn!(job_id, step_id, node = %node_name, %error, "ClaimStep release failed");
        }
    }
}

/// Releases every target's claim if dropped while still armed — covers the
/// `run_step` future being dropped (e.g. srun killed) between the claim
/// phase starting and the claims being handed to the RunCommand dispatch,
/// where no explicit `release_on_nodes` call runs.
pub(crate) struct StepClaimReleaseGuard {
    targets: Vec<ClaimTarget>,
    job_id: u32,
    run_attempt: u32,
    step_id: u32,
    armed: bool,
}

impl StepClaimReleaseGuard {
    pub(crate) fn new(
        targets: Vec<ClaimTarget>,
        job_id: u32,
        run_attempt: u32,
        step_id: u32,
    ) -> Self {
        Self {
            targets,
            job_id,
            run_attempt,
            step_id,
            armed: true,
        }
    }

    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StepClaimReleaseGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let targets = std::mem::take(&mut self.targets);
        let job_id = self.job_id;
        let run_attempt = self.run_attempt;
        let step_id = self.step_id;
        tokio::spawn(async move {
            release_on_nodes(&targets, job_id, run_attempt, step_id).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn ok(node: &str) -> (String, Result<(), Status>) {
        (node.to_string(), Ok(()))
    }

    fn busy(node: &str) -> (String, Result<(), Status>) {
        (node.to_string(), Err(spur_proto::step_share_busy("busy")))
    }

    #[test]
    fn classify_all_ok_is_claimed() {
        let mut nodes = match classify(vec![ok("n1"), ok("n2")]) {
            ClaimVerdict::Claimed(nodes) => nodes,
            _ => panic!("expected Claimed"),
        };
        nodes.sort();
        assert_eq!(nodes, vec!["n1".to_string(), "n2".to_string()]);
    }

    #[test]
    fn classify_one_busy_and_one_ok_is_busy() {
        match classify(vec![
            ok("n1"),
            ("n2".to_string(), Err(spur_proto::step_share_busy("busy"))),
        ]) {
            ClaimVerdict::Busy { message } => assert_eq!(message, "busy"),
            _ => panic!("expected Busy"),
        }
    }

    #[test]
    fn classify_an_invalid_argument_is_refused() {
        match classify(vec![(
            "n1".to_string(),
            Err(Status::invalid_argument("bad request")),
        )]) {
            ClaimVerdict::Refused { failures } => assert_eq!(failures.len(), 1),
            _ => panic!("expected Refused"),
        }
    }

    #[test]
    fn classify_unimplemented_is_refused() {
        match classify(vec![
            ok("n1"),
            ("n2".to_string(), Err(Status::unimplemented("ClaimStep"))),
        ]) {
            ClaimVerdict::Refused { failures } => assert_eq!(failures.len(), 1),
            _ => panic!("expected Refused"),
        }
    }

    #[test]
    fn classify_a_refusal_outranks_busy() {
        match classify(vec![
            busy("n1"),
            (
                "n2".to_string(),
                Err(Status::invalid_argument("bad request")),
            ),
        ]) {
            ClaimVerdict::Refused { failures } => {
                assert_eq!(failures.len(), 1);
                assert_eq!(failures[0].code(), tonic::Code::InvalidArgument);
            }
            _ => panic!("expected Refused"),
        }
    }

    /// Minimal SlurmAgent that only answers ClaimStep, for a unit test that
    /// drives `release_on_nodes` against a real gRPC server.
    struct RecordingAgent {
        claims: Arc<std::sync::Mutex<Vec<ClaimStepRequest>>>,
    }

    #[tonic::async_trait]
    impl spur_proto::proto::slurm_agent_server::SlurmAgent for RecordingAgent {
        type StreamJobOutputStream =
            tonic::codegen::BoxStream<spur_proto::proto::StreamJobOutputChunk>;
        type InteractiveSessionStream =
            tonic::codegen::BoxStream<spur_proto::proto::InteractiveOutput>;

        async fn ping(
            &self,
            _request: tonic::Request<()>,
        ) -> Result<tonic::Response<spur_proto::proto::PingResponse>, Status> {
            Ok(tonic::Response::new(
                spur_proto::proto::PingResponse::default(),
            ))
        }
        async fn launch_job(
            &self,
            _request: tonic::Request<spur_proto::proto::LaunchJobRequest>,
        ) -> Result<tonic::Response<spur_proto::proto::LaunchJobResponse>, Status> {
            Err(Status::unimplemented("not used in this test"))
        }
        async fn prepare_pmix(
            &self,
            _request: tonic::Request<spur_proto::proto::PreparePmixRequest>,
        ) -> Result<tonic::Response<spur_proto::proto::PreparePmixResponse>, Status> {
            Err(Status::unimplemented("not used in this test"))
        }
        async fn release_pmix(
            &self,
            _request: tonic::Request<spur_proto::proto::ReleasePmixRequest>,
        ) -> Result<tonic::Response<spur_proto::proto::ReleasePmixResponse>, Status> {
            Err(Status::unimplemented("not used in this test"))
        }
        async fn start_job(
            &self,
            _request: tonic::Request<spur_proto::proto::AgentStartJobRequest>,
        ) -> Result<tonic::Response<()>, Status> {
            Err(Status::unimplemented("not used in this test"))
        }
        async fn cancel_job(
            &self,
            _request: tonic::Request<spur_proto::proto::AgentCancelJobRequest>,
        ) -> Result<tonic::Response<()>, Status> {
            Err(Status::unimplemented("not used in this test"))
        }
        async fn signal_job(
            &self,
            _request: tonic::Request<spur_proto::proto::AgentSignalJobRequest>,
        ) -> Result<tonic::Response<()>, Status> {
            Err(Status::unimplemented("not used in this test"))
        }
        async fn suspend_job(
            &self,
            _request: tonic::Request<spur_proto::proto::AgentSuspendJobRequest>,
        ) -> Result<tonic::Response<()>, Status> {
            Err(Status::unimplemented("not used in this test"))
        }
        async fn get_node_resources(
            &self,
            _request: tonic::Request<()>,
        ) -> Result<tonic::Response<spur_proto::proto::NodeResourcesResponse>, Status> {
            Err(Status::unimplemented("not used in this test"))
        }
        async fn probe_stepd(
            &self,
            _request: tonic::Request<spur_proto::proto::StepdProbeRequest>,
        ) -> Result<tonic::Response<spur_proto::proto::StepdProbeResponse>, Status> {
            Err(Status::unimplemented("not used in this test"))
        }
        async fn exec_in_job(
            &self,
            _request: tonic::Request<spur_proto::proto::ExecInJobRequest>,
        ) -> Result<tonic::Response<spur_proto::proto::ExecInJobResponse>, Status> {
            Err(Status::unimplemented("not used in this test"))
        }
        async fn run_command(
            &self,
            _request: tonic::Request<spur_proto::proto::RunCommandRequest>,
        ) -> Result<tonic::Response<spur_proto::proto::RunCommandResponse>, Status> {
            Err(Status::unimplemented("not used in this test"))
        }
        async fn cancel_step(
            &self,
            _request: tonic::Request<spur_proto::proto::CancelStepRequest>,
        ) -> Result<tonic::Response<()>, Status> {
            Err(Status::unimplemented("not used in this test"))
        }
        async fn claim_step(
            &self,
            request: tonic::Request<ClaimStepRequest>,
        ) -> Result<tonic::Response<()>, Status> {
            self.claims.lock().unwrap().push(request.into_inner());
            Ok(tonic::Response::new(()))
        }
        async fn register_job_allocation(
            &self,
            _request: tonic::Request<spur_proto::proto::RegisterJobAllocationRequest>,
        ) -> Result<tonic::Response<spur_proto::proto::RegisterJobAllocationResponse>, Status>
        {
            Err(Status::unimplemented("not used in this test"))
        }
        async fn await_step(
            &self,
            _request: tonic::Request<spur_proto::proto::AwaitStepRequest>,
        ) -> Result<tonic::Response<spur_proto::proto::RunCommandResponse>, Status> {
            Err(Status::unimplemented("not used in this test"))
        }
        async fn stream_job_output(
            &self,
            _request: tonic::Request<spur_proto::proto::StreamJobOutputRequest>,
        ) -> Result<tonic::Response<Self::StreamJobOutputStream>, Status> {
            Err(Status::unimplemented("not used in this test"))
        }
        async fn interactive_session(
            &self,
            _request: tonic::Request<tonic::Streaming<spur_proto::proto::InteractiveInput>>,
        ) -> Result<tonic::Response<Self::InteractiveSessionStream>, Status> {
            Err(Status::unimplemented("not used in this test"))
        }
        async fn start_cluster_component(
            &self,
            _request: tonic::Request<spur_proto::proto::StartClusterComponentRequest>,
        ) -> Result<tonic::Response<spur_proto::proto::StartClusterComponentResponse>, Status>
        {
            Err(Status::unimplemented("not used in this test"))
        }
        async fn stop_cluster_component(
            &self,
            _request: tonic::Request<spur_proto::proto::StopClusterComponentRequest>,
        ) -> Result<tonic::Response<spur_proto::proto::StopClusterComponentResponse>, Status>
        {
            Err(Status::unimplemented("not used in this test"))
        }
        async fn get_cluster_component_status(
            &self,
            _request: tonic::Request<spur_proto::proto::GetClusterComponentStatusRequest>,
        ) -> Result<tonic::Response<spur_proto::proto::GetClusterComponentStatusResponse>, Status>
        {
            Err(Status::unimplemented("not used in this test"))
        }
        async fn create_k0s_join_token(
            &self,
            _request: tonic::Request<spur_proto::proto::CreateK0sJoinTokenRequest>,
        ) -> Result<tonic::Response<spur_proto::proto::CreateK0sJoinTokenResponse>, Status>
        {
            Err(Status::unimplemented("not used in this test"))
        }
        async fn drain_k8s_node(
            &self,
            _request: tonic::Request<spur_proto::proto::DrainK8sNodeRequest>,
        ) -> Result<tonic::Response<spur_proto::proto::DrainK8sNodeResponse>, Status> {
            Err(Status::unimplemented("not used in this test"))
        }
        async fn delete_k8s_node(
            &self,
            _request: tonic::Request<spur_proto::proto::DeleteK8sNodeRequest>,
        ) -> Result<tonic::Response<spur_proto::proto::DeleteK8sNodeResponse>, Status> {
            Err(Status::unimplemented("not used in this test"))
        }
        async fn get_kubeconfig(
            &self,
            _request: tonic::Request<spur_proto::proto::GetKubeconfigRequest>,
        ) -> Result<tonic::Response<spur_proto::proto::GetKubeconfigResponse>, Status> {
            Err(Status::unimplemented("not used in this test"))
        }
        async fn apply_mesh(
            &self,
            _request: tonic::Request<spur_proto::proto::MeshMembership>,
        ) -> Result<tonic::Response<spur_proto::proto::ApplyMeshResponse>, Status> {
            Err(Status::unimplemented("not used in this test"))
        }
    }

    async fn spawn_recording_agent() -> (
        std::net::SocketAddr,
        Arc<std::sync::Mutex<Vec<ClaimStepRequest>>>,
    ) {
        let claims = Arc::new(std::sync::Mutex::new(Vec::new()));
        let agent = RecordingAgent {
            claims: claims.clone(),
        };
        let incoming =
            tonic::transport::server::TcpIncoming::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = incoming.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(spur_proto::agent_server(agent))
                .serve_with_incoming(incoming)
                .await;
        });
        (addr, claims)
    }

    // The TTL is only a backstop: a release must still reach every target,
    // including one whose claim result the controller never saw.
    #[tokio::test]
    async fn a_timed_out_claim_still_gets_a_release() {
        let (addr1, claims1) = spawn_recording_agent().await;
        let (addr2, claims2) = spawn_recording_agent().await;
        let targets = vec![
            ClaimTarget {
                node_name: "n1".into(),
                agent_addr: format!("http://{addr1}"),
                tasks_on_node: 1,
            },
            ClaimTarget {
                node_name: "n2".into(),
                agent_addr: format!("http://{addr2}"),
                tasks_on_node: 1,
            },
        ];

        release_on_nodes(&targets, 7, 1, 3).await;

        let n1_claims = claims1.lock().unwrap();
        let n2_claims = claims2.lock().unwrap();
        assert_eq!(n1_claims.len(), 1);
        assert_eq!(n2_claims.len(), 1);
        assert!(n1_claims[0].release);
        assert!(n2_claims[0].release);
    }
}
