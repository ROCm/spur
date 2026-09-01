// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Shared multi-node PMIx prepare / release helpers for batch launch and srun steps.

use std::time::Duration;

use tracing::{error, warn};

use spur_core::node::NodeSource;
use spur_proto::proto::{PreparePmixRequest, ReleasePmixRequest};

pub const MULTI_NODE_PMIX_K8S_UNSUPPORTED: &str =
    "multi-node PMIx is not supported on K8s virtual agents";

/// Rollback is best-effort, so it gets its own short bound rather than the caller's
/// dispatch deadline — an agent wedged in `server_stop` must not hold the loop.
const RELEASE_RPC_TIMEOUT: Duration = Duration::from_secs(5);

/// Reject multi-node PMIx at submit when the user pins a K8s virtual agent.
pub fn validate_multi_node_pmix_nodelist(
    mpi: &str,
    num_nodes: u32,
    nodelist: Option<&str>,
    node_source: impl Fn(&str) -> Option<NodeSource>,
) -> Result<(), String> {
    if mpi != spur_core::mpi::MPI_PMIX || num_nodes <= 1 {
        return Ok(());
    }
    let Some(nodelist) = nodelist.filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    for name in nodelist.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if node_source(name).is_some_and(|source| matches!(source, NodeSource::Kubernetes { .. })) {
            return Err(MULTI_NODE_PMIX_K8S_UNSUPPORTED.into());
        }
    }
    Ok(())
}

/// Returns an error detail when any node is a K8s virtual agent.
pub fn multi_node_pmix_unsupported(
    sources: impl IntoIterator<Item = NodeSource>,
) -> Option<String> {
    for source in sources {
        if matches!(source, NodeSource::Kubernetes { .. }) {
            return Some(MULTI_NODE_PMIX_K8S_UNSUPPORTED.into());
        }
    }
    None
}

/// One agent target for a parallel PreparePmix RPC.
pub struct PmixPrepareNode {
    pub node_name: String,
    pub agent_addr: String,
    pub pmix_plan: spur_proto::proto::PmixLaunchPlan,
}

pub async fn prepare_pmix_on_agent(
    agent_addr: &str,
    job_id: u32,
    run_attempt: u32,
    pmix_plan: spur_proto::proto::PmixLaunchPlan,
    deadline: Option<Duration>,
) -> Result<(), String> {
    // The agent runs PMIx `server_start` inline in this handler, so it keeps answering
    // HTTP/2 pings while wedged — only a call-site deadline bounds it.
    let prepare = prepare_pmix_on_agent_inner(agent_addr, job_id, run_attempt, pmix_plan);
    match deadline {
        Some(limit) => tokio::time::timeout(limit, prepare)
            .await
            .unwrap_or_else(|_| Err(format!("PreparePmix RPC exceeded {}s", limit.as_secs()))),
        None => prepare.await,
    }
}

async fn prepare_pmix_on_agent_inner(
    agent_addr: &str,
    job_id: u32,
    run_attempt: u32,
    pmix_plan: spur_proto::proto::PmixLaunchPlan,
) -> Result<(), String> {
    let mut client = crate::agent_client::connect(agent_addr.to_string())
        .await
        .map_err(|e| format!("connect failed: {e}"))?
        .max_decoding_message_size(spur_proto::MAX_GRPC_MESSAGE_SIZE)
        .max_encoding_message_size(spur_proto::MAX_GRPC_REQUEST_SIZE);
    let resp = client
        .prepare_pmix(PreparePmixRequest {
            job_id,
            pmix_plan: Some(pmix_plan),
            run_attempt,
        })
        .await
        .map_err(|e| format!("PreparePmix RPC failed: {e}"))?
        .into_inner();
    if resp.success {
        Ok(())
    } else if resp.error.is_empty() {
        Err("PreparePmix rejected without detail".into())
    } else {
        Err(resp.error)
    }
}

pub async fn release_pmix_on_agent(agent_addr: &str, job_id: u32) {
    let result = async {
        let mut client = crate::agent_client::connect(agent_addr.to_string())
            .await
            .map_err(|e| tonic::Status::unavailable(e.to_string()))?
            .max_decoding_message_size(spur_proto::MAX_GRPC_MESSAGE_SIZE)
            .max_encoding_message_size(spur_proto::MAX_GRPC_REQUEST_SIZE);
        client.release_pmix(ReleasePmixRequest { job_id }).await?;
        Ok::<(), tonic::Status>(())
    };
    match tokio::time::timeout(RELEASE_RPC_TIMEOUT, result).await {
        Ok(Err(e)) => {
            warn!(job_id, agent = %agent_addr, error = %e, "ReleasePmix rollback failed")
        }
        Err(_) => {
            warn!(job_id, agent = %agent_addr, "ReleasePmix rollback timed out")
        }
        Ok(Ok(())) => {}
    }
}

pub async fn release_pmix_on_agents(agent_addrs: &[String], job_id: u32) {
    let mut release_set = tokio::task::JoinSet::new();
    for agent_addr in agent_addrs {
        let agent_addr = agent_addr.clone();
        release_set.spawn(async move {
            release_pmix_on_agent(&agent_addr, job_id).await;
        });
    }
    while release_set.join_next().await.is_some() {}
}

