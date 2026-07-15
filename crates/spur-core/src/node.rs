// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::resource::{ResourceAllocations, ResourceSet};

/// Node states matching Slurm's model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum NodeState {
    Idle,
    Allocated,
    Mixed,
    Down,
    Drain,
    Draining,
    Error,
    Unknown,
    Suspended,
}

/// Events that drive node state transitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeEvent {
    /// First-time registration via WAL.
    Register,
    /// No heartbeat received within the health-check threshold.
    HeartbeatTimeout,
    /// Heartbeat resumed on a previously-timed-out node.
    HeartbeatRecovered,
    /// Admin or API explicitly requests a target state.
    AdminSetState(NodeState),
    /// Power management suspended the node.
    PowerSuspend,
    /// Power management resumed the node.
    PowerResume,
}

impl NodeState {
    /// Centralized transition table. Returns the new state if the transition
    /// is valid, `None` if the current state should be preserved.
    ///
    /// When `admin_locked` is true, auto-recovery (HeartbeatRecovered) is
    /// suppressed — only an explicit admin action can clear the state.
    pub fn transition(&self, event: &NodeEvent, admin_locked: bool) -> Option<NodeState> {
        match (self, event) {
            // --- Registration ---
            (NodeState::Unknown, NodeEvent::Register) => Some(NodeState::Idle),
            (_, NodeEvent::Register) => None,

            // --- Heartbeat liveness (symmetric pair) ---
            (NodeState::Down | NodeState::Drain, NodeEvent::HeartbeatTimeout) => None,
            (_, NodeEvent::HeartbeatTimeout) => Some(NodeState::Down),

            (NodeState::Down | NodeState::Error, NodeEvent::HeartbeatRecovered)
                if !admin_locked =>
            {
                Some(NodeState::Idle)
            }
            (_, NodeEvent::HeartbeatRecovered) => None,

            // --- Power management ---
            (_, NodeEvent::PowerSuspend) => Some(NodeState::Suspended),
            (NodeState::Suspended, NodeEvent::PowerResume) => Some(NodeState::Idle),
            (_, NodeEvent::PowerResume) => None,

            // --- Admin / API ---
            (_, NodeEvent::AdminSetState(target)) => Some(*target),
        }
    }

    /// Whether this is an operator-managed hold state that allocation-driven
    /// transitions (Idle/Mixed/Allocated) must not override.
    pub fn is_admin_hold(&self) -> bool {
        matches!(
            self,
            Self::Down | Self::Drain | Self::Draining | Self::Error | Self::Suspended
        )
    }

