// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Conversion between controller metrics snapshots and gRPC messages.

use spur_core::job::JobState;
use spur_core::node::NodeState;
use spur_metrics::job::JobMetricsSnapshot;
use spur_metrics::node::NodeMetricsSnapshot;
use spur_metrics::RpcStatsSnapshot;
use spur_proto::proto::{
    JobMetrics, JobStateCount, NodeMetrics, NodeMetricsEntry, NodeStateCount, RpcOperationStats,
    RpcStats,
};

pub fn rpc_stats_to_proto(snap: &RpcStatsSnapshot) -> RpcStats {
    let by_operation = snap
        .by_operation
        .iter()
        .map(|op| RpcOperationStats {
            operation: op.operation.clone(),
            count: op.count,
            total_time_us: op.total_time_us,
            avg_time_us: op.avg_time_us(),
        })
        .collect();

    RpcStats { by_operation }
}

pub fn job_metrics_to_proto(snap: &JobMetricsSnapshot) -> JobMetrics {
    let by_state = JobState::ALL
        .iter()
        .map(|&state| JobStateCount {
            state: state.to_proto_i32(),
            count: snap.count_state(state),
        })
        .collect();

    JobMetrics {
        total: snap.total,
        by_state,
        held_pending: snap.held_pending,
        running_cpus: snap.running_cpus,
        running_memory_bytes: snap.running_memory_bytes,
        running_gpus: snap.running_gpus,
    }
}

pub fn node_metrics_to_proto(snap: &NodeMetricsSnapshot) -> NodeMetrics {
    let by_state = NodeState::ALL
        .iter()
        .map(|&state| NodeStateCount {
            state: state.to_proto_i32(),
            count: snap.count_state(state),
        })
        .collect();

    let per_node = snap
        .per_node
        .iter()
        .map(|n| NodeMetricsEntry {
            name: n.name.clone(),
            total_cpus: n.total_cpus,
            alloc_cpus: n.alloc_cpus,
            total_memory_bytes: n.total_memory_bytes,
            alloc_memory_bytes: n.alloc_memory_bytes,
            total_gpus: n.total_gpus,
            alloc_gpus: n.alloc_gpus,
            cpu_load: n.cpu_load,
            free_memory_bytes: n.free_memory_bytes,
        })
        .collect();

    NodeMetrics {
        total: snap.total,
        by_state,
        total_cpus: snap.total_cpus,
        alloc_cpus: snap.alloc_cpus,
        total_memory_bytes: snap.total_memory_bytes,
        alloc_memory_bytes: snap.alloc_memory_bytes,
        total_gpus: snap.total_gpus,
        alloc_gpus: snap.alloc_gpus,
        per_node,
    }
}

#[cfg(test)]
mod tests {
    use spur_core::job::{Job, JobSpec, JobState, PendingReason};
    use spur_core::node::{Node, NodeState};
    use spur_core::resource::{GpuLinkType, GpuResource, ResourceAllocations, ResourceSet};
    use spur_metrics::export::jobs::{encode_job_metrics, job_state_metric_suffix};
    use spur_metrics::export::nodes::encode_nodes_metrics;
    use spur_metrics::export::rpc::encode_rpc_metrics;
    use spur_metrics::job::JobMetricsSnapshot;
    use spur_metrics::node::NodeMetricsSnapshot;
    use spur_metrics::{RpcOperationSnapshot, RpcStatsSnapshot};

    use super::*;

    fn gauge_value(body: &str, name: &str) -> u64 {
        let needle = format!("{name} ");
        body.lines()
            .find(|line| line.starts_with(&needle))
            .unwrap_or_else(|| panic!("missing metric line for {name}"))
            .split_whitespace()
            .nth(1)
            .unwrap_or_else(|| panic!("malformed metric line for {name}"))
            .parse()
            .unwrap_or_else(|_| panic!("non-numeric value for {name}"))
    }

    fn sample_job_snapshot() -> JobMetricsSnapshot {
        let jobs = [
            {
                let mut j = Job::new(1, JobSpec::default());
                j.state = JobState::Pending;
                j.pending_reason = PendingReason::Held;
                j
            },
            {
                let mut j = Job::new(2, JobSpec::default());
                j.state = JobState::Pending;
                j
            },
            {
                let mut j = Job::new(3, JobSpec::default());
                j.state = JobState::Running;
                j.allocated_resources = Some(ResourceAllocations::with_scalar(4, 8192));
                j
            },
            {
                let mut j = Job::new(4, JobSpec::default());
                j.state = JobState::OutOfMemory;
                j
            },
        ];
        JobMetricsSnapshot::collect(jobs.iter())
    }

