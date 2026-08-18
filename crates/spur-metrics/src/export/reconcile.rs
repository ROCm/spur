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

fn bump(family: &CounterFamily<TriggerLabel>, trigger: &str, value: u64) {
    if value > 0 {
        family
            .get_or_create(&TriggerLabel {
                trigger: trigger.to_string(),
            })
            .inc_by(value);
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
    let last_under = Family::<TriggerLabel, Gauge<u64, AtomicU64>>::default();

    for r in &snap.rebuilds {
        bump(&rebuilds, &r.trigger, r.rebuilds);
        bump(&under_nodes, &r.trigger, r.nodes_undercharged);
        bump(&over_nodes, &r.trigger, r.nodes_overcharged);
        bump(&cpus_under, &r.trigger, r.cpus_undercharged);
        bump(&cpus_over, &r.trigger, r.cpus_overcharged);
        bump(&devices_under, &r.trigger, r.devices_undercharged);
        bump(&devices_over, &r.trigger, r.devices_overcharged);
        last_under
            .get_or_create(&TriggerLabel {
                trigger: r.trigger.clone(),
            })
            .set(r.last_nodes_undercharged);
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
                cpus_overcharged: 0,
                devices_undercharged: 4,
                devices_overcharged: 1,
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
    fn empty_snapshot_emits_no_labels() {
        let body = encode_reconcile_metrics(&ReconcileStatsSnapshot::default());
        assert!(!body.contains("trigger="));
        assert!(!body.contains("cause="));
        assert!(body.ends_with("# EOF\n"));
    }
}
