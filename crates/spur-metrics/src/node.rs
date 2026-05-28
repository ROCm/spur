// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use spur_core::node::{Node, NodeState};

/// Number of [`NodeState`] variants (index for `by_state`).
pub const NODE_STATE_COUNT: usize = NodeState::COUNT;

/// Per-node resource and telemetry fields for labeled export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerNodeMetrics {
    pub name: String,
    pub total_cpus: u64,
    pub alloc_cpus: u64,
    pub total_memory_bytes: u64,
    pub alloc_memory_bytes: u64,
    pub total_gpus: u64,
    pub alloc_gpus: u64,
    pub cpu_load: u64,
    pub free_memory_bytes: u64,
}

/// Aggregated node metrics derived from the controller node map.
///
/// Built by scanning in-memory `Node` records (lazy, on read). Durable catalog
/// fields (`state`, `total_resources`, `alloc_resources`) live in the Raft-backed
/// node map on `ClusterManager`. Telemetry fields (`cpu_load`, `free_memory_mb`)
/// are ephemeral until agents report after reconnect.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeMetricsSnapshot {
    /// Total nodes in the controller map.
    pub total: u64,
    /// Count per [`NodeState`]; index via [`node_state_index`].
    pub by_state: [u64; NODE_STATE_COUNT],
    /// Sum of `total_resources.cpus` across all nodes.
    pub total_cpus: u64,
    /// Sum of `alloc_resources.cpus` across all nodes.
    pub alloc_cpus: u64,
    /// Sum of total memory (bytes) across all nodes.
    pub total_memory_bytes: u64,
    /// Sum of allocated memory (bytes) across all nodes.
    pub alloc_memory_bytes: u64,
    /// Sum of total GPUs across all nodes.
    pub total_gpus: u64,
    /// Sum of allocated GPUs across all nodes.
    pub alloc_gpus: u64,
    /// Per-node breakdown, sorted by name for stable encoding.
    pub per_node: Vec<PerNodeMetrics>,
}

/// Map [`NodeState`] to a stable index in [`NodeMetricsSnapshot::by_state`]
/// (proto wire discriminant via [`NodeState::from_proto_i32`]).
pub fn node_state_index(state: NodeState) -> usize {
    let wire = state.to_proto_i32();
    debug_assert_eq!(NodeState::from_proto_i32(wire), Some(state));
    wire as usize
}

/// Metric name suffix for a [`NodeState`] (e.g. `idle`, `alloc`).
pub fn node_state_metric_suffix(state: NodeState) -> &'static str {
    match state {
        NodeState::Idle => "idle",
        NodeState::Allocated => "alloc",
        NodeState::Mixed => "mixed",
        NodeState::Down => "down",
        NodeState::Drain => "drain",
        NodeState::Draining => "draining",
        NodeState::Error => "error",
        NodeState::Unknown => "unknown",
        NodeState::Suspended => "suspended",
    }
}

fn memory_mb_to_bytes(mb: u64) -> u64 {
    mb.saturating_mul(1024 * 1024)
}

impl NodeMetricsSnapshot {
    /// Rebuild metrics by scanning all nodes.
    pub fn collect<'a>(nodes: impl IntoIterator<Item = &'a Node>) -> Self {
        let mut snap = Self::default();
        let mut per_node = Vec::new();

        for node in nodes {
            snap.total += 1;
            snap.by_state[node_state_index(node.state)] += 1;

            let total_cpus = u64::from(node.total_resources.cpus);
            let alloc_cpus = u64::from(node.alloc_resources.cpus);
            let total_memory_bytes = memory_mb_to_bytes(node.total_resources.memory_mb);
            let alloc_memory_bytes = memory_mb_to_bytes(node.alloc_resources.memory_mb);
            let total_gpus = u64::from(node.total_resources.total_gpus());
            let alloc_gpus = u64::from(node.alloc_resources.total_gpus());

            snap.total_cpus += total_cpus;
            snap.alloc_cpus += alloc_cpus;
            snap.total_memory_bytes += total_memory_bytes;
            snap.alloc_memory_bytes += alloc_memory_bytes;
            snap.total_gpus += total_gpus;
            snap.alloc_gpus += alloc_gpus;

            per_node.push(PerNodeMetrics {
                name: node.name.clone(),
                total_cpus,
                alloc_cpus,
                total_memory_bytes,
                alloc_memory_bytes,
                total_gpus,
                alloc_gpus,
                cpu_load: u64::from(node.cpu_load),
                free_memory_bytes: memory_mb_to_bytes(node.free_memory_mb),
            });
        }

        per_node.sort_by(|a, b| a.name.cmp(&b.name));
        snap.per_node = per_node;
        snap
    }

    /// Count for a single state.
    pub fn count_state(&self, state: NodeState) -> u64 {
        self.by_state[node_state_index(state)]
    }
}