    pub fn display(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Allocated => "allocated",
            Self::Mixed => "mixed",
            Self::Down => "down",
            Self::Drain => "drained",
            Self::Draining => "draining",
            Self::Error => "error",
            Self::Unknown => "unknown",
            Self::Suspended => "suspended",
        }
    }

    /// Uppercase display for scontrol output (Slurm convention).
    pub fn display_upper(&self) -> &'static str {
        match self {
            Self::Idle => "IDLE",
            Self::Allocated => "ALLOCATED",
            Self::Mixed => "MIXED",
            Self::Down => "DOWN",
            Self::Drain => "DRAINED",
            Self::Draining => "DRAINING",
            Self::Error => "ERROR",
            Self::Unknown => "UNKNOWN",
            Self::Suspended => "SUSPENDED",
        }
    }

    /// Short suffix used in sinfo (e.g., "idle", "alloc", "mix").
    pub fn short(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Allocated => "alloc",
            Self::Mixed => "mix",
            Self::Down => "down",
            Self::Drain => "drain",
            Self::Draining => "drng",
            Self::Error => "err",
            Self::Unknown => "unk",
            Self::Suspended => "susp",
        }
    }

    pub fn is_available(&self) -> bool {
        matches!(self, Self::Idle | Self::Mixed)
    }

    /// Operationally up: `Idle`/`Mixed`/`Allocated`. Broader than [`is_available`]
    /// because a fully-busy `Allocated` node is still up (a `Resources` wait, not
    /// `NodeDown`); admin/system-down states are not up.
    pub fn is_up(&self) -> bool {
        matches!(self, Self::Idle | Self::Mixed | Self::Allocated)
    }

    /// Every core variant, in proto discriminant order for iteration only.
    /// Wire conversion uses [`from_proto`](Self::from_proto) / [`to_proto`](Self::to_proto), not array index.
    pub const ALL: [NodeState; 9] = [
        Self::Idle,
        Self::Allocated,
        Self::Mixed,
        Self::Down,
        Self::Drain,
        Self::Draining,
        Self::Error,
        Self::Unknown,
        Self::Suspended,
    ];

    pub const COUNT: usize = Self::ALL.len();

    /// Convert a prost `NodeState` enum to core.
    pub fn from_proto(p: spur_proto::proto::NodeState) -> Self {
        match p {
            spur_proto::proto::NodeState::NodeIdle => Self::Idle,
            spur_proto::proto::NodeState::NodeAllocated => Self::Allocated,
            spur_proto::proto::NodeState::NodeMixed => Self::Mixed,
            spur_proto::proto::NodeState::NodeDown => Self::Down,
            spur_proto::proto::NodeState::NodeDrain => Self::Drain,
            spur_proto::proto::NodeState::NodeDraining => Self::Draining,
            spur_proto::proto::NodeState::NodeError => Self::Error,
            spur_proto::proto::NodeState::NodeUnknown => Self::Unknown,
            spur_proto::proto::NodeState::NodeSuspended => Self::Suspended,
        }
    }

    /// Convert core state to prost `NodeState`.
    pub fn to_proto(self) -> spur_proto::proto::NodeState {
        match self {
            Self::Idle => spur_proto::proto::NodeState::NodeIdle,
            Self::Allocated => spur_proto::proto::NodeState::NodeAllocated,
            Self::Mixed => spur_proto::proto::NodeState::NodeMixed,
            Self::Down => spur_proto::proto::NodeState::NodeDown,
            Self::Drain => spur_proto::proto::NodeState::NodeDrain,
            Self::Draining => spur_proto::proto::NodeState::NodeDraining,
            Self::Error => spur_proto::proto::NodeState::NodeError,
            Self::Unknown => spur_proto::proto::NodeState::NodeUnknown,
            Self::Suspended => spur_proto::proto::NodeState::NodeSuspended,
        }
    }

    /// Convert a proto wire discriminant to core.
    pub fn from_proto_i32(v: i32) -> Option<Self> {
        spur_proto::proto::NodeState::try_from(v)
            .ok()
            .map(Self::from_proto)
    }

    /// Core state as proto wire discriminant.
    pub fn to_proto_i32(self) -> i32 {
        self.to_proto() as i32
    }
}

impl std::fmt::Display for NodeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.display())
    }
}

/// Where a node originates from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum NodeSource {
    /// Traditional native-host node running spurd.
    #[default]
    NativeHost,
    /// Kubernetes node managed by the spur-k8s operator.
    Kubernetes { namespace: String },
}

/// A compute node in the cluster.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub name: String,
    pub state: NodeState,
    pub state_reason: Option<String>,
    /// When true, the current state was set by an operator (admin API, drain,
    /// etc.) and auto-recovery is suppressed. Only an explicit admin action
    /// can clear it. Automatically-set states (heartbeat timeout) leave this
    /// false so the node can self-heal when the agent reconnects.
    #[serde(default)]
    pub admin_locked: bool,
    pub partitions: Vec<String>,
    /// Where this node comes from (native-host or K8s).
    #[serde(default)]
    pub source: NodeSource,

    pub total_resources: ResourceSet,
    pub alloc_resources: ResourceAllocations,

    /// Node feature tags (e.g., "gpu", "nvme", "rack1") for --constraint matching.
    #[serde(default)]
    pub features: Vec<String>,

    /// Key-value labels for partition routing and policy application.
    #[serde(default)]
    pub labels: HashMap<String, String>,

    pub arch: String,
    pub os: String,
    pub cpu_load: u32,
    pub free_memory_mb: u64,

    pub boot_time: Option<DateTime<Utc>>,
    pub last_busy: Option<DateTime<Utc>>,
    pub agent_start_time: Option<DateTime<Utc>>,
    pub last_heartbeat: Option<DateTime<Utc>>,

    /// Agent address for gRPC communication.
    pub address: Option<String>,
    /// Agent gRPC listen port.
    pub port: u16,
    /// WireGuard public key (for mesh setup).
    pub wg_pubkey: Option<String>,
    /// Agent version.
    pub version: Option<String>,
    /// Scheduling weight. Higher weight = preferred for scheduling.
    #[serde(default = "default_weight")]
    pub weight: u32,
    /// Leaf switch this node belongs to (from topology config).
    #[serde(default)]
    pub switch_name: Option<String>,
}

