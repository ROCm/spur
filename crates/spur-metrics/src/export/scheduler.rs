// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Scheduler metric registration for `/metrics/scheduler`.

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use std::sync::atomic::AtomicU64;

use crate::export::encode_registered;
use crate::export::{register_counter, register_gauge};
use crate::scheduler::SchedStatsSnapshot;

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct PluginLabel {
    plugin: String,
}

fn set_plugin_info(registry: &mut Registry, snap: &SchedStatsSnapshot) {
    let family = Family::<PluginLabel, Gauge<u64, AtomicU64>>::default();
    family
        .get_or_create(&PluginLabel {
            plugin: snap.plugin.clone(),
        })
        .set(1);
    registry.register(
        "spur_scheduler_info",
        "Active scheduler plugin (value is always 1)",
        family,
    );
}

/// Register scheduler metrics into `registry` from `snap`.
pub fn register_scheduler(registry: &mut Registry, snap: &SchedStatsSnapshot) {
    set_plugin_info(registry, snap);

    register_gauge(
        registry,
        "spur_scheduler_cycle_last_time_us",
        "Most recent scheduling cycle wall time in microseconds",
        snap.cycle_last_time_us,
    );
    register_gauge(
        registry,
        "spur_scheduler_cycle_total_time_us",
        "Cumulative scheduling cycle wall time in microseconds",
        snap.cycle_total_time_us,
    );
    register_counter(
        registry,
        "spur_scheduler_cycles",
        "Scheduling cycles executed since reset",
        snap.cycles,
    );
    register_gauge(
        registry,
        "spur_scheduler_schedule_last_time_us",
        "Most recent Scheduler::schedule() time in microseconds",
        snap.schedule_last_time_us,
    );
    register_gauge(
        registry,
        "spur_scheduler_schedule_total_time_us",
        "Cumulative Scheduler::schedule() time in microseconds",
        snap.schedule_total_time_us,
    );
    register_gauge(
        registry,
        "spur_scheduler_jobs_started_last_cycle",
        "Jobs started in the most recent scheduling cycle",
        snap.jobs_started_last_cycle,
    );
    register_counter(
        registry,
        "spur_scheduler_jobs_submitted",
        "Jobs submitted since reset",
        snap.jobs_submitted,
    );
    register_counter(
        registry,
        "spur_scheduler_jobs_started",
        "Jobs started since reset",
        snap.jobs_started,
    );
    register_counter(
        registry,
        "spur_scheduler_jobs_finalized",
        "Jobs reaching a terminal state since reset",
        snap.jobs_finalized,
    );
}

/// Encode scheduler metrics for `/metrics/scheduler` as OpenMetrics 1.0 text.
pub fn encode_scheduler_metrics(snap: &SchedStatsSnapshot) -> String {
    encode_registered(|registry| register_scheduler(registry, snap))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_snapshot() -> SchedStatsSnapshot {
        SchedStatsSnapshot {
            plugin: "backfill".into(),
            cycles: 10,
            cycle_total_time_us: 5000,
            cycle_last_time_us: 600,
            schedule_total_time_us: 1500,
            schedule_last_time_us: 200,
            jobs_submitted: 42,
            jobs_started: 30,
            jobs_finalized: 28,
            jobs_started_last_cycle: 3,
        }
    }

    fn metric_type<'a>(body: &'a str, name: &str) -> &'a str {
        let prefix = format!("# TYPE {name} ");
        body.lines()
            .find(|line| line.starts_with(&prefix))
            .unwrap_or_else(|| panic!("missing TYPE line for {name}"))
            .split_whitespace()
            .nth(3)
            .unwrap_or_else(|| panic!("malformed TYPE line for {name}"))
    }

    #[test]
    fn export_includes_scheduler_metrics() {
        let body = encode_scheduler_metrics(&sample_snapshot());
        assert!(body.contains("spur_scheduler_info{plugin=\"backfill\"} 1"));
        assert!(body.contains("spur_scheduler_cycles_total 10"));
        assert!(body.contains("spur_scheduler_cycle_total_time_us 5000"));
        assert!(!body.contains("_avg_time_us"));
        assert!(body.contains("spur_scheduler_jobs_submitted_total 42"));
        assert!(body.contains("spur_scheduler_jobs_started_last_cycle 3"));
        assert_eq!(metric_type(&body, "spur_scheduler_cycles"), "counter");
        assert_eq!(
            metric_type(&body, "spur_scheduler_jobs_submitted"),
            "counter"
        );
        assert_eq!(
            metric_type(&body, "spur_scheduler_cycle_last_time_us"),
            "gauge"
        );
        assert!(body.ends_with("# EOF\n"));
    }

    #[test]
    fn empty_snapshot_still_exports_plugin_info() {
        let snap = SchedStatsSnapshot {
            plugin: "backfill".into(),
            ..Default::default()
        };
        let body = encode_scheduler_metrics(&snap);
        assert!(body.contains("spur_scheduler_cycles_total 0"));
        assert_eq!(metric_type(&body, "spur_scheduler_cycles"), "counter");
        assert!(body.ends_with("# EOF\n"));
    }
}
