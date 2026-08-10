// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! k0s cluster + node metric registration for `/metrics/k8s`.

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use std::sync::atomic::AtomicU64;

use crate::export::encode_registered;
use crate::k8s::{
    phase_label, role_label, BaseLabel, K8sClusterMetricsSnapshot, K8sMetrics, ALL_PHASES,
};

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct PhaseLabel {
    distribution: String,
    cluster: String,
    phase: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct RoleLabel {
    distribution: String,
    cluster: String,
    role: String,
}

fn base_gauge(
    registry: &mut Registry,
    name: &str,
    help: &str,
    snap: &K8sClusterMetricsSnapshot,
    v: u64,
) {
    let family = Family::<BaseLabel, Gauge<u64, AtomicU64>>::default();
    family
        .get_or_create(&BaseLabel {
            distribution: snap.distribution.clone(),
            cluster: snap.cluster.clone(),
        })
        .set(v);
    registry.register(name, help, family);
}

/// Register the cluster-level gauges from `snap`.
pub fn register_k8s_cluster(registry: &mut Registry, snap: &K8sClusterMetricsSnapshot) {
    let phase_family = Family::<PhaseLabel, Gauge<u64, AtomicU64>>::default();
    for phase in ALL_PHASES {
        phase_family
            .get_or_create(&PhaseLabel {
                distribution: snap.distribution.clone(),
                cluster: snap.cluster.clone(),
                phase: phase_label(phase).into(),
            })
            .set(u64::from(phase == snap.phase));
    }
    registry.register(
        "spur_k8s_cluster_phase",
        "Current k0s cluster phase as a one-hot state set (value 1 on the active phase)",
        phase_family,
    );

    base_gauge(
        registry,
        "spur_k8s_cluster_up",
        "Whether the k0s cluster is Ready (1) or not (0)",
        snap,
        snap.up(),
    );
    base_gauge(
        registry,
        "spur_k8s_control_plane_replicas",
        "Configured k0s control-plane replica count",
        snap,
        snap.control_plane_replicas,
    );
    base_gauge(
        registry,
        "spur_k8s_nodes_total",
        "Total nodes with a k0s role assigned",
        snap,
        snap.nodes_total,
    );

    let role_family = Family::<RoleLabel, Gauge<u64, AtomicU64>>::default();
    for (role, count) in snap.nodes_by_role {
        role_family
            .get_or_create(&RoleLabel {
                distribution: snap.distribution.clone(),
                cluster: snap.cluster.clone(),
                role: role_label(role).into(),
            })
            .set(count);
    }
    registry.register(
        "spur_k8s_nodes_by_role",
        "k0s node count per role",
        role_family,
    );
}

/// Encode `/metrics/k8s`: cluster gauges from `snap` plus the long-lived lifecycle/node metrics.
pub fn encode_k8s_metrics(snap: &K8sClusterMetricsSnapshot, metrics: &K8sMetrics) -> String {
    metrics.ensure_series(&snap.cluster);
    encode_registered(|registry| {
        register_k8s_cluster(registry, snap);
        metrics.register(registry);
    })
}

/// Encode only the cluster gauges (used by tests and callers without a live accumulator).
pub fn encode_k8s_cluster_metrics(snap: &K8sClusterMetricsSnapshot) -> String {
    encode_registered(|registry| register_k8s_cluster(registry, snap))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k8s::K8sMetrics;
    use spur_core::k0s::{K0sPhase, K0sRole};

    fn sample() -> K8sClusterMetricsSnapshot {
        K8sClusterMetricsSnapshot::collect(
            "prod",
            3,
            K0sPhase::Ready,
            [
                Some(K0sRole::Controller),
                Some(K0sRole::Worker),
                Some(K0sRole::Worker),
            ],
        )
    }

    #[test]
    fn cluster_gauges_render_expected_series() {
        let body = encode_k8s_cluster_metrics(&sample());
        assert!(body.contains(
            "spur_k8s_cluster_phase{distribution=\"k0s\",cluster=\"prod\",phase=\"ready\"} 1\n"
        ));
        assert!(body.contains(
            "spur_k8s_cluster_phase{distribution=\"k0s\",cluster=\"prod\",phase=\"down\"} 0\n"
        ));
        assert!(body.contains("spur_k8s_cluster_up{distribution=\"k0s\",cluster=\"prod\"} 1\n"));
        assert!(body.contains(
            "spur_k8s_control_plane_replicas{distribution=\"k0s\",cluster=\"prod\"} 3\n"
        ));
        assert!(body.contains("spur_k8s_nodes_total{distribution=\"k0s\",cluster=\"prod\"} 3\n"));
        assert!(body.contains(
            "spur_k8s_nodes_by_role{distribution=\"k0s\",cluster=\"prod\",role=\"worker\"} 2\n"
        ));
        assert!(body.ends_with("# EOF\n"));
    }

    #[test]
    fn combined_export_includes_lifecycle_and_node_metrics() {
        let metrics = K8sMetrics::new();
        metrics.record_provision_attempt("prod");
        metrics.record_provision_failure("prod");
        metrics.record_phase_transition("prod", K0sPhase::Down, K0sPhase::Provisioning);
        metrics.record_reconcile_error("prod");
        metrics.observe_reconcile_duration("prod", 0.02);
        metrics.set_node_up("prod", "node-a", K0sRole::Worker, true);
        metrics.set_node_restart_total("prod", "node-a", 2);
        metrics.observe_node_install_duration("prod", "node-a", 3.0);

        let body = encode_k8s_metrics(&sample(), &metrics);
        assert!(body.contains(
            "spur_k8s_provision_attempts_total{distribution=\"k0s\",cluster=\"prod\"} 1\n"
        ));
        assert!(body.contains(
            "spur_k8s_provision_failures_total{distribution=\"k0s\",cluster=\"prod\"} 1\n"
        ));
        assert!(body.contains("from=\"down\",to=\"provisioning\"} 1\n"));
        assert!(body.contains(
            "spur_k8s_reconcile_errors_total{distribution=\"k0s\",cluster=\"prod\"} 1\n"
        ));
        assert!(body.contains("spur_k8s_reconcile_duration_seconds_count"));
        assert!(body.contains(
            "spur_k8s_node_up{distribution=\"k0s\",cluster=\"prod\",node=\"node-a\",role=\"worker\"} 1\n"
        ));
        assert!(body.contains(
            "spur_k8s_node_restart_total{distribution=\"k0s\",cluster=\"prod\",node=\"node-a\"} 2\n"
        ));
        assert!(body.contains("spur_k8s_install_duration_seconds_count"));
        assert!(body.ends_with("# EOF\n"));
    }
}
