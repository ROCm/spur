// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Shared multi-node PMIx prepare / release helpers for batch launch and srun steps.

use tracing::{error, warn};

use spur_core::node::NodeSource;
use spur_proto::proto::slurm_agent_client::SlurmAgentClient;
use spur_proto::proto::{PreparePmixRequest, ReleasePmixRequest};

pub const MULTI_NODE_PMIX_K8S_UNSUPPORTED: &str =
    "multi-node PMIx is not supported on K8s virtual agents";

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
) -> Result<(), String> {
    let mut client = SlurmAgentClient::connect(agent_addr.to_string())
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
        let mut client = SlurmAgentClient::connect(agent_addr.to_string())
            .await
            .map_err(|e| tonic::Status::unavailable(e.to_string()))?
            .max_decoding_message_size(spur_proto::MAX_GRPC_MESSAGE_SIZE)
            .max_encoding_message_size(spur_proto::MAX_GRPC_REQUEST_SIZE);
        client.release_pmix(ReleasePmixRequest { job_id }).await?;
        Ok::<(), tonic::Status>(())
    }
    .await;
    if let Err(e) = result {
        warn!(job_id, agent = %agent_addr, error = %e, "ReleasePmix rollback failed");
    }
}

pub async fn release_pmix_on_agents(agent_addrs: &[String], job_id: u32) {
    for agent_addr in agent_addrs {
        release_pmix_on_agent(agent_addr, job_id).await;
    }
}

/// Parallel PreparePmix on all nodes. Rolls back successful prepares when any node fails.
pub async fn prepare_pmix_on_nodes(
    job_id: u32,
    run_attempt: u32,
    nodes: Vec<PmixPrepareNode>,
) -> Result<(), String> {
    if nodes.is_empty() {
        return Ok(());
    }

    let mut prepare_set = tokio::task::JoinSet::new();
    for node in nodes {
        let agent_addr = node.agent_addr.clone();
        let node_name = node.node_name.clone();
        let pmix_plan = node.pmix_plan;
        prepare_set.spawn(async move {
            prepare_pmix_on_agent(&agent_addr, job_id, run_attempt, pmix_plan)
                .await
                .map(|()| agent_addr)
                .map_err(|e| format!("{node_name}: {e}"))
        });
    }

    let mut errors: Vec<String> = Vec::new();
    let mut prepared_agents: Vec<String> = Vec::new();
    while let Some(result) = prepare_set.join_next().await {
        match result {
            Ok(Ok(agent_addr)) => prepared_agents.push(agent_addr),
            Ok(Err(e)) => errors.push(e),
            Err(e) => errors.push(format!("prepare task panicked: {e}")),
        }
    }

    if errors.is_empty() {
        return Ok(());
    }

    let detail = errors.join("; ");
    error!(job_id, error = %detail, "PMIx prepare failed — rolling back prepared agents");
    release_pmix_on_agents(&prepared_agents, job_id).await;
    Err(detail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_core::node::NodeSource;

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
}
