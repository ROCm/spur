// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Snapshot of k0s cluster + node state for `/metrics/k8s` gauge export, plus the long-lived
//! `K8sMetrics` accumulator for lifecycle counters/histograms and heartbeat-fed node metrics.

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::{exponential_buckets, Histogram};
use prometheus_client::registry::Registry;

use spur_core::k0s::{K0sPhase, K0sRole};

/// Distribution label value. All current implementations are k0s; the label exists so future
/// distributions can share the `spur_k8s_*` metric surface.
pub const DEFAULT_DISTRIBUTION: &str = "k0s";

/// All phases in state-set order, for exporting `spur_k8s_cluster_phase` as a one-hot label set.
pub const ALL_PHASES: [K0sPhase; 4] = [
    K0sPhase::Down,
    K0sPhase::Provisioning,
    K0sPhase::Ready,
    K0sPhase::Degraded,
];

/// Lower-case label value for a phase (matches the serde `snake_case` wire form).
pub fn phase_label(phase: K0sPhase) -> &'static str {
    match phase {
        K0sPhase::Down => "down",
        K0sPhase::Provisioning => "provisioning",
        K0sPhase::Ready => "ready",
        K0sPhase::Degraded => "degraded",
    }
}

/// Lower-case label value for a role.
pub fn role_label(role: K0sRole) -> &'static str {
    match role {
        K0sRole::Controller => "controller",
        K0sRole::Worker => "worker",
        K0sRole::Single => "single",
    }
}

/// Current k0s cluster-level state, rebuilt from live controller state on each scrape.
#[derive(Debug, Clone)]
pub struct K8sClusterMetricsSnapshot {
    pub cluster: String,
    pub distribution: String,
    pub phase: K0sPhase,
    pub control_plane_replicas: u64,
    pub nodes_total: u64,
    pub nodes_by_role: [(K0sRole, u64); 3],
}

impl Default for K8sClusterMetricsSnapshot {
    fn default() -> Self {
        Self {
            cluster: String::new(),
            distribution: DEFAULT_DISTRIBUTION.into(),
            phase: K0sPhase::Down,
            control_plane_replicas: 0,
            nodes_total: 0,
            nodes_by_role: [
                (K0sRole::Controller, 0),
                (K0sRole::Worker, 0),
                (K0sRole::Single, 0),
            ],
        }
    }
}

impl K8sClusterMetricsSnapshot {
    /// Build a snapshot from the cluster name, replica count, phase, and the assigned roles of all
    /// nodes that currently hold a k0s role (`None` roles are ignored).
    pub fn collect(
        cluster: impl Into<String>,
        control_plane_replicas: u64,
        phase: K0sPhase,
        node_roles: impl IntoIterator<Item = Option<K0sRole>>,
    ) -> Self {
        let mut controller = 0;
        let mut worker = 0;
        let mut single = 0;
        for role in node_roles.into_iter().flatten() {
            match role {
                K0sRole::Controller => controller += 1,
                K0sRole::Worker => worker += 1,
                K0sRole::Single => single += 1,
            }
        }
        let nodes_total = controller + worker + single;
        Self {
            cluster: cluster.into(),
            distribution: DEFAULT_DISTRIBUTION.into(),
            phase,
            control_plane_replicas,
            nodes_total,
            nodes_by_role: [
                (K0sRole::Controller, controller),
                (K0sRole::Worker, worker),
                (K0sRole::Single, single),
            ],
        }
    }

    /// 1 when the cluster phase is Ready (the primary alerting signal), else 0.
    pub fn up(&self) -> u64 {
        u64::from(self.phase == K0sPhase::Ready)
    }
}

