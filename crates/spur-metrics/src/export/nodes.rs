// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Node gauge registration for `/metrics/nodes` (Layer 1b).

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use spur_core::config::MetricsExpositionFormat;
use spur_core::node::NodeState;
use std::sync::atomic::AtomicU64;

use crate::export::encode_registered;
use crate::node::{node_state_metric_suffix, NodeMetricsSnapshot};

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct NodeLabel {
    node: String,
}

fn register_gauge(registry: &mut Registry, name: &str, help: &str, value: u64) {
    let gauge = Gauge::<u64, AtomicU64>::default();
    gauge.set(value);
    registry.register(name, help, gauge);
}

fn set_family_gauge(family: &Family<NodeLabel, Gauge<u64, AtomicU64>>, node: &str, value: u64) {
    let gauge = family.get_or_create(&NodeLabel {
        node: node.to_string(),
    });
    gauge.set(value);
}

/// Register node catalog gauges into `registry` from `snap`.
pub fn register_nodes(registry: &mut Registry, snap: &NodeMetricsSnapshot) {
    register_gauge(registry, "spur_nodes", "Total number of nodes", snap.total);

    for &state in &NodeState::ALL {
        let suffix = node_state_metric_suffix(state);
        let name = format!("spur_nodes_{suffix}");
        let help = format!("Number of nodes in {} state", state.display());
        register_gauge(registry, &name, &help, snap.count_state(state));
    }

    register_gauge(
        registry,
        "spur_nodes_cpus",
        "Total CPUs across all nodes",
        snap.total_cpus,
    );
    register_gauge(
        registry,
        "spur_nodes_cpus_alloc",
        "Total CPUs allocated across all nodes",
        snap.alloc_cpus,
    );
    register_gauge(
        registry,
        "spur_nodes_memory_bytes",
        "Total memory in bytes across all nodes",
        snap.total_memory_bytes,
    );
    register_gauge(
        registry,
        "spur_nodes_memory_alloc_bytes",
        "Total memory in bytes allocated across all nodes",
        snap.alloc_memory_bytes,
    );
    register_gauge(
        registry,
        "spur_nodes_gpus",
        "Total GPUs across all nodes",
        snap.total_gpus,
    );
    register_gauge(
        registry,
        "spur_nodes_gpus_alloc",
        "Total GPUs allocated across all nodes",
        snap.alloc_gpus,
    );

    let node_cpus = Family::<NodeLabel, Gauge<u64, AtomicU64>>::default();
    let node_cpus_alloc = Family::<NodeLabel, Gauge<u64, AtomicU64>>::default();
    let node_memory_bytes = Family::<NodeLabel, Gauge<u64, AtomicU64>>::default();
    let node_memory_alloc_bytes = Family::<NodeLabel, Gauge<u64, AtomicU64>>::default();
    let node_gpus = Family::<NodeLabel, Gauge<u64, AtomicU64>>::default();
    let node_gpus_alloc = Family::<NodeLabel, Gauge<u64, AtomicU64>>::default();
    let node_cpu_load = Family::<NodeLabel, Gauge<u64, AtomicU64>>::default();
    let node_free_memory_bytes = Family::<NodeLabel, Gauge<u64, AtomicU64>>::default();

    for node in &snap.per_node {
        set_family_gauge(&node_cpus, &node.name, node.total_cpus);
        set_family_gauge(&node_cpus_alloc, &node.name, node.alloc_cpus);
        set_family_gauge(&node_memory_bytes, &node.name, node.total_memory_bytes);
        set_family_gauge(
            &node_memory_alloc_bytes,
            &node.name,
            node.alloc_memory_bytes,
        );
        set_family_gauge(&node_gpus, &node.name, node.total_gpus);
        set_family_gauge(&node_gpus_alloc, &node.name, node.alloc_gpus);
        set_family_gauge(&node_cpu_load, &node.name, node.cpu_load);
        set_family_gauge(&node_free_memory_bytes, &node.name, node.free_memory_bytes);
    }

    registry.register("spur_node_cpus", "CPUs on the specified node", node_cpus);
    registry.register(
        "spur_node_cpus_alloc",
        "CPUs allocated on the specified node",
        node_cpus_alloc,
    );
    registry.register(
        "spur_node_memory_bytes",
        "Memory in bytes on the specified node",
        node_memory_bytes,
    );
    registry.register(
        "spur_node_memory_alloc_bytes",
        "Memory in bytes allocated on the specified node",
        node_memory_alloc_bytes,
    );
    registry.register("spur_node_gpus", "GPUs on the specified node", node_gpus);
    registry.register(
        "spur_node_gpus_alloc",
        "GPUs allocated on the specified node",
        node_gpus_alloc,
    );
    registry.register(
        "spur_node_cpu_load",
        "CPU load reported by the node agent",
        node_cpu_load,
    );
    registry.register(
        "spur_node_free_memory_bytes",
        "Free memory in bytes reported by the node agent",
        node_free_memory_bytes,
    );
}