fn default_weight() -> u32 {
    Node::DEFAULT_WEIGHT
}

impl Node {
    /// Default scheduling weight for a node with no matching `NodeConfig`.
    pub const DEFAULT_WEIGHT: u32 = 1;

    pub fn new(name: String, resources: ResourceSet) -> Self {
        Self {
            name,
            state: NodeState::Unknown,
            state_reason: None,
            admin_locked: false,
            partitions: Vec::new(),
            source: NodeSource::default(),
            total_resources: resources,
            alloc_resources: ResourceAllocations::default(),
            features: Vec::new(),
            labels: HashMap::new(),
            arch: String::new(),
            os: String::new(),
            cpu_load: 0,
            free_memory_mb: 0,
            boot_time: None,
            last_busy: None,
            agent_start_time: None,
            last_heartbeat: None,
            address: None,
            port: 6818,
            wg_pubkey: None,
            version: None,
            weight: Self::DEFAULT_WEIGHT,
            switch_name: None,
        }
    }

    /// Reset config-derived scheduling policy (features, weight) to defaults.
    /// Used when a node no longer matches any `NodeConfig` so stale policy does
    /// not persist. Keeps the "no match" state identical to a freshly created node.
    pub fn reset_config_policy(&mut self) {
        self.features.clear();
        self.weight = Self::DEFAULT_WEIGHT;
    }

    /// Whether available inventory can satisfy a count-based request.
    pub fn can_satisfy_request(&self, request: &ResourceSet) -> bool {
        self.total_resources
            .can_satisfy_with_allocated(&self.alloc_resources, request)
    }

    /// Whether this node can accept new work.
    pub fn is_schedulable(&self) -> bool {
        self.state.is_available()
    }

