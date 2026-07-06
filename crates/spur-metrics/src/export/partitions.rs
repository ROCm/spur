// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Partition gauge registration for `/metrics/partitions` (Layer 1d).

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use spur_core::job::JobState;
use spur_core::node::NodeState;
use std::sync::atomic::AtomicU64;

use crate::export::encode_registered;
use crate::export::jobs::job_state_metric_suffix;
use crate::export::register_gauge;
use crate::node::node_state_metric_suffix;
use crate::partition::PartitionMetricsSnapshot;

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct PartitionLabel {
    partition: String,
}

fn family_gauge(
    family: &Family<PartitionLabel, Gauge<u64, AtomicU64>>,
    label: &PartitionLabel,
    value: u64,
) {
    family.get_or_create(label).set(value);
}

/// Register partition gauges into `registry` from `snap`.
pub fn register_partitions(registry: &mut Registry, snap: &PartitionMetricsSnapshot) {
    register_gauge(
        registry,
        "spur_partitions",
        "Total number of partitions",
        snap.total,
    );

    let jobs = Family::<PartitionLabel, Gauge<u64, AtomicU64>>::default();
    let cpus = Family::<PartitionLabel, Gauge<u64, AtomicU64>>::default();
    let cpus_alloc = Family::<PartitionLabel, Gauge<u64, AtomicU64>>::default();
    let nodes = Family::<PartitionLabel, Gauge<u64, AtomicU64>>::default();
    let memory_bytes = Family::<PartitionLabel, Gauge<u64, AtomicU64>>::default();
    let memory_alloc_bytes = Family::<PartitionLabel, Gauge<u64, AtomicU64>>::default();
    let mut job_state_families = Vec::new();
    for &state in &JobState::ALL {
        job_state_families.push((
            state,
            Family::<PartitionLabel, Gauge<u64, AtomicU64>>::default(),
        ));
    }
    let mut node_state_families = Vec::new();
    for &state in &NodeState::ALL {
        node_state_families.push((
            state,
            Family::<PartitionLabel, Gauge<u64, AtomicU64>>::default(),
        ));
    }

    for p in &snap.per_partition {
        let label = PartitionLabel {
            partition: p.name.clone(),
        };
        family_gauge(&jobs, &label, p.jobs_total);
        family_gauge(&cpus, &label, p.cpus);
        family_gauge(&cpus_alloc, &label, p.cpus_alloc);
        family_gauge(&nodes, &label, p.nodes_total);
        family_gauge(&memory_bytes, &label, p.memory_bytes);
        family_gauge(&memory_alloc_bytes, &label, p.memory_alloc_bytes);
        for (state, family) in &job_state_families {
            family_gauge(family, &label, p.count_job_state(*state));
        }
        for (state, family) in &node_state_families {
            family_gauge(family, &label, p.count_node_state(*state));
        }
    }

    registry.register(
        "spur_partition_jobs",
        "Jobs on the specified partition",
        jobs,
    );
    registry.register(
        "spur_partition_cpus",
        "Total CPUs on the specified partition",
        cpus,
    );
    registry.register(
        "spur_partition_cpus_alloc",
        "Allocated CPUs on the specified partition",
        cpus_alloc,
    );
    registry.register(
        "spur_partition_nodes",
        "Nodes on the specified partition",
        nodes,
    );
    registry.register(
        "spur_partition_memory_bytes",
        "Total memory in bytes on the specified partition",
        memory_bytes,
    );
    registry.register(
        "spur_partition_memory_alloc_bytes",
        "Allocated memory in bytes on the specified partition",
        memory_alloc_bytes,
    );
    for (state, family) in job_state_families {
        let name = format!("spur_partition_jobs_{}", job_state_metric_suffix(state));
        let help = format!(
            "Jobs in {} state on the specified partition",
            state.display()
        );
        registry.register(name, help, family);
    }
    for (state, family) in node_state_families {
        let name = format!("spur_partition_nodes_{}", node_state_metric_suffix(state));
        let help = format!(
            "Nodes in {} state on the specified partition",
            state.display()
        );
        registry.register(name, help, family);
    }
}

/// Encode partition metrics for `/metrics/partitions` as OpenMetrics 1.0 text.
pub fn encode_partitions_metrics(snap: &PartitionMetricsSnapshot) -> String {
    encode_registered(|registry| register_partitions(registry, snap))
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_core::job::{Job, JobSpec};
    use spur_core::node::Node;
    use spur_core::resource::ResourceSet;

    #[test]
    fn empty_partitions_export_has_eof_only() {
        let body = encode_partitions_metrics(&PartitionMetricsSnapshot::default());
        assert!(body.contains("spur_partitions 0\n"));
        assert!(body.ends_with("# EOF\n"));
    }

    #[test]
    fn export_contains_per_partition_families() {
        let spec = JobSpec {
            partition: Some("default".into()),
            ..Default::default()
        };
        let mut job = Job::new(1, spec);
        job.state = JobState::Running;

        let mut node = Node::new(
            "n1".into(),
            ResourceSet {
                cpus: 8,
                memory_mb: 16384,
                gpus: Vec::new(),
                generic: Default::default(),
            },
        );
        node.partitions = vec!["default".into()];

        let snap = PartitionMetricsSnapshot::collect(["default"], [&job], [&node]);
        let body = encode_partitions_metrics(&snap);

        assert!(body.contains("spur_partitions 1\n"));
        assert!(body.contains("spur_partition_jobs{partition=\"default\"} 1\n"));
        assert!(body.contains("spur_partition_jobs_running{partition=\"default\"} 1\n"));
        assert!(body.contains("spur_partition_nodes{partition=\"default\"} 1\n"));
        assert!(body.contains("spur_partition_cpus{partition=\"default\"} 8\n"));
    }
}
