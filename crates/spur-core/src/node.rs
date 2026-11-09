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

    /// The unavailable states `sinfo -R` reports: Down, Drain, Draining, Error.
    /// Unlike [`is_admin_hold`](Self::is_admin_hold), this is purely about state,
    /// not who set it or whether a reason exists, and excludes `Suspended`
    /// (power management, not a fault).
    pub fn is_unavailable(&self) -> bool {
        matches!(
            self,
            Self::Down | Self::Drain | Self::Draining | Self::Error
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

    /// Parse from a short sinfo suffix ("idle", "alloc", "mix") or full name
    /// ("idle", "allocated", "mixed"). Case-insensitive.
    pub fn from_short_or_name(s: &str) -> Option<Self> {
        let lower = s.to_lowercase();
        Self::ALL
            .iter()
            .find(|st| st.short() == lower || st.display() == lower)
            .copied()
    }
}

impl std::fmt::Display for NodeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.display())
    }
}

/// A display-only condition layered on top of an Idle node — CLI-facing
/// (sinfo/scontrol), not part of the persisted [`NodeState`] machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeOverlay {
    /// Held by an active reservation; `maint` also carries the MAINT flag.
    Reserved { maint: bool },
    /// Idle now, but earmarked by backfill for a future job's start.
    Planned,
}