    /// Update state based on allocation level.
    pub fn update_state_from_alloc(&mut self) {
        if self.state.is_admin_hold() {
            return;
        }

        if self.alloc_resources.cpus == 0 && !self.alloc_resources.has_devices() {
            self.state = NodeState::Idle;
        } else if self.alloc_resources.cpus >= self.total_resources.cpus {
            self.state = NodeState::Allocated;
        } else {
            self.state = NodeState::Mixed;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_from_unknown_yields_idle() {
        assert_eq!(
            NodeState::Unknown.transition(&NodeEvent::Register, false),
            Some(NodeState::Idle),
        );
    }

    #[test]
    fn register_from_non_unknown_is_noop() {
        for &s in NodeState::ALL.iter().filter(|s| **s != NodeState::Unknown) {
            assert_eq!(
                s.transition(&NodeEvent::Register, false),
                None,
                "from {s:?}"
            );
        }
    }

    #[test]
    fn heartbeat_recovered_auto_downed() {
        assert_eq!(
            NodeState::Down.transition(&NodeEvent::HeartbeatRecovered, false),
            Some(NodeState::Idle),
        );
        assert_eq!(
            NodeState::Error.transition(&NodeEvent::HeartbeatRecovered, false),
            Some(NodeState::Idle),
        );
    }

    #[test]
    fn heartbeat_recovered_blocked_by_admin_lock() {
        assert_eq!(
            NodeState::Down.transition(&NodeEvent::HeartbeatRecovered, true),
            None,
        );
        assert_eq!(
            NodeState::Error.transition(&NodeEvent::HeartbeatRecovered, true),
            None,
        );
    }

    #[test]
    fn heartbeat_recovered_noop_for_live_and_admin_states() {
        let preserved = [
            NodeState::Idle,
            NodeState::Allocated,
            NodeState::Mixed,
            NodeState::Drain,
            NodeState::Draining,
            NodeState::Suspended,
            NodeState::Unknown,
        ];
        for &s in &preserved {
            assert_eq!(
                s.transition(&NodeEvent::HeartbeatRecovered, false),
                None,
                "from {s:?}"
            );
            assert_eq!(
                s.transition(&NodeEvent::HeartbeatRecovered, true),
                None,
                "from {s:?} (locked)"
            );
        }
    }

    #[test]
    fn heartbeat_timeout_marks_down() {
        let should_go_down = [
            NodeState::Idle,
            NodeState::Allocated,
            NodeState::Mixed,
            NodeState::Draining,
            NodeState::Error,
            NodeState::Unknown,
            NodeState::Suspended,
        ];
        for &s in &should_go_down {
            assert_eq!(
                s.transition(&NodeEvent::HeartbeatTimeout, false),
                Some(NodeState::Down),
                "from {s:?}",
            );
        }
    }

    #[test]
    fn heartbeat_timeout_noop_for_down_and_drain() {
        assert_eq!(
            NodeState::Down.transition(&NodeEvent::HeartbeatTimeout, false),
            None
        );
        assert_eq!(
            NodeState::Drain.transition(&NodeEvent::HeartbeatTimeout, false),
            None
        );
    }

    #[test]
    fn admin_can_force_any_state() {
        for &from in &NodeState::ALL {
            for &to in &NodeState::ALL {
                assert_eq!(
                    from.transition(&NodeEvent::AdminSetState(to), false),
                    Some(to),
                    "admin {from:?} -> {to:?}",
                );
            }
        }
    }

    #[test]
    fn power_suspend_from_any_state() {
        for &s in &NodeState::ALL {
            assert_eq!(
                s.transition(&NodeEvent::PowerSuspend, false),
                Some(NodeState::Suspended),
                "from {s:?}",
            );
        }
    }

    #[test]
    fn power_resume_only_from_suspended() {
        assert_eq!(
            NodeState::Suspended.transition(&NodeEvent::PowerResume, false),
            Some(NodeState::Idle),
        );
        for &s in NodeState::ALL
            .iter()
            .filter(|s| **s != NodeState::Suspended)
        {
            assert_eq!(
                s.transition(&NodeEvent::PowerResume, false),
                None,
                "from {s:?}"
            );
        }
    }

    #[test]
    fn admin_hold_states() {
        let holds = [
            NodeState::Down,
            NodeState::Drain,
            NodeState::Draining,
            NodeState::Error,
            NodeState::Suspended,
        ];
        let non_holds = [
            NodeState::Idle,
            NodeState::Allocated,
            NodeState::Mixed,
            NodeState::Unknown,
        ];
        for &s in &holds {
            assert!(s.is_admin_hold(), "{s:?} should be admin hold");
        }
        for &s in &non_holds {
            assert!(!s.is_admin_hold(), "{s:?} should not be admin hold");
        }
    }

    #[test]
    fn all_is_complete_and_ordered() {
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        assert_eq!(NodeState::ALL.len(), NodeState::COUNT);
        for state in &NodeState::ALL {
            assert!(seen.insert(state), "duplicate variant in ALL: {state}");
        }
    }

    #[test]
    fn node_state_proto_discriminants_match_core() {
        use spur_proto::proto::NodeState as P;

        const TABLE: &[(P, NodeState)] = &[
            (P::NodeIdle, NodeState::Idle),
            (P::NodeAllocated, NodeState::Allocated),
            (P::NodeMixed, NodeState::Mixed),
            (P::NodeDown, NodeState::Down),
            (P::NodeDrain, NodeState::Drain),
            (P::NodeDraining, NodeState::Draining),
            (P::NodeError, NodeState::Error),
            (P::NodeUnknown, NodeState::Unknown),
            (P::NodeSuspended, NodeState::Suspended),
        ];

        assert_eq!(TABLE.len(), NodeState::COUNT);
        for &(proto, core) in TABLE {
            let wire = proto as i32;
            assert_eq!(P::try_from(wire).ok(), Some(proto));
            assert_eq!(NodeState::from_proto_i32(wire), Some(core));
            assert_eq!(
                NodeState::ALL.iter().position(|&s| s == core),
                Some(wire as usize),
                "ALL position for {core:?}"
            );
        }
    }

    #[test]
    fn node_state_proto_try_from_unknown_wire_values() {
        use spur_proto::proto::NodeState as P;

        for bad in [-1, NodeState::COUNT as i32, 99, i32::MAX] {
            assert_eq!(NodeState::from_proto_i32(bad), None);
            assert!(P::try_from(bad).is_err());
        }
    }

    #[test]
    fn node_state_core_proto_roundtrip() {
        for &core in &NodeState::ALL {
            assert_eq!(NodeState::from_proto_i32(core.to_proto_i32()), Some(core));
            assert_eq!(NodeState::from_proto(core.to_proto()), core);
        }
    }
}