/// Encode node metrics for `/metrics/nodes`.
pub fn encode_nodes_metrics_with_format(
    snap: &NodeMetricsSnapshot,
    format: MetricsExpositionFormat,
) -> String {
    encode_registered(|registry| register_nodes(registry, snap), format)
}

/// Encode node metrics for `/metrics/nodes` (default: Slurm 0.0.4 text).
pub fn encode_nodes_metrics(snap: &NodeMetricsSnapshot) -> String {
    encode_nodes_metrics_with_format(snap, MetricsExpositionFormat::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::NodeMetricsSnapshot;
    use spur_core::node::Node;
    use spur_core::resource::{GpuLinkType, GpuResource, ResourceSet};

    #[test]
    fn empty_nodes_export_slurm_exports_zeroes() {
        let body = encode_nodes_metrics_with_format(
            &NodeMetricsSnapshot::default(),
            MetricsExpositionFormat::Slurm_0_0_4,
        );
        assert!(body.contains("spur_nodes 0\n"));
        assert!(body.contains("spur_nodes_idle 0\n"));
        assert!(!body.contains("# EOF"));
    }

    #[test]
    fn empty_nodes_export_openmetrics_exports_zeroes_and_eof() {
        let body = encode_nodes_metrics_with_format(
            &NodeMetricsSnapshot::default(),
            MetricsExpositionFormat::OpenMetrics_1_0,
        );
        assert!(body.contains("spur_nodes 0\n"));
        assert!(body.ends_with("# EOF\n"));
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

    #[test]
    fn export_contains_core_gauges_and_per_node_families() {
        let mut n1 = Node::new("node-a".into(), resources(8, 16384, 2));
        n1.state = NodeState::Idle;
        n1.cpu_load = 12;
        n1.free_memory_mb = 4096;

        let mut n2 = Node::new("node-b".into(), resources(4, 8192, 0));
        n2.state = NodeState::Allocated;
        n2.alloc_resources = resources(2, 4096, 0);

        let snap = NodeMetricsSnapshot::collect([&n1, &n2]);
        let body = encode_nodes_metrics_with_format(&snap, MetricsExpositionFormat::Slurm_0_0_4);

        assert!(body.contains("# HELP spur_nodes "));
        assert!(body.contains("spur_nodes 2\n"));
        assert!(body.contains("spur_nodes_idle 1\n"));
        assert!(body.contains("spur_nodes_alloc 1\n"));
        assert!(body.contains("spur_nodes_cpus 12\n"));
        assert!(body.contains("spur_nodes_cpus_alloc 2\n"));

        assert!(body.contains("spur_node_cpus{node=\"node-a\"} 8\n"));
        assert!(body.contains("spur_node_cpus{node=\"node-b\"} 4\n"));
        assert!(body.contains("spur_node_cpus_alloc{node=\"node-b\"} 2\n"));
        assert!(body.contains("spur_node_cpu_load{node=\"node-a\"} 12\n"));
        assert!(body.contains("spur_node_free_memory_bytes{node=\"node-a\"} 4294967296\n"));
    }
}