/// Resolve the overlay for a node, matching Slurm's own precedence: an
/// active reservation wins over a backfill plan, and both only apply while
/// the node is otherwise Idle.
pub fn node_overlay(node: &spur_proto::proto::NodeInfo) -> Option<NodeOverlay> {
    if node.state != spur_proto::proto::NodeState::NodeIdle as i32 {
        return None;
    }
    // An older controller sends only the name.
    if node.in_reservation || !node.active_reservation.is_empty() {
        return Some(NodeOverlay::Reserved {
            maint: node.reservation_maint,
        });
    }
    if node.planned_job_id != 0 {
        return Some(NodeOverlay::Planned);
    }
    None
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

/// `RegisterAgentRequest.version` sent by the spur-k8s operator.
pub const K8S_OPERATOR_VERSION: &str = "spur-k8s-operator";

/// Agent capability: the agent implements the `SignalJob` RPC.
pub const CAP_SIGNAL_JOB: &str = "signal-job";

/// Node label carrying the operator's Kubernetes namespace.
pub const K8S_NAMESPACE_LABEL: &str = "spur.amd.com/k8s-namespace";

/// Derive node source from agent registration metadata.
pub fn node_source_from_registration(
    version: &str,
    labels: &HashMap<String, String>,
) -> NodeSource {
    if version == K8S_OPERATOR_VERSION {
        NodeSource::Kubernetes {
            namespace: labels
                .get(K8S_NAMESPACE_LABEL)
                .cloned()
                .filter(|ns| !ns.is_empty())
                .unwrap_or_else(|| "default".to_string()),
        }
    } else {
        NodeSource::NativeHost
    }
}

/// Apply WAL-stored source on replay; re-derive from registration metadata when the
/// persisted value is the default (pre-source WAL entries and native agents).
pub fn resolve_wal_node_source(
    source: &NodeSource,
    version: &str,
    labels: &HashMap<String, String>,
) -> NodeSource {
    if matches!(source, NodeSource::NativeHost) {
        node_source_from_registration(version, labels)
    } else {
        source.clone()
    }
}

/// A compute node in the cluster.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    /// Cluster node name (NodeName): used by `-w`, partitions, and display.
    pub name: String,
    /// OS hostname reported at agent registration (NodeHostname).
    #[serde(default)]
    pub hostname: String,
    pub state: NodeState,
    pub state_reason: Option<String>,
    /// UID that set the current `state_reason` (Slurm's `reason_uid`). `None`
    /// when no reason is set or the setter was unauthenticated.
    #[serde(default)]
    pub reason_uid: Option<u32>,
    /// When the current `state_reason` was set (Slurm's `reason_time`).
    /// Captured at emit time so Raft replay is deterministic.
    #[serde(default)]
    pub reason_time: Option<DateTime<Utc>>,
    /// An administrative scheduling hold. Explicit ReturnToService=2 can clear
    /// plain DOWN after a valid registration; a retained drain still requires
    /// an administrative resume.
    #[serde(default)]
    pub admin_locked: bool,
    pub partitions: Vec<String>,
    /// Where this node comes from (native-host or K8s).
    #[serde(default)]
    pub source: NodeSource,

    pub total_resources: ResourceSet,
    /// Raw resources the agent last reported, before configured caps/headroom are
    /// applied. `total_resources` is the post-clamp schedulable inventory; this
    /// retains the detected truth for observability. Defaults to an empty set for
    /// Raft entries persisted before this field existed.
    #[serde(default)]
    pub detected_resources: ResourceSet,
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
    /// Last response before DOWN, retained for the ReturnToService reboot check.
    #[serde(default)]
    pub last_response_before_down: Option<DateTime<Utc>>,
    /// Unknown on old snapshots: an administrative DOWN may have retained a drain.
    #[serde(default)]
    pub down_from_drain: Option<bool>,
    #[serde(default)]
    pub down_since: Option<DateTime<Utc>>,
    pub last_busy: Option<DateTime<Utc>>,
    pub agent_start_time: Option<DateTime<Utc>>,
    pub last_heartbeat: Option<DateTime<Utc>>,

    /// Routable comm address for agent gRPC and inter-node TCP (NodeAddr).
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
    /// Native k0s: role assigned to this node's spurd-owned unit.
    #[serde(default)]
    pub k0s_role: Option<crate::k0s::K0sRole>,
    /// mesh IP allocated to this node for k0s (--node-ip / advertise address).
    #[serde(default)]
    pub k0s_mesh_ip: Option<String>,
    /// per-node pod /24 carved from the cluster pod_cidr.
    #[serde(default)]
    pub k0s_pod_cidr: Option<String>,
    /// Why this node blocked convergence when the cluster went `degraded` (surfaced in status).
    #[serde(default)]
    pub k0s_last_error: Option<String>,

    /// The node's core layout, derived from its topology and `NodeConfig`.
    /// Never persisted.
    #[serde(skip)]
    pub core_layout: Option<crate::core_layout::CoreLayout>,
    /// Set when the node's core settings cannot yield a valid layout (spec cores
    /// exceed the node, topology missing, etc.). Makes the node unschedulable
    /// with a `core-layout:` reason. Derived state, never persisted.
    #[serde(skip)]
    pub layout_error: Option<String>,
    /// Memory (MB) reserved for node services, shown as `MemSpecLimit`. Set from
    /// the matching config's `reserved_memory_mb` even for memory-only nodes
    /// that have no CPU layout. Derived state, never persisted.
    #[serde(skip)]
    pub spec_memory_mb: u64,
    /// Advanced whenever `core_layout` changes content, a clear included; the
    /// per-node part of the delivery revision. Derived state, never persisted.
    #[serde(skip)]
    pub layout_seq: u64,
    /// Why the agent reports it is not enforcing the layout it holds. Derived
    /// from heartbeats, never persisted.
    #[serde(skip)]
    pub layout_enforce_error: Option<String>,
    /// Optional RPCs the node's agent advertised at registration (e.g. `CAP_SIGNAL_JOB`).
    #[serde(default)]
    pub capabilities: Vec<String>,
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
            hostname: String::new(),
            state: NodeState::Unknown,
            state_reason: None,
            reason_uid: None,
            reason_time: None,
            admin_locked: false,
            partitions: Vec::new(),
            source: NodeSource::default(),
            detected_resources: resources.clone(),
            total_resources: resources,
            alloc_resources: ResourceAllocations::default(),
            features: Vec::new(),
            labels: HashMap::new(),
            arch: String::new(),
            os: String::new(),
            cpu_load: 0,
            free_memory_mb: 0,
            boot_time: None,
            last_response_before_down: None,
            down_from_drain: None,
            down_since: None,
            last_busy: None,
            agent_start_time: None,
            last_heartbeat: None,
            address: None,
            port: 6818,
            wg_pubkey: None,
            version: None,
            weight: Self::DEFAULT_WEIGHT,
            switch_name: None,
            k0s_role: None,
            k0s_mesh_ip: None,
            k0s_pod_cidr: None,
            k0s_last_error: None,
            core_layout: None,
            layout_error: None,
            spec_memory_mb: 0,
            layout_seq: 0,
            layout_enforce_error: None,
            capabilities: Vec::new(),
        }
    }

    /// Whether the node's agent advertised `capability` at registration.
    pub fn supports(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|c| c == capability)
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

    /// Whether the node has any unallocated CPU headroom (a saturated node is full).
    pub fn has_free_cpu_capacity(&self) -> bool {
        !(self.alloc_resources.cpus >= self.total_resources.cpus && self.total_resources.cpus > 0)
    }

    /// Whether this node can accept new work. A node whose core settings cannot
    /// yield a valid layout is excluded until the settings are fixed, so jobs
    /// never land on a node that cannot enforce its spec cores.
    pub fn is_schedulable(&self) -> bool {
        self.state.is_available() && self.layout_block_reason().is_none()
    }

    /// CPUs counted before spec cores are set aside (Slurm `CPUTot`).
    pub fn cpu_tot(&self) -> u32 {
        let spec = self
            .core_layout
            .as_ref()
            .map_or(0, |l| l.spec_cpus.len() as u32);
        self.total_resources.cpus + spec
    }

    /// Why the node's core layout keeps it from running jobs, if it does.
    pub fn layout_block_reason(&self) -> Option<String> {
        if let Some(e) = &self.layout_error {
            return Some(format!("core-layout: {e}"));
        }
        self.layout_enforce_error
            .as_ref()
            .map(|e| format!("core-layout: not enforced: {e}"))
    }

    /// The reason to show when an otherwise available node is blocked by its
    /// core layout, so it reads as DRAIN. Never masks an admin state or reason.
    pub fn layout_drain_reason(&self) -> Option<String> {
        self.layout_block_reason()
            .filter(|_| self.state.is_available())
    }

    /// Routable comm address (NodeAddr), when registered.
    pub fn comm_addr(&self) -> Option<&str> {
        self.address.as_deref()
    }

    /// True once `spur k8s up` has claimed this node for the managed k0s cluster.
    /// Such nodes are owned by the k8s scheduler and must not also take Spur jobs.
    pub fn is_k0s_reserved(&self) -> bool {
        self.k0s_role.is_some()
    }

    /// Update state based on allocation level.
    pub fn update_state_from_alloc(&mut self) {
        if self.state == NodeState::Draining {
            if self.alloc_resources.cpus == 0 && !self.alloc_resources.has_devices() {
                self.state = NodeState::Drain;
            }
            return;
        }

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
    fn is_k0s_reserved_tracks_role_assignment() {
        let mut n = Node::new("n1".into(), ResourceSet::default());
        assert!(!n.is_k0s_reserved());
        n.k0s_role = Some(crate::k0s::K0sRole::Worker);
        assert!(n.is_k0s_reserved());
        n.k0s_role = None;
        assert!(!n.is_k0s_reserved());
    }

    #[test]
    fn a_node_snapshot_without_k0s_last_error_still_deserializes() {
        // Pre-upgrade snapshots carry no k0s_last_error; replay must load it as absent
        // rather than failing the whole restore.
        let node = Node::new("n1".into(), ResourceSet::default());
        let mut value = serde_json::to_value(&node).expect("serialize node");
        value.as_object_mut().unwrap().remove("k0s_last_error");
        let back: Node = serde_json::from_value(value).expect("deserialize node");
        assert_eq!(back.k0s_last_error, None);
    }

    // Frozen pre-`capabilities` node as stored in a Raft snapshot; it must still
    // load or a new controller cannot restore an old snapshot. Never regenerate.
    #[test]
    fn a_node_snapshot_without_capabilities_still_deserializes() {
        const NODE_PRE_CAPABILITIES: &str = r#"{"name":"n1","hostname":"host1","state":"IDLE","state_reason":null,"reason_uid":null,"reason_time":null,"admin_locked":false,"partitions":["gpu"],"source":{"type":"NativeHost"},"total_resources":{"cpus":8,"memory_mb":16000,"gpus":[],"generic":{},"generation":0,"fabric_binding_version":0,"block_file_revision":null},"detected_resources":{"cpus":8,"memory_mb":16000,"gpus":[],"generic":{},"generation":0,"fabric_binding_version":0,"block_file_revision":null},"alloc_resources":{"cpus":0,"memory_mb":0,"devices":{},"generation":0,"fabric_binding":null,"gpu_binding":null},"features":[],"labels":{},"arch":"x86_64","os":"linux","cpu_load":0,"free_memory_mb":16000,"boot_time":null,"last_busy":null,"agent_start_time":null,"last_heartbeat":null,"address":"10.0.0.1","port":6818,"wg_pubkey":null,"version":null,"weight":1,"switch_name":null,"k0s_role":null,"k0s_mesh_ip":null,"k0s_pod_cidr":null,"k0s_last_error":null}"#;
        let back: Node = serde_json::from_str(NODE_PRE_CAPABILITIES)
            .expect("a pre-capabilities node must load; a new field needs #[serde(default)]");
        assert_eq!(back.name, "n1");
        assert_eq!(back.total_resources.cpus, 8);
        assert!(back.capabilities.is_empty());
        assert_eq!(back.down_from_drain, None);
        assert_eq!(back.last_response_before_down, None);
        assert_eq!(back.down_since, None);
        assert!(!back.supports(CAP_SIGNAL_JOB));
    }

    #[test]
    fn supports_matches_only_advertised_capabilities() {
        let mut node = Node::new("n1".into(), ResourceSet::default());
        assert!(!node.supports(CAP_SIGNAL_JOB));
        node.capabilities = vec![CAP_SIGNAL_JOB.to_string()];
        assert!(node.supports(CAP_SIGNAL_JOB));
        assert!(!node.supports("other"));
    }

    #[test]
    fn a_node_snapshot_without_reason_attribution_still_deserializes() {
        // Pre-upgrade snapshots predate reason_uid/reason_time; replay must load
        // them as absent rather than failing the restore.
        let node = Node::new("n1".into(), ResourceSet::default());
        let mut value = serde_json::to_value(&node).expect("serialize node");
        let obj = value.as_object_mut().unwrap();
        obj.remove("reason_uid");
        obj.remove("reason_time");
        let back: Node = serde_json::from_value(value).expect("deserialize node");
        assert_eq!(back.reason_uid, None);
        assert_eq!(back.reason_time, None);
    }

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
    fn unavailable_states() {
        let unavailable = [
            NodeState::Down,
            NodeState::Drain,
            NodeState::Draining,
            NodeState::Error,
        ];
        let available = [
            NodeState::Idle,
            NodeState::Allocated,
            NodeState::Mixed,
            NodeState::Suspended,
            NodeState::Unknown,
        ];
        for &s in &unavailable {
            assert!(s.is_unavailable(), "{s:?} should be unavailable");
        }
        for &s in &available {
            assert!(!s.is_unavailable(), "{s:?} should not be unavailable");
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

    #[test]
    fn node_state_from_short_or_name_roundtrip() {
        for &state in &NodeState::ALL {
            assert_eq!(NodeState::from_short_or_name(state.short()), Some(state));
            assert_eq!(NodeState::from_short_or_name(state.display()), Some(state));
            assert_eq!(
                NodeState::from_short_or_name(&state.short().to_uppercase()),
                Some(state)
            );
        }
    }

    #[test]
    fn node_state_from_short_or_name_rejects_unknown() {
        assert_eq!(NodeState::from_short_or_name("bogus"), None);
        assert_eq!(NodeState::from_short_or_name(""), None);
    }

    fn idle_node_info() -> spur_proto::proto::NodeInfo {
        spur_proto::proto::NodeInfo {
            state: spur_proto::proto::NodeState::NodeIdle as i32,
            ..Default::default()
        }
    }

    #[test]
    fn node_overlay_none_when_plain_idle() {
        assert_eq!(node_overlay(&idle_node_info()), None);
    }

    #[test]
    fn node_overlay_none_when_not_idle_even_with_reservation() {
        let mut node = idle_node_info();
        node.state = spur_proto::proto::NodeState::NodeAllocated as i32;
        node.active_reservation = "r1".into();
        assert_eq!(node_overlay(&node), None);
    }

    #[test]
    fn node_overlay_reserved_without_maint() {
        let mut node = idle_node_info();
        node.active_reservation = "r1".into();
        assert_eq!(
            node_overlay(&node),
            Some(NodeOverlay::Reserved { maint: false })
        );
    }

    #[test]
    fn node_overlay_reserved_when_the_name_is_hidden() {
        let mut node = idle_node_info();
        node.in_reservation = true;
        node.reservation_maint = true;
        assert_eq!(
            node_overlay(&node),
            Some(NodeOverlay::Reserved { maint: true })
        );
    }

    #[test]
    fn node_overlay_reserved_with_maint() {
        let mut node = idle_node_info();
        node.active_reservation = "r1".into();
        node.reservation_maint = true;
        assert_eq!(
            node_overlay(&node),
            Some(NodeOverlay::Reserved { maint: true })
        );
    }

    #[test]
    fn node_overlay_planned() {
        let mut node = idle_node_info();
        node.planned_job_id = 42;
        assert_eq!(node_overlay(&node), Some(NodeOverlay::Planned));
    }

    #[test]
    fn node_overlay_reservation_wins_over_planned() {
        let mut node = idle_node_info();
        node.active_reservation = "r1".into();
        node.planned_job_id = 42;
        assert_eq!(
            node_overlay(&node),
            Some(NodeOverlay::Reserved { maint: false })
        );
    }
}
