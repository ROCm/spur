// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use spur_core::job::{Job, JobState};
use spur_core::node::{Node, NodeState};

use crate::job::{job_state_index, JOB_STATE_COUNT};
use crate::node::{node_state_index, NODE_STATE_COUNT};

/// Aggregated metrics for a single partition, keyed by name for labeled export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerPartitionMetrics {
    pub name: String,
    pub jobs_total: u64,
    pub jobs_by_state: [u64; JOB_STATE_COUNT],
    pub nodes_total: u64,
    pub nodes_by_state: [u64; NODE_STATE_COUNT],
    pub cpus: u64,
    pub cpus_alloc: u64,
    pub memory_bytes: u64,
    pub memory_alloc_bytes: u64,
}

/// Aggregated partition metrics derived from the controller's job and node maps.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartitionMetricsSnapshot {
    pub total: u64,
    pub per_partition: Vec<PerPartitionMetrics>,
}

fn memory_mb_to_bytes(mb: u64) -> u64 {
    mb.saturating_mul(1024 * 1024)
}

impl PartitionMetricsSnapshot {
    /// Rebuild metrics by scanning jobs and nodes against the named partitions.
    ///
    /// A job belongs to the partition named by `job.spec.partition`; a node
    /// belongs to every partition listed in `node.partitions` (already-resolved
    /// membership, not the raw hostlist pattern in partition config).
    pub fn collect<'a>(
        partition_names: impl IntoIterator<Item = &'a str>,
        jobs: impl IntoIterator<Item = &'a Job> + Clone,
        nodes: impl IntoIterator<Item = &'a Node> + Clone,
    ) -> Self {
        let mut per_partition = Vec::new();

        for name in partition_names {
            let mut m = PerPartitionMetrics {
                name: name.to_string(),
                jobs_total: 0,
                jobs_by_state: [0; JOB_STATE_COUNT],
                nodes_total: 0,
                nodes_by_state: [0; NODE_STATE_COUNT],
                cpus: 0,
                cpus_alloc: 0,
                memory_bytes: 0,
                memory_alloc_bytes: 0,
            };

            for job in jobs.clone() {
                if job.spec.partition.as_deref() != Some(name) {
                    continue;
                }
                m.jobs_total += 1;
                m.jobs_by_state[job_state_index(job.state)] += 1;
            }

            for node in nodes.clone() {
                if !node.partitions.iter().any(|p| p == name) {
                    continue;
                }
                m.nodes_total += 1;
                m.nodes_by_state[node_state_index(node.state)] += 1;
                m.cpus += u64::from(node.total_resources.cpus);
                m.cpus_alloc += u64::from(node.alloc_resources.cpus);
                m.memory_bytes += memory_mb_to_bytes(node.total_resources.memory_mb);
                m.memory_alloc_bytes += memory_mb_to_bytes(node.alloc_resources.memory_mb);
            }

            per_partition.push(m);
        }

        per_partition.sort_by(|a, b| a.name.cmp(&b.name));
        Self {
            total: per_partition.len() as u64,
            per_partition,
        }
    }
}

impl PerPartitionMetrics {
    /// Count for a single job state.
    pub fn count_job_state(&self, state: JobState) -> u64 {
        self.jobs_by_state[job_state_index(state)]
    }

    /// Count for a single node state.
    pub fn count_node_state(&self, state: NodeState) -> u64 {
        self.nodes_by_state[node_state_index(state)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_core::job::JobSpec;
    use spur_core::resource::{ResourceAllocations, ResourceSet};

    fn job_in(id: u32, partition: &str, state: JobState) -> Job {
        let spec = JobSpec {
            partition: Some(partition.into()),
            ..Default::default()
        };
        let mut job = Job::new(id, spec);
        job.state = state;
        job
    }

    fn node_in(name: &str, partitions: &[&str], cpus: u32, memory_mb: u64) -> Node {
        let mut node = Node::new(
            name.into(),
            ResourceSet {
                cpus,
                memory_mb,
                gpus: Vec::new(),
                generic: Default::default(),
            },
        );
        node.partitions = partitions.iter().map(|p| p.to_string()).collect();
        node.alloc_resources = ResourceAllocations::with_scalar(0, 0);
        node
    }

    #[test]
    fn empty_partitions() {
        let snap = PartitionMetricsSnapshot::collect([], [].iter(), [].iter());
        assert_eq!(snap, PartitionMetricsSnapshot::default());
    }

    #[test]
    fn scopes_jobs_and_nodes_by_partition() {
        let jobs = [
            job_in(1, "default", JobState::Pending),
            job_in(2, "default", JobState::Running),
            job_in(3, "gpu", JobState::Running),
        ];
        let nodes = [
            node_in("n1", &["default"], 8, 16384),
            node_in("n2", &["default", "gpu"], 4, 8192),
        ];

        let snap = PartitionMetricsSnapshot::collect(["default", "gpu"], jobs.iter(), nodes.iter());
        assert_eq!(snap.total, 2);

        let default = &snap.per_partition[0];
        assert_eq!(default.name, "default");
        assert_eq!(default.jobs_total, 2);
        assert_eq!(default.count_job_state(JobState::Pending), 1);
        assert_eq!(default.count_job_state(JobState::Running), 1);
        assert_eq!(default.nodes_total, 2);
        assert_eq!(default.cpus, 12);

        let gpu = &snap.per_partition[1];
        assert_eq!(gpu.name, "gpu");
        assert_eq!(gpu.jobs_total, 1);
        assert_eq!(gpu.nodes_total, 1);
        assert_eq!(gpu.cpus, 4);
    }
}
