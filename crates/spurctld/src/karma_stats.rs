// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Per-user karma event counters, accumulated in memory on the Raft leader.
//!
//! Each counter is monotonically increasing (true Prometheus counter semantics).
//! On leader failover the counters reset to zero; Prometheus `rate()` / `increase()`
//! handle resets transparently.

use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::{DashMap, DashSet};
use spur_core::job::JobState;
use spur_metrics::KarmaStatsSnapshot;

#[derive(Debug, Default)]
struct UserCounters {
    submitted: AtomicU64,
    completed: AtomicU64,
    failed: AtomicU64,
    timeout: AtomicU64,
    node_fail: AtomicU64,
    cancelled: AtomicU64,
    gpus_requested: AtomicU64,
    overflow_borrows: AtomicU64,
    walltime_requested_secs: AtomicU64,
    walltime_actual_secs: AtomicU64,
}

/// Leader-side per-user karma counters.
///
/// Reset on every follower→leader transition so that a re-elected leader does
/// not resume exporting stale values that Prometheus would interpret as new work.
#[derive(Debug, Default)]
pub struct KarmaStatsCollector {
    users: DashMap<String, UserCounters>,
    started_jobs: DashSet<u32>,
}

impl KarmaStatsCollector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&self) {
        self.users.clear();
        self.started_jobs.clear();
    }

    pub fn record_submitted(&self, username: &str) {
        self.users
            .entry(username.to_string())
            .or_default()
            .submitted
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_started(
        &self,
        job_id: u32,
        username: &str,
        total_gpus: u64,
        time_limit_secs: u64,
        idle_fill: bool,
    ) {
        if !self.started_jobs.insert(job_id) {
            return;
        }
        let entry = self.users.entry(username.to_string()).or_default();
        if total_gpus > 0 {
            entry
                .gpus_requested
                .fetch_add(total_gpus, Ordering::Relaxed);
        }
        if time_limit_secs > 0 {
            entry
                .walltime_requested_secs
                .fetch_add(time_limit_secs, Ordering::Relaxed);
        }
        if idle_fill {
            entry.overflow_borrows.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_finalized(&self, username: &str, state: JobState, actual_secs: u64) {
        let entry = self.users.entry(username.to_string()).or_default();
        match state {
            JobState::Completed => {
                entry.completed.fetch_add(1, Ordering::Relaxed);
            }
            JobState::Failed | JobState::OutOfMemory | JobState::Deadline => {
                entry.failed.fetch_add(1, Ordering::Relaxed);
            }
            JobState::Timeout => {
                entry.timeout.fetch_add(1, Ordering::Relaxed);
            }
            JobState::NodeFail => {
                entry.node_fail.fetch_add(1, Ordering::Relaxed);
            }
            JobState::Cancelled => {
                entry.cancelled.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
        if actual_secs > 0 {
            entry
                .walltime_actual_secs
                .fetch_add(actual_secs, Ordering::Relaxed);
        }
    }

    pub fn snapshot(&self) -> KarmaStatsSnapshot {
        let mut users = Vec::with_capacity(self.users.len());
        for entry in self.users.iter() {
            let c = entry.value();
            users.push((
                entry.key().clone(),
                spur_metrics::KarmaUserSnapshot {
                    submitted: c.submitted.load(Ordering::Relaxed),
                    completed: c.completed.load(Ordering::Relaxed),
                    failed: c.failed.load(Ordering::Relaxed),
                    timeout: c.timeout.load(Ordering::Relaxed),
                    node_fail: c.node_fail.load(Ordering::Relaxed),
                    cancelled: c.cancelled.load(Ordering::Relaxed),
                    gpus_requested: c.gpus_requested.load(Ordering::Relaxed),
                    overflow_borrows: c.overflow_borrows.load(Ordering::Relaxed),
                    walltime_requested_secs: c.walltime_requested_secs.load(Ordering::Relaxed),
                    walltime_actual_secs: c.walltime_actual_secs.load(Ordering::Relaxed),
                },
            ));
        }
        users.sort_by(|a, b| a.0.cmp(&b.0));
        KarmaStatsSnapshot { users }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submitted_increments() {
        let k = KarmaStatsCollector::new();
        k.record_submitted("alice");
        k.record_submitted("alice");
        k.record_submitted("bob");
        let snap = k.snapshot();
        assert_eq!(snap.users.len(), 2);
        let alice = snap.users.iter().find(|(u, _)| u == "alice").unwrap();
        assert_eq!(alice.1.submitted, 2);
        let bob = snap.users.iter().find(|(u, _)| u == "bob").unwrap();
        assert_eq!(bob.1.submitted, 1);
    }

    #[test]
    fn started_accumulates_gpus_walltime_overflow() {
        let k = KarmaStatsCollector::new();
        k.record_started(1, "alice", 4, 3600, false);
        k.record_started(2, "alice", 8, 7200, true);
        let snap = k.snapshot();
        let alice = &snap.users[0].1;
        assert_eq!(alice.gpus_requested, 12);
        assert_eq!(alice.walltime_requested_secs, 10800);
        assert_eq!(alice.overflow_borrows, 1);
    }

    #[test]
    fn started_deduplicates_by_job_id() {
        let k = KarmaStatsCollector::new();
        k.record_started(1, "alice", 4, 3600, true);
        k.record_started(1, "alice", 4, 3600, true);
        let snap = k.snapshot();
        let alice = &snap.users[0].1;
        assert_eq!(alice.gpus_requested, 4);
        assert_eq!(alice.walltime_requested_secs, 3600);
        assert_eq!(alice.overflow_borrows, 1);
    }

    #[test]
    fn finalized_routes_states() {
        let k = KarmaStatsCollector::new();
        k.record_finalized("alice", JobState::Completed, 100);
        k.record_finalized("alice", JobState::Completed, 200);
        k.record_finalized("alice", JobState::Failed, 50);
        k.record_finalized("alice", JobState::Timeout, 30);
        k.record_finalized("alice", JobState::NodeFail, 10);
        k.record_finalized("alice", JobState::Cancelled, 0);
        k.record_finalized("alice", JobState::OutOfMemory, 20);
        k.record_finalized("alice", JobState::Deadline, 15);
        let snap = k.snapshot();
        let a = &snap.users[0].1;
        assert_eq!(a.completed, 2);
        assert_eq!(a.failed, 3); // Failed + OOM + Deadline
        assert_eq!(a.timeout, 1);
        assert_eq!(a.node_fail, 1);
        assert_eq!(a.cancelled, 1);
        assert_eq!(a.walltime_actual_secs, 425);
    }
}