    fn resources(cpus: u32, memory_mb: u64, gpu_count: u32) -> ResourceSet {
        let mut gpus = Vec::new();
        for i in 0..gpu_count {
            gpus.push(GpuResource {
                device_id: i,
                gpu_type: "mi300x".into(),
                memory_mb: 0,
                peer_gpus: vec![],
                link_type: GpuLinkType::XGMI,
            });
        }
        ResourceSet {
            cpus,
            memory_mb,
            gpus,
            generic: Default::default(),
        }
    }

    fn sample_node_snapshot() -> NodeMetricsSnapshot {
        let mut idle = Node::new("node-a".into(), resources(8, 16384, 0));
        idle.state = NodeState::Idle;
        let mut alloc = Node::new("node-b".into(), resources(8, 16384, 2));
        alloc.state = NodeState::Allocated;
        alloc.alloc_resources = ResourceAllocations::with_scalar(4, 4096);
        NodeMetricsSnapshot::collect([&idle, &alloc])
    }

    #[test]
    fn job_proto_matches_http_gauges() {
        let snap = sample_job_snapshot();
        let proto = job_metrics_to_proto(&snap);
        let body = encode_job_metrics(&snap);

        assert_eq!(proto.total, gauge_value(&body, "spur_jobs"));
        assert_eq!(proto.held_pending, 1);
        assert_eq!(
            proto.running_cpus,
            gauge_value(&body, "spur_jobs_cpus_alloc")
        );
        assert_eq!(
            proto.running_memory_bytes,
            gauge_value(&body, "spur_jobs_memory_alloc_bytes")
        );

        for &state in &JobState::ALL {
            let suffix = job_state_metric_suffix(state);
            let count = proto
                .by_state
                .iter()
                .find(|e| JobState::from_proto_i32(e.state) == Some(state))
                .map(|e| e.count)
                .unwrap_or_else(|| panic!("missing proto entry for {state:?}"));
            assert_eq!(
                count,
                gauge_value(&body, &format!("spur_jobs_{suffix}")),
                "mismatch for {state:?}"
            );
        }
    }

    #[test]
    fn node_proto_matches_http_gauges() {
        let snap = sample_node_snapshot();
        let proto = node_metrics_to_proto(&snap);
        let body = encode_nodes_metrics(&snap);

        assert_eq!(proto.total, gauge_value(&body, "spur_nodes"));
        assert_eq!(proto.total_cpus, gauge_value(&body, "spur_nodes_cpus"));
        assert_eq!(
            proto.alloc_cpus,
            gauge_value(&body, "spur_nodes_cpus_alloc")
        );
        assert_eq!(proto.total, 2);
        assert_eq!(proto.per_node.len(), 2);
    }

    #[test]
    fn rpc_proto_matches_http_gauges() {
        let snap = RpcStatsSnapshot {
            by_operation: vec![
                RpcOperationSnapshot {
                    operation: "SubmitJob".into(),
                    count: 2,
                    total_time_us: 3000,
                },
                RpcOperationSnapshot {
                    operation: "GetJobs".into(),
                    count: 5,
                    total_time_us: 250,
                },
            ],
        };
        let proto = rpc_stats_to_proto(&snap);
        let body = encode_rpc_metrics(&snap);

        for op in &proto.by_operation {
            let prefix = format!("spur_rpc_stats{{operation=\"{}\"}}", op.operation);
            let count_line = body
                .lines()
                .find(|line| line.starts_with(&prefix))
                .unwrap_or_else(|| panic!("missing count line for {}", op.operation));
            let count: u64 = count_line
                .split_whitespace()
                .nth(1)
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(count, op.count);

            let avg_prefix = format!("spur_rpc_stats_avg_time{{operation=\"{}\"}}", op.operation);
            let avg_line = body
                .lines()
                .find(|line| line.starts_with(&avg_prefix))
                .unwrap();
            let avg: u64 = avg_line.split_whitespace().nth(1).unwrap().parse().unwrap();
            assert_eq!(avg, op.avg_time_us);

            let total_prefix = format!(
                "spur_rpc_stats_total_time{{operation=\"{}\"}}",
                op.operation
            );
            let total_line = body
                .lines()
                .find(|line| line.starts_with(&total_prefix))
                .unwrap();
            let total: u64 = total_line
                .split_whitespace()
                .nth(1)
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(total, op.total_time_us);
        }
    }
}