/// Parallel PreparePmix on all nodes. Rolls back successful prepares when any node fails.
pub async fn prepare_pmix_on_nodes(
    job_id: u32,
    run_attempt: u32,
    nodes: Vec<PmixPrepareNode>,
    deadline: Option<Duration>,
) -> Result<(), String> {
    if nodes.is_empty() {
        return Ok(());
    }

    let all_agent_addrs: Vec<String> = nodes.iter().map(|n| n.agent_addr.clone()).collect();

    let mut prepare_set = tokio::task::JoinSet::new();
    for node in nodes {
        let agent_addr = node.agent_addr.clone();
        let node_name = node.node_name.clone();
        let pmix_plan = node.pmix_plan;
        prepare_set.spawn(async move {
            prepare_pmix_on_agent(&agent_addr, job_id, run_attempt, pmix_plan, deadline)
                .await
                .map(|()| agent_addr)
                .map_err(|e| format!("{node_name}: {e}"))
        });
    }

    let mut errors: Vec<String> = Vec::new();
    while let Some(result) = prepare_set.join_next().await {
        match result {
            Ok(Ok(_agent_addr)) => {}
            Ok(Err(e)) => errors.push(e),
            Err(e) => errors.push(format!("prepare task panicked: {e}")),
        }
    }

    if errors.is_empty() {
        return Ok(());
    }

    let detail = errors.join("; ");
    error!(job_id, error = %detail, "PMIx prepare failed — rolling back prepared agents");
    release_pmix_on_agents(&all_agent_addrs, job_id).await;
    Err(detail)
}

/// Rolls back controller-side PMIx prepare when an srun step handler is cancelled
/// before the normal release path runs.
pub struct PmixPreparedReleaseGuard {
    job_id: u32,
    agent_addrs: Vec<String>,
    release: bool,
}

impl PmixPreparedReleaseGuard {
    pub fn new(job_id: u32, agent_addrs: Vec<String>) -> Self {
        Self {
            job_id,
            agent_addrs,
            release: true,
        }
    }

    pub fn disarm(&mut self) {
        self.release = false;
    }
}

impl Drop for PmixPreparedReleaseGuard {
    fn drop(&mut self) {
        if !self.release {
            return;
        }
        let job_id = self.job_id;
        let addrs = self.agent_addrs.clone();
        tokio::spawn(async move {
            release_pmix_on_agents(&addrs, job_id).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_core::node::NodeSource;

    /// A listener that completes the TCP handshake and then never serves HTTP/2. Keepalive alone
    /// needs ~20s to give up on it, so an elapsed bound below that pins the deadline as the cause.
    async fn silent_agent() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut accepted = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                accepted.push(sock);
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prepare_pmix_gives_up_on_a_silent_agent_once_the_deadline_passes() {
        let agent = silent_agent().await;
        let started = std::time::Instant::now();

        let err = prepare_pmix_on_agent(
            &agent,
            1,
            0,
            spur_proto::proto::PmixLaunchPlan::default(),
            Some(Duration::from_secs(1)),
        )
        .await
        .expect_err("a silent agent must not report a successful prepare");

        assert!(err.contains("exceeded"), "unexpected error: {err}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the prepare fan-out held the assignment loop well past its deadline"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn release_pmix_gives_up_on_a_silent_agent() {
        let agent = silent_agent().await;
        let started = std::time::Instant::now();

        release_pmix_on_agent(&agent, 1).await;

        assert!(
            started.elapsed() < RELEASE_RPC_TIMEOUT * 3,
            "a best-effort rollback must never be what blocks the caller"
        );
    }

    #[test]
    fn multi_node_pmix_unsupported_on_k8s_agents() {
        let err = multi_node_pmix_unsupported([NodeSource::Kubernetes {
            namespace: "spur-ci".into(),
        }]);
        assert_eq!(err.as_deref(), Some(MULTI_NODE_PMIX_K8S_UNSUPPORTED));
    }

    #[test]
    fn multi_node_pmix_allowed_on_native_hosts() {
        let err = multi_node_pmix_unsupported([NodeSource::NativeHost]);
        assert!(err.is_none());
    }

    #[test]
    fn multi_node_pmix_nodelist_rejects_k8s_agent_at_submit() {
        let err = validate_multi_node_pmix_nodelist(
            spur_core::mpi::MPI_PMIX,
            2,
            Some("k8s-worker1"),
            |name| {
                if name == "k8s-worker1" {
                    Some(NodeSource::Kubernetes {
                        namespace: "spur-ci".into(),
                    })
                } else {
                    Some(NodeSource::NativeHost)
                }
            },
        );
        assert_eq!(err.unwrap_err(), MULTI_NODE_PMIX_K8S_UNSUPPORTED);
    }

    #[test]
    fn multi_node_pmix_nodelist_allows_native_hosts_at_submit() {
        assert!(validate_multi_node_pmix_nodelist(
            spur_core::mpi::MPI_PMIX,
            2,
            Some("n1,n2"),
            |_| Some(NodeSource::NativeHost),
        )
        .is_ok());
    }
}
