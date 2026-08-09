// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! In-process `SlurmController` mock for exercising CLI code paths that hold a
//! live [`SlurmControllerClient`].
//!
//! Follows the same shape as the `MockAgent` harness in `spurctld`: bind an
//! ephemeral localhost port, serve a hand-written service on it, and hand the
//! caller back the address plus a shared record of what the server observed.
//! Only a handful of RPCs are implemented (`CreateJobStep`, `RunStep`,
//! `UpdateNode`); every other RPC reports `unimplemented` so an unexpected call
//! fails loudly instead of silently returning a default.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use spur_proto::proto::slurm_controller_client::SlurmControllerClient;
use spur_proto::proto::{self, slurm_controller_server};
use tonic::transport::{Channel, Endpoint};

/// Step id the mock hands back from `CreateJobStep`. Distinctive so tests can
/// prove it is threaded into the follow-up `RunStep` rather than defaulted.
pub(crate) const MOCK_STEP_ID: u32 = 4242;

/// Exit code the mock reports from `RunStep`.
pub(crate) const MOCK_EXIT_CODE: i32 = 7;

/// What the mock controller actually received, shared with the test body.
#[derive(Clone, Default)]
pub(crate) struct StepCapture {
    create_step_num_tasks: Arc<AtomicU32>,
    run_step_step_id: Arc<AtomicU32>,
    run_step_calls: Arc<AtomicU32>,
    update_node_names: Arc<Mutex<Vec<String>>>,
    /// Node names that `update_node` should reject with `NotFound`.
    update_node_fail_names: Arc<Mutex<HashSet<String>>>,
}

impl StepCapture {
    /// Task count carried by the most recent `CreateJobStep`.
    pub(crate) fn create_step_num_tasks(&self) -> u32 {
        self.create_step_num_tasks.load(Ordering::SeqCst)
    }

    /// Step id carried by the most recent `RunStep`.
    pub(crate) fn run_step_step_id(&self) -> u32 {
        self.run_step_step_id.load(Ordering::SeqCst)
    }

    /// Number of `RunStep` calls, so tests can assert dispatch stopped early.
    pub(crate) fn run_step_calls(&self) -> u32 {
        self.run_step_calls.load(Ordering::SeqCst)
    }

    pub(crate) fn update_node_names(&self) -> Vec<String> {
        self.update_node_names.lock().unwrap().clone()
    }

    pub(crate) fn set_update_node_fail_names(&self, names: HashSet<String>) {
        *self.update_node_fail_names.lock().unwrap() = names;
    }
}

struct MockController {
    capture: StepCapture,
}

/// Emit the whole `impl` block, including the `#[tonic::async_trait]`
/// attribute. The attribute has to be applied by the macro rather than written
/// above the invocation: `async_trait` rewrites `async fn` signatures, and it
/// only sees method bodies that already exist when it runs.
macro_rules! mock_controller_impl {
    (
        implemented { $($implemented:tt)* }
        unimplemented { $($method:ident($req:ty) -> $resp:ty;)* }
    ) => {
        #[tonic::async_trait]
        impl slurm_controller_server::SlurmController for MockController {
            $($implemented)*
            $(
                async fn $method(
                    &self,
                    _request: tonic::Request<$req>,
                ) -> Result<tonic::Response<$resp>, tonic::Status> {
                    Err(tonic::Status::unimplemented(stringify!($method)))
                }
            )*
        }
    };
}