#[cfg(test)]
mod tests {
    use spur_core::node::{Node, NodeState};
    use spur_core::resource::{GpuLinkType, GpuResource, ResourceSet};

    use super::*;

    fn node_named(
        name: &str,
        state: NodeState,
        total: ResourceSet,
        alloc: ResourceSet,
        cpu_load: u32,
        free_memory_mb: u64,
    ) -> Node {
        let mut node = Node::new(name.into(), total);
        node.state = state;
        node.alloc_resources = alloc;
        node.cpu_load = cpu_load;
        node.free_memory_mb = free_memory_mb;
        node
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
    fn empty_nodes() {
        let snap = NodeMetricsSnapshot::collect([]);
        assert_eq!(snap, NodeMetricsSnapshot::default());
    }

    #[test]
    fn counts_by_state() {
        let nodes = [
            node_named(
                "b",
                NodeState::Idle,
                resources(8, 16384, 0),
                ResourceSet::default(),
                0,
                0,
            ),
            node_named(
                "a",
                NodeState::Allocated,
                resources(8, 16384, 2),
                resources(8, 8192, 2),
                10,
                4096,
            ),
            node_named(
                "c",
                NodeState::Down,
                resources(4, 8192, 0),
                ResourceSet::default(),
                0,
                0,
            ),
        ];
        let snap = NodeMetricsSnapshot::collect(nodes.iter());
        assert_eq!(snap.total, 3);
        assert_eq!(snap.count_state(NodeState::Idle), 1);
        assert_eq!(snap.count_state(NodeState::Allocated), 1);
        assert_eq!(snap.count_state(NodeState::Down), 1);
        assert_eq!(snap.total_cpus, 8 + 8 + 4);
        assert_eq!(snap.alloc_cpus, 8);
        assert_eq!(
            snap.total_memory_bytes,
            memory_mb_to_bytes(16384 + 16384 + 8192)
        );
        assert_eq!(snap.alloc_memory_bytes, memory_mb_to_bytes(8192));
        assert_eq!(snap.total_gpus, 2);
        assert_eq!(snap.alloc_gpus, 2);
        assert_eq!(snap.per_node.len(), 3);
        assert_eq!(snap.per_node[0].name, "a");
        assert_eq!(snap.per_node[1].name, "b");
        assert_eq!(snap.per_node[2].name, "c");
        assert_eq!(snap.per_node[0].cpu_load, 10);
        assert_eq!(snap.per_node[0].free_memory_bytes, memory_mb_to_bytes(4096));
    }

    #[test]
    fn node_state_index_uses_proto_wire() {
        for &state in &NodeState::ALL {
            let wire = state.to_proto_i32();
            assert_eq!(NodeState::from_proto_i32(wire), Some(state));
            assert_eq!(node_state_index(state), wire as usize);
        }
    }

    #[test]
    fn node_state_metric_suffixes_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for &state in &NodeState::ALL {
            let suffix = node_state_metric_suffix(state);
            assert!(seen.insert(suffix), "duplicate suffix for {state:?}");
        }
    }
}
