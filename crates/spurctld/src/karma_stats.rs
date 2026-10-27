// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Per-user karma counters on the Raft leader. Reset on failover;
//! Prometheus `increase()` handles resets transparently.

use std::collections::{HashMap, HashSet};

use parking_lot::Mutex;
use spur_core::job::JobState;
use spur_metrics::KarmaStatsSnapshot;

#[derive(Debug, Default)]
struct UserCounters {
    submitted: u64,
    completed: u64,
    failed: u64,
    timeout: u64,
    node_fail: u64,
    cancelled: u64,
    gpus_requested: u64,
    overflow_borrows: u64,
    walltime_requested_secs: u64,
    walltime_actual_secs: u64,
}

/// Leader-side per-user karma counters.
///
/// Reset on every follower→leader transition so that a re-elected leader does
/// not resume exporting stale values that Prometheus would interpret as new work.
#[derive(Debug, Default)]
pub struct KarmaStatsCollector {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    users: HashMap<String, UserCounters>,
    // Never evicted: requeue reuses the job_id, so a terminal-state eviction would re-count.
    started_jobs: HashSet<u32>,
}

impl KarmaStatsCollector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&self) {
        let mut inner = self.inner.lock();
        inner.users.clear();
        inner.started_jobs.clear();
    }

    pub fn record_submitted(&self, username: &str) {
        self.inner
            .lock()
            .users
            .entry(username.to_string())
            .or_default()
            .submitted += 1;
    }

    pub fn record_started(
        &self,
        job_id: u32,
        username: &str,
        total_gpus: u64,
        time_limit_secs: u64,
        idle_fill: bool,
    ) {
        let mut inner = self.inner.lock();
        if !inner.started_jobs.insert(job_id) {
            return;
        }
        let entry = inner.users.entry(username.to_string()).or_default();
        if total_gpus > 0 {
            entry.gpus_requested += total_gpus;
        }
        if time_limit_secs > 0 {
            entry.walltime_requested_secs += time_limit_secs;
        }
        if idle_fill {
            entry.overflow_borrows += 1;
        }
    }

    pub fn record_finalized(&self, username: &str, state: JobState, actual_secs: u64) {
        let mut inner = self.inner.lock();
        let entry = inner.users.entry(username.to_string()).or_default();
        match state {
            JobState::Completed => {
                entry.completed += 1;
            }
            JobState::Failed | JobState::OutOfMemory | JobState::Deadline => {
                entry.failed += 1;
            }
            JobState::Timeout => {
                entry.timeout += 1;
            }
            JobState::NodeFail => {
                entry.node_fail += 1;
            }
            JobState::Cancelled => {
                entry.cancelled += 1;
            }
            _ => {}
        }
        if actual_secs > 0 {
            entry.walltime_actual_secs += actual_secs;
        }
    }

    pub fn snapshot(&self) -> KarmaStatsSnapshot {
        let inner = self.inner.lock();
        let mut users: Vec<_> = inner
            .users
            .iter()
            .map(|(name, c)| {
                (
                    name.clone(),
                    spur_metrics::KarmaUserSnapshot {
                        submitted: c.submitted,
                        completed: c.completed,
                        failed: c.failed,
                        timeout: c.timeout,
                        node_fail: c.node_fail,
                        cancelled: c.cancelled,
                        gpus_requested: c.gpus_requested,
                        overflow_borrows: c.overflow_borrows,
                        walltime_requested_secs: c.walltime_requested_secs,
                        walltime_actual_secs: c.walltime_actual_secs,
                    },
                )
            })
            .collect();
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

    #[test]
    fn requeue_does_not_recount_request() {
        let k = KarmaStatsCollector::new();
        // Start job 1, preempt it, restart with same id -- must not re-count.
        k.record_started(1, "alice", 4, 3600, true);
        k.record_finalized("alice", JobState::Preempted, 1800);
        k.record_started(1, "alice", 4, 3600, true);
        let snap = k.snapshot();
        let alice = &snap.users[0].1;
        assert_eq!(alice.gpus_requested, 4);
        assert_eq!(alice.walltime_requested_secs, 3600);
        assert_eq!(alice.overflow_borrows, 1);
        assert_eq!(alice.walltime_actual_secs, 1800);

        // Complete it, then start same id again -- still counted once.
        k.record_finalized("alice", JobState::Completed, 1800);
        k.record_started(1, "alice", 8, 7200, false);
        let snap = k.snapshot();
        assert_eq!(
            snap.users[0].1.gpus_requested, 4,
            "never evict: same job_id always deduped"
        );
    }
}