mock_controller_impl! {
    implemented {
        async fn create_job_step(
            &self,
            request: tonic::Request<proto::CreateJobStepRequest>,
        ) -> Result<tonic::Response<proto::CreateJobStepResponse>, tonic::Status> {
            self.capture
                .create_step_num_tasks
                .store(request.into_inner().num_tasks, Ordering::SeqCst);
            Ok(tonic::Response::new(proto::CreateJobStepResponse {
                step_id: MOCK_STEP_ID,
                node_addr: String::new(),
            }))
        }

        async fn run_step(
            &self,
            request: tonic::Request<proto::RunStepRequest>,
        ) -> Result<tonic::Response<proto::RunStepResponse>, tonic::Status> {
            self.capture
                .run_step_step_id
                .store(request.into_inner().step_id, Ordering::SeqCst);
            self.capture.run_step_calls.fetch_add(1, Ordering::SeqCst);
            Ok(tonic::Response::new(proto::RunStepResponse {
                exit_code: MOCK_EXIT_CODE,
                stdout: String::new(),
                stderr: String::new(),
                node: String::new(),
            }))
        }

        async fn update_node(
            &self,
            request: tonic::Request<proto::UpdateNodeRequest>,
        ) -> Result<tonic::Response<()>, tonic::Status> {
            let name = request.into_inner().name;
            self.capture.update_node_names.lock().unwrap().push(name.clone());
            if self.capture.update_node_fail_names.lock().unwrap().contains(&name) {
                return Err(tonic::Status::not_found(format!("node {name} not found")));
            }
            Ok(tonic::Response::new(()))
        }
    }
    unimplemented {
        submit_job(proto::SubmitJobRequest) -> proto::SubmitJobResponse;
        get_jobs(proto::GetJobsRequest) -> proto::GetJobsResponse;
        get_job(proto::GetJobRequest) -> proto::JobInfo;
        cancel_job(proto::CancelJobRequest) -> ();
        complete_job(proto::CompleteJobRequest) -> ();
        suspend_job(proto::SuspendJobRequest) -> ();
        resume_job(proto::ResumeJobRequest) -> ();
        update_job(proto::UpdateJobRequest) -> ();
        get_nodes(proto::GetNodesRequest) -> proto::GetNodesResponse;
        get_node(proto::GetNodeRequest) -> proto::NodeInfo;
        drain_node(proto::DrainNodeRequest) -> proto::DrainNodeResponse;
        deregister_node(proto::DeregisterNodeRequest) -> proto::DeregisterNodeResponse;
        deregister_agent(proto::DeregisterAgentRequest) -> ();
        get_partitions(proto::GetPartitionsRequest) -> proto::GetPartitionsResponse;
        create_partition(proto::CreatePartitionRequest) -> ();
        update_partition(proto::UpdatePartitionRequest) -> ();
        delete_partition(proto::DeletePartitionRequest) -> ();
        reconfigure(()) -> ();
        get_job_steps(proto::GetJobStepsRequest) -> proto::GetJobStepsResponse;
        ping(()) -> proto::PingResponse;
        get_job_metrics(()) -> proto::JobMetrics;
        get_node_metrics(()) -> proto::NodeMetrics;
        get_rpc_stats(()) -> proto::RpcStats;
        reset_diag_stats(()) -> ();
        get_sched_stats(()) -> proto::SchedStats;
        register_agent(proto::RegisterAgentRequest) -> proto::RegisterAgentResponse;
        heartbeat(proto::HeartbeatRequest) -> proto::HeartbeatResponse;
        poll_agent_commands(proto::PollAgentCommandsRequest) -> proto::PollAgentCommandsResponse;
        acknowledge_agent_command(proto::AcknowledgeAgentCommandRequest) -> ();
        create_token(proto::CreateTokenRequest) -> proto::CreateTokenResponse;
        list_tokens(proto::ListTokensRequest) -> proto::ListTokensResponse;
        revoke_token(proto::RevokeTokenRequest) -> proto::RevokeTokenResponse;
        report_job_status(proto::ReportJobStatusRequest) -> ();
        create_reservation(proto::CreateReservationRequest) -> ();
        update_reservation(proto::UpdateReservationRequest) -> ();
        delete_reservation(proto::DeleteReservationRequest) -> ();
        list_reservations(proto::ListReservationsRequest) -> proto::ListReservationsResponse;
        exec_in_job(proto::ExecInJobRequest) -> proto::ExecInJobResponse;
        cluster_up(proto::ClusterUpRequest) -> proto::ClusterUpResponse;
        cluster_down(proto::ClusterDownRequest) -> proto::ClusterDownResponse;
        cluster_status(proto::ClusterStatusRequest) -> proto::ClusterStatusResponse;
        cluster_kubeconfig(proto::ClusterKubeconfigRequest) -> proto::ClusterKubeconfigResponse;
    }
}

/// Serve the mock on an OS-assigned localhost port.
pub(crate) async fn spawn() -> (SocketAddr, StepCapture) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let capture = StepCapture::default();
    let service = MockController {
        capture: capture.clone(),
    };
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(spur_proto::controller_server(service))
            .serve_with_incoming(incoming),
    );
    (addr, capture)
}

/// Dial the mock through the same helper production code uses.
pub(crate) async fn client(addr: SocketAddr) -> SlurmControllerClient<Channel> {
    let channel = spur_client::connect_channel(&format!("http://{addr}"))
        .await
        .expect("connect to mock controller");
    spur_proto::controller_client(channel)
}

/// A client whose channel is created without dialing, so the first RPC is what
/// fails. Lets tests drive the RPC-failure path without a server.
pub(crate) fn lazy_client(addr: SocketAddr) -> SlurmControllerClient<Channel> {
    let channel = Endpoint::from_shared(format!("http://{addr}"))
        .expect("valid endpoint")
        .connect_lazy();
    spur_proto::controller_client(channel)
}

/// Reserve a localhost port and release it, so connecting to it is refused
/// immediately instead of hanging until the connect timeout.
pub(crate) async fn unreachable_addr() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    listener.local_addr().expect("local addr")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RPCs the mock does not implement must surface as `Unimplemented` rather
    /// than a default-valued success, so a test that drifts onto an unmocked
    /// call fails instead of silently passing.
    #[tokio::test]
    async fn unmocked_rpc_reports_unimplemented() {
        let (addr, _capture) = spawn().await;
        let status = client(addr)
            .await
            .ping(())
            .await
            .expect_err("ping is not mocked");
        assert_eq!(status.code(), tonic::Code::Unimplemented);
        assert_eq!(status.message(), "ping");
    }
}
