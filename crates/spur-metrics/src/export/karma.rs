// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Per-user karma counter registration for OpenMetrics export.

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::registry::Registry;
use std::sync::atomic::AtomicU64;

use crate::export::encode_registered;
use crate::karma::{KarmaStatsSnapshot, KarmaUserSnapshot};

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct UserLabel {
    username: String,
}

type LabeledCounter = Family<UserLabel, Counter<u64, AtomicU64>>;

struct CounterDef {
    name: &'static str,
    help: &'static str,
    value: fn(&KarmaUserSnapshot) -> u64,
}

const COUNTERS: &[CounterDef] = &[
    CounterDef {
        name: "spur_user_jobs_submitted",
        help: "Total jobs submitted by user (monotonic counter)",
        value: |u| u.submitted,
    },
    CounterDef {
        name: "spur_user_jobs_completed",
        help: "Total jobs completed successfully by user",
        value: |u| u.completed,
    },
    CounterDef {
        name: "spur_user_jobs_failed",
        help: "Total user-attributable job failures (Failed + OOM + Deadline)",
        value: |u| u.failed,
    },
    CounterDef {
        name: "spur_user_jobs_timeout",
        help: "Total jobs that hit their wall time limit",
        value: |u| u.timeout,
    },
    CounterDef {
        name: "spur_user_jobs_node_fail",
        help: "Total jobs failed due to node failure (not user's fault)",
        value: |u| u.node_fail,
    },
    CounterDef {
        name: "spur_user_jobs_cancelled",
        help: "Total jobs cancelled by user",
        value: |u| u.cancelled,
    },
    CounterDef {
        name: "spur_user_gpus_requested",
        help: "Total GPUs requested across all dispatched jobs",
        value: |u| u.gpus_requested,
    },
    CounterDef {
        name: "spur_user_overflow_borrows",
        help: "Total jobs dispatched using borrowed/overflow capacity",
        value: |u| u.overflow_borrows,
    },
    CounterDef {
        name: "spur_user_walltime_requested_seconds",
        help: "Total wall time requested in seconds across all dispatched jobs",
        value: |u| u.walltime_requested_secs,
    },
    CounterDef {
        name: "spur_user_walltime_actual_seconds",
        help: "Total actual wall time consumed in seconds across all terminal jobs",
        value: |u| u.walltime_actual_secs,
    },
];

pub fn register_karma(registry: &mut Registry, snap: &KarmaStatsSnapshot) {
    for def in COUNTERS {
        let family = LabeledCounter::default();
        for (username, user_snap) in &snap.users {
            let label = UserLabel {
                username: username.clone(),
            };
            let val = (def.value)(user_snap);
            if val > 0 {
                family.get_or_create(&label).inc_by(val);
            } else {
                let _ = family.get_or_create(&label);
            }
        }
        registry.register(def.name, def.help, family);
    }
}

pub fn encode_karma_metrics(snap: &KarmaStatsSnapshot) -> String {
    encode_registered(|registry| register_karma(registry, snap))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_snapshot_produces_eof_only() {
        let body = encode_karma_metrics(&KarmaStatsSnapshot::default());
        assert_eq!(body, "# EOF\n");
    }

    #[test]
    fn counters_appear_with_total_suffix() {
        let snap = KarmaStatsSnapshot {
            users: vec![(
                "alice".into(),
                KarmaUserSnapshot {
                    submitted: 10,
                    completed: 8,
                    failed: 1,
                    timeout: 1,
                    node_fail: 0,
                    cancelled: 0,
                    gpus_requested: 40,
                    overflow_borrows: 0,
                    walltime_requested_secs: 3600,
                    walltime_actual_secs: 2700,
                },
            )],
        };
        let body = encode_karma_metrics(&snap);
        assert!(
            body.contains("# TYPE spur_user_jobs_submitted counter"),
            "missing counter TYPE for submitted\n{body}"
        );
        assert!(body.contains("spur_user_jobs_submitted_total{username=\"alice\"} 10"));
        assert!(body.contains("spur_user_jobs_completed_total{username=\"alice\"} 8"));
        assert!(body.contains("spur_user_jobs_failed_total{username=\"alice\"} 1"));
        assert!(body.contains("spur_user_gpus_requested_total{username=\"alice\"} 40"));
        assert!(
            body.contains("spur_user_walltime_requested_seconds_total{username=\"alice\"} 3600")
        );
        assert!(body.contains("spur_user_walltime_actual_seconds_total{username=\"alice\"} 2700"));
    }

    #[test]
    fn zero_counters_still_present() {
        let snap = KarmaStatsSnapshot {
            users: vec![(
                "bob".into(),
                KarmaUserSnapshot {
                    submitted: 1,
                    ..Default::default()
                },
            )],
        };
        let body = encode_karma_metrics(&snap);
        assert!(body.contains("spur_user_jobs_submitted_total{username=\"bob\"} 1"));
        assert!(body.contains("spur_user_overflow_borrows_total{username=\"bob\"} 0"));
    }
}