/// Base `distribution` + `cluster` labels carried by every k8s metric.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct BaseLabel {
    pub distribution: String,
    pub cluster: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct TransitionLabel {
    distribution: String,
    cluster: String,
    from: String,
    to: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct NodeBaseLabel {
    distribution: String,
    cluster: String,
    node: String,
}

/// Long-lived accumulator for k0s lifecycle counters/histograms and heartbeat-fed node metrics.
///
/// Cumulative and event-timed metrics cannot be rebuilt from a plain snapshot each scrape the way
/// gauges are, so the controller holds one `Arc<K8sMetrics>` for the process lifetime and the
/// `/metrics/k8s` handler registers clones (Arc-shared inner state) into the response registry.
#[derive(Debug, Clone)]
pub struct K8sMetrics {
    provision_attempts: Family<BaseLabel, Counter>,
    provision_failures: Family<BaseLabel, Counter>,
    phase_transitions: Family<TransitionLabel, Counter>,
    reconcile_errors: Family<BaseLabel, Counter>,
    reconcile_duration: Family<BaseLabel, Histogram>,
    node_up: Family<NodeBaseLabel, Gauge>,
    node_restarts: Family<NodeBaseLabel, Counter>,
    node_install_duration: Family<NodeBaseLabel, Histogram>,
}

impl Default for K8sMetrics {
    fn default() -> Self {
        // Reconcile loop and install both span sub-second to tens of seconds.
        let reconcile_buckets = || Histogram::new(exponential_buckets(0.005, 2.0, 12));
        let install_buckets = || Histogram::new(exponential_buckets(0.5, 2.0, 10));
        Self {
            provision_attempts: Family::default(),
            provision_failures: Family::default(),
            phase_transitions: Family::default(),
            reconcile_errors: Family::default(),
            reconcile_duration: Family::new_with_constructor(reconcile_buckets),
            node_up: Family::default(),
            node_restarts: Family::default(),
            node_install_duration: Family::new_with_constructor(install_buckets),
        }
    }
}

impl K8sMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    fn base(&self, cluster: &str) -> BaseLabel {
        BaseLabel {
            distribution: DEFAULT_DISTRIBUTION.into(),
            cluster: cluster.to_string(),
        }
    }

    /// Record a provisioning attempt (a Down→Provisioning transition).
    pub fn record_provision_attempt(&self, cluster: &str) {
        self.provision_attempts
            .get_or_create(&self.base(cluster))
            .inc();
    }

    /// Record a provisioning failure (failed to reach Ready).
    pub fn record_provision_failure(&self, cluster: &str) {
        self.provision_failures
            .get_or_create(&self.base(cluster))
            .inc();
    }

    /// Record a phase transition from `from` to `to`.
    pub fn record_phase_transition(&self, cluster: &str, from: K0sPhase, to: K0sPhase) {
        self.phase_transitions
            .get_or_create(&TransitionLabel {
                distribution: DEFAULT_DISTRIBUTION.into(),
                cluster: cluster.to_string(),
                from: phase_label(from).into(),
                to: phase_label(to).into(),
            })
            .inc();
    }

    /// Record a reconcile-loop error (token minting, RPC to node agents, etc.).
    pub fn record_reconcile_error(&self, cluster: &str) {
        self.reconcile_errors
            .get_or_create(&self.base(cluster))
            .inc();
    }

    /// Record the wall time of one reconcile-loop iteration.
    pub fn observe_reconcile_duration(&self, cluster: &str, seconds: f64) {
        self.reconcile_duration
            .get_or_create(&self.base(cluster))
            .observe(seconds);
    }

    fn node_label(&self, cluster: &str, node: &str) -> NodeBaseLabel {
        NodeBaseLabel {
            distribution: DEFAULT_DISTRIBUTION.into(),
            cluster: cluster.to_string(),
            node: node.to_string(),
        }
    }

    /// Set a node's k0s unit up/down gauge (1 active, 0 otherwise). Role is intentionally not a
    /// label here (it would churn/leak series on a role change); per-role counts live in the
    /// fresh-per-scrape `spur_k8s_nodes_by_role` gauge.
    pub fn set_node_up(&self, cluster: &str, node: &str, active: bool) {
        self.node_up
            .get_or_create(&self.node_label(cluster, node))
            .set(i64::from(active));
    }

    /// Absolute cumulative restart count reported by a node; advances the counter by any positive
    /// delta so a monotonic counter is preserved across scrapes.
    pub fn set_node_restart_total(&self, cluster: &str, node: &str, total: u64) {
        let counter = self
            .node_restarts
            .get_or_create(&self.node_label(cluster, node));
        let current = counter.get();
        if total > current {
            counter.inc_by(total - current);
        }
    }

    /// Observe a node's k0s install duration (once per successful install).
    pub fn observe_node_install_duration(&self, cluster: &str, node: &str, seconds: f64) {
        self.node_install_duration
            .get_or_create(&self.node_label(cluster, node))
            .observe(seconds);
    }

    /// Drop all per-node series for a node that left k0s inventory (deregistered or role cleared),
    /// so decommissioned names don't accumulate and export stale values forever.
    pub fn remove_node(&self, cluster: &str, node: &str) {
        let label = self.node_label(cluster, node);
        self.node_up.remove(&label);
        self.node_restarts.remove(&label);
        self.node_install_duration.remove(&label);
    }

    /// Ensure the base-labeled counter/histogram series exist at zero for `cluster`, so a fresh
    /// controller still exposes them (alerting rules reference the series before the first event).
    pub fn ensure_series(&self, cluster: &str) {
        let base = self.base(cluster);
        let _ = self.provision_attempts.get_or_create(&base);
        let _ = self.provision_failures.get_or_create(&base);
        let _ = self.reconcile_errors.get_or_create(&base);
        let _ = self.reconcile_duration.get_or_create(&base);
    }

    /// Register the accumulator's metrics (Arc-shared clones) into a response registry.
    pub fn register(&self, registry: &mut Registry) {
        registry.register(
            "spur_k8s_provision_attempts",
            "k0s provisioning attempts (Down to Provisioning transitions)",
            self.provision_attempts.clone(),
        );
        registry.register(
            "spur_k8s_provision_failures",
            "k0s provisioning attempts that failed to reach Ready",
            self.provision_failures.clone(),
        );
        registry.register(
            "spur_k8s_phase_transitions",
            "k0s cluster phase transitions labeled by source and destination",
            self.phase_transitions.clone(),
        );
        registry.register(
            "spur_k8s_reconcile_errors",
            "Errors during a k0s reconcile-loop iteration",
            self.reconcile_errors.clone(),
        );
        registry.register(
            "spur_k8s_reconcile_duration_seconds",
            "Wall time of each k0s reconcile-loop iteration",
            self.reconcile_duration.clone(),
        );
        registry.register(
            "spur_k8s_node_up",
            "Whether a node's k0s systemd unit reports active (1) or not (0)",
            self.node_up.clone(),
        );
        registry.register(
            "spur_k8s_node_restart",
            "Cumulative k0s unit restarts per node",
            self.node_restarts.clone(),
        );
        registry.register(
            "spur_k8s_install_duration_seconds",
            "Time to install k0s and bring the unit active, per node",
            self.node_install_duration.clone(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_counts_roles_and_ignores_unassigned() {
        let roles = [
            Some(K0sRole::Controller),
            Some(K0sRole::Worker),
            Some(K0sRole::Worker),
            None,
        ];
        let snap = K8sClusterMetricsSnapshot::collect("c1", 1, K0sPhase::Ready, roles);
        assert_eq!(snap.nodes_total, 3);
        assert_eq!(snap.nodes_by_role[0], (K0sRole::Controller, 1));
        assert_eq!(snap.nodes_by_role[1], (K0sRole::Worker, 2));
        assert_eq!(snap.nodes_by_role[2], (K0sRole::Single, 0));
        assert_eq!(snap.up(), 1);
    }

    #[test]
    fn up_is_zero_when_not_ready() {
        let snap = K8sClusterMetricsSnapshot::collect("c1", 3, K0sPhase::Degraded, [None]);
        assert_eq!(snap.up(), 0);
        assert_eq!(snap.control_plane_replicas, 3);
    }
}
