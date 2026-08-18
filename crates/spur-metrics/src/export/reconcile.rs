// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Allocation-reconciliation metric registration for `/metrics/reconcile`.

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use std::sync::atomic::AtomicU64;

use crate::export::{encode_registered, register_counter};
use crate::reconcile::ReconcileStatsSnapshot;

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct TriggerLabel {
    trigger: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct CauseLabel {
    cause: String,
}

type CounterFamily<L> = Family<L, Counter<u64, AtomicU64>>;

fn bump(family: &CounterFamily<TriggerLabel>, label: &TriggerLabel, value: u64) {
    // get_or_create even at zero: a clean rebuild must still publish the series,
    // or absent()-style alerts fire on a healthy cluster.
    let counter = family.get_or_create(label);
    if value > 0 {
        counter.inc_by(value);
    }
}

/// Register reconciliation counters into `registry` from `snap`.
pub fn register_reconcile(registry: &mut Registry, snap: &ReconcileStatsSnapshot) {
    let rebuilds = CounterFamily::<TriggerLabel>::default();
    let under_nodes = CounterFamily::<TriggerLabel>::default();
    let over_nodes = CounterFamily::<TriggerLabel>::default();
    let cpus_under = CounterFamily::<TriggerLabel>::default();
    let cpus_over = CounterFamily::<TriggerLabel>::default();
    let devices_under = CounterFamily::<TriggerLabel>::default();
    let devices_over = CounterFamily::<TriggerLabel>::default();
    let memory_under = CounterFamily::<TriggerLabel>::default();
    let memory_over = CounterFamily::<TriggerLabel>::default();
    let last_under = Family::<TriggerLabel, Gauge<u64, AtomicU64>>::default();
    let unaccounted = Family::<TriggerLabel, Gauge<u64, AtomicU64>>::default();
    let checked = Family::<TriggerLabel, Gauge<u64, AtomicU64>>::default();

    for r in &snap.rebuilds {
        let label = TriggerLabel {
            trigger: r.trigger.clone(),
        };
        bump(&rebuilds, &label, r.rebuilds);
        bump(&under_nodes, &label, r.nodes_undercharged);
        bump(&over_nodes, &label, r.nodes_overcharged);
        bump(&cpus_under, &label, r.cpus_undercharged);
        bump(&cpus_over, &label, r.cpus_overcharged);
        bump(&memory_under, &label, r.memory_undercharged_mb);
        bump(&memory_over, &label, r.memory_overcharged_mb);
        bump(&devices_under, &label, r.devices_undercharged);
        bump(&devices_over, &label, r.devices_overcharged);
        last_under
            .get_or_create(&label)
            .set(r.last_nodes_undercharged);
        unaccounted.get_or_create(&label).set(r.unaccounted_slices);
        checked.get_or_create(&label).set(r.nodes_checked);
    }

    let reclaims = CounterFamily::<CauseLabel>::default();
    for r in &snap.reclaims {
        if r.count > 0 {
            reclaims
                .get_or_create(&CauseLabel {
                    cause: r.cause.clone(),
                })
                .inc_by(r.count);
        }
    }

    registry.register(
        "spur_reconcile_rebuilds",
        "Node allocation rebuilds run, by trigger",
        rebuilds,
    );
    registry.register(
        "spur_reconcile_undercharged_nodes",
        "Nodes charged less than their job records prove is in use, by trigger",
        under_nodes,
    );
    registry.register(
        "spur_reconcile_overcharged_nodes",
        "Nodes charged beyond their job records, by trigger",
        over_nodes,
    );
    registry.register(
        "spur_reconcile_last_undercharged_nodes",
        "Nodes undercharged on the most recent rebuild, by trigger",
        last_under,
    );
    registry.register(
        "spur_reconcile_undercharged_cpus",
        "CPUs the node index was short of its job records, by trigger",
        cpus_under,
    );
    registry.register(
        "spur_reconcile_overcharged_cpus",
        "CPUs the node index held beyond its job records, by trigger",
        cpus_over,
    );
    registry.register(
        "spur_reconcile_undercharged_devices",
        "Device units the node index was short of its job records, by trigger",
        devices_under,
    );
    registry.register(
        "spur_reconcile_overcharged_devices",
        "Device units the node index held beyond its job records, by trigger",
        devices_over,
    );
    registry.register(
        "spur_reconcile_undercharged_memory_mb",
        "Memory (MB) the node index was short of its job records, by trigger",
        memory_under,
    );
    registry.register(
        "spur_reconcile_overcharged_memory_mb",
        "Memory (MB) the node index held beyond its job records, by trigger",
        memory_over,
    );
    registry.register(
        "spur_reconcile_unaccounted_slices",
        "Active job/node pairs whose record carries no allocation, so nothing is charged for them",
        unaccounted,
    );
    registry.register(
        "spur_reconcile_nodes_checked",
        "Nodes examined on the most recent pass, by trigger",
        checked,
    );
    registry.register(
        "spur_reconcile_agent_job_reclaims",
        "Jobs reclaimed from an agent heartbeat, by cause",
        reclaims,
    );
    register_counter(
        registry,
        "spur_reconcile_heartbeats",
        "Agent heartbeats examined for held-job disagreement",
        snap.heartbeats,
    );
    register_counter(
        registry,
        "spur_reconcile_empty_heartbeats",
        "Agent heartbeats reporting no held jobs",
        snap.heartbeats_empty,
    );
}

/// Encode reconciliation metrics for `/metrics/reconcile` as OpenMetrics 1.0 text.
pub fn encode_reconcile_metrics(snap: &ReconcileStatsSnapshot) -> String {
    encode_registered(|registry| register_reconcile(registry, snap))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconcile::{RebuildSnapshot, ReclaimCauseSnapshot};

    fn sample() -> ReconcileStatsSnapshot {
        ReconcileStatsSnapshot {
            rebuilds: vec![RebuildSnapshot {
                trigger: "restore".into(),
                rebuilds: 3,
                nodes_undercharged: 5,
                nodes_overcharged: 1,
                last_nodes_undercharged: 2,
                cpus_undercharged: 8,
                cpus_overcharged: 9,
                memory_undercharged_mb: 2048,
                memory_overcharged_mb: 4096,
                devices_undercharged: 4,
                devices_overcharged: 1,
                unaccounted_slices: 3,
                nodes_checked: 12,
            }],
            reclaims: vec![ReclaimCauseSnapshot {
                cause: "terminal".into(),
                count: 7,
            }],
            heartbeats: 100,
            heartbeats_empty: 40,
        }
    }

    #[test]
    fn export_labels_rebuilds_by_trigger() {
        let body = encode_reconcile_metrics(&sample());
        assert!(body.contains("spur_reconcile_rebuilds_total{trigger=\"restore\"} 3"));
        assert!(body.contains("spur_reconcile_undercharged_nodes_total{trigger=\"restore\"} 5"));
        assert!(body.contains("spur_reconcile_overcharged_nodes_total{trigger=\"restore\"} 1"));
        assert!(body.contains("spur_reconcile_last_undercharged_nodes{trigger=\"restore\"} 2"));
        assert!(body.contains("spur_reconcile_undercharged_cpus_total{trigger=\"restore\"} 8"));
        assert!(body.contains("spur_reconcile_undercharged_devices_total{trigger=\"restore\"} 4"));
        assert!(
            body.contains("spur_reconcile_undercharged_memory_mb_total{trigger=\"restore\"} 2048")
        );
        assert!(body.contains("spur_reconcile_unaccounted_slices{trigger=\"restore\"} 3"));
        assert!(body.contains("spur_reconcile_nodes_checked{trigger=\"restore\"} 12"));
        // Distinct values per dimension, so a family wired to the wrong snapshot
        // field cannot pass by coincidence.
        assert!(body.contains("spur_reconcile_overcharged_cpus_total{trigger=\"restore\"} 9"));
        assert!(
            body.contains("spur_reconcile_overcharged_memory_mb_total{trigger=\"restore\"} 4096")
        );
        assert!(body.contains("spur_reconcile_overcharged_devices_total{trigger=\"restore\"} 1"));
        assert!(body.ends_with("# EOF\n"));
    }

    #[test]
    fn export_labels_reclaims_by_cause() {
        let body = encode_reconcile_metrics(&sample());
        assert!(body.contains("spur_reconcile_agent_job_reclaims_total{cause=\"terminal\"} 7"));
        assert!(body.contains("spur_reconcile_heartbeats_total 100"));
        assert!(body.contains("spur_reconcile_empty_heartbeats_total 40"));
    }

    #[test]
    fn a_clean_rebuild_still_publishes_its_series_at_zero() {
        let snap = ReconcileStatsSnapshot {
            rebuilds: vec![RebuildSnapshot {
                trigger: "leadership_gain".into(),
                rebuilds: 1,
                ..Default::default()
            }],
            ..Default::default()
        };
        let body = encode_reconcile_metrics(&snap);
        assert!(
            body.contains("spur_reconcile_undercharged_nodes_total{trigger=\"leadership_gain\"} 0"),
            "a zero series must still be published or absent() alerts fire on a healthy cluster"
        );
    }

    #[test]
    fn empty_snapshot_emits_no_labels() {
        let body = encode_reconcile_metrics(&ReconcileStatsSnapshot::default());
        assert!(!body.contains("trigger="));
        assert!(!body.contains("cause="));
        assert!(body.ends_with("# EOF\n"));
    }
}
