// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Observe-only accumulator for allocation-reconciliation statistics. Nothing
//! here feeds a scheduling or eviction decision.

use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use spur_metrics::reconcile::{RebuildSnapshot, ReclaimCauseSnapshot, ReconcileStatsSnapshot};

use crate::cluster::AllocDrift;

/// Why a rebuild ran. Both are points where a process's view of the cluster is
/// newly assembled and therefore may carry drift it did not create.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebuildTrigger {
    /// Installing a Raft snapshot, which ships the writing leader's accumulator.
    Restore,
    /// This process just became leader and is about to schedule against the index.
    LeadershipGain,
}

impl RebuildTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Restore => "restore",
            Self::LeadershipGain => "leadership_gain",
        }
    }
}

/// Why a job reported by an agent was reclaimed. Mirrors the branches of
/// `reclaim_cause`, so a shift in the mix points at which fault is occurring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReclaimCause {
    /// The controller has the job in a terminal state.
    Terminal,
    /// The controller has the job active, but allocated to different nodes.
    ActiveElsewhere,
    /// The controller has no record of a job id it must once have issued.
    Unknown,
}

impl ReclaimCause {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::ActiveElsewhere => "active_elsewhere",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct RebuildAccum {
    rebuilds: u64,
    nodes_undercharged: u64,
    nodes_overcharged: u64,
    last_nodes_undercharged: u64,
    cpus_undercharged: u64,
    cpus_overcharged: u64,
    memory_undercharged_mb: u64,
    memory_overcharged_mb: u64,
    devices_undercharged: u64,
    devices_overcharged: u64,
    unaccounted_slices: u64,
    nodes_checked: u64,
}

impl RebuildAccum {
    fn record(&mut self, drift: AllocDrift) {
        self.rebuilds = self.rebuilds.saturating_add(1);
        self.nodes_undercharged = self
            .nodes_undercharged
            .saturating_add(drift.nodes_undercharged as u64);
        self.nodes_overcharged = self
            .nodes_overcharged
            .saturating_add(drift.nodes_overcharged as u64);
        self.last_nodes_undercharged = drift.nodes_undercharged as u64;
        self.cpus_undercharged = self
            .cpus_undercharged
            .saturating_add(drift.cpus_undercharged);
        self.cpus_overcharged = self.cpus_overcharged.saturating_add(drift.cpus_overcharged);
        self.memory_undercharged_mb = self
            .memory_undercharged_mb
            .saturating_add(drift.memory_undercharged_mb);
        self.memory_overcharged_mb = self
            .memory_overcharged_mb
            .saturating_add(drift.memory_overcharged_mb);
        self.devices_undercharged = self
            .devices_undercharged
            .saturating_add(drift.devices_undercharged);
        self.devices_overcharged = self
            .devices_overcharged
            .saturating_add(drift.devices_overcharged);
        // Last-value, not cumulative: both are exported as gauges describing the
        // most recent pass.
        self.unaccounted_slices = drift.unaccounted_slices as u64;
        self.nodes_checked = drift.nodes_checked as u64;
    }
}

/// Allocation-reconciliation statistics since process start.
#[derive(Debug, Default)]
pub struct ReconcileStatsCollector {
    restore: Mutex<RebuildAccum>,
    leadership_gain: Mutex<RebuildAccum>,
    reclaim_terminal: AtomicU64,
    reclaim_active_elsewhere: AtomicU64,
    reclaim_unknown: AtomicU64,
    heartbeats: AtomicU64,
    heartbeats_empty: AtomicU64,
}

impl ReconcileStatsCollector {
    pub fn new() -> Self {
        Self::default()
    }

    fn accum(&self, trigger: RebuildTrigger) -> &Mutex<RebuildAccum> {
        match trigger {
            RebuildTrigger::Restore => &self.restore,
            RebuildTrigger::LeadershipGain => &self.leadership_gain,
        }
    }

    pub fn record_rebuild(&self, trigger: RebuildTrigger, drift: AllocDrift) {
        self.accum(trigger).lock().record(drift);
    }

    pub fn record_reclaim(&self, cause: ReclaimCause) {
        let counter = match cause {
            ReclaimCause::Terminal => &self.reclaim_terminal,
            ReclaimCause::ActiveElsewhere => &self.reclaim_active_elsewhere,
            ReclaimCause::Unknown => &self.reclaim_unknown,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Record one heartbeat's held-job report. `held` is what the agent sent,
    /// which is empty both when it holds nothing and when its job map was locked.
    pub fn record_heartbeat(&self, held: usize) {
        self.heartbeats.fetch_add(1, Ordering::Relaxed);
        if held == 0 {
            self.heartbeats_empty.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn snapshot(&self) -> ReconcileStatsSnapshot {
        let rebuilds = [
            (RebuildTrigger::Restore, *self.restore.lock()),
            (RebuildTrigger::LeadershipGain, *self.leadership_gain.lock()),
        ]
        .into_iter()
        .filter(|(_, a)| a.rebuilds > 0)
        .map(|(trigger, a)| RebuildSnapshot {
            trigger: trigger.as_str().to_string(),
            rebuilds: a.rebuilds,
            nodes_undercharged: a.nodes_undercharged,
            nodes_overcharged: a.nodes_overcharged,
            last_nodes_undercharged: a.last_nodes_undercharged,
            cpus_undercharged: a.cpus_undercharged,
            cpus_overcharged: a.cpus_overcharged,
            memory_undercharged_mb: a.memory_undercharged_mb,
            memory_overcharged_mb: a.memory_overcharged_mb,
            devices_undercharged: a.devices_undercharged,
            devices_overcharged: a.devices_overcharged,
            unaccounted_slices: a.unaccounted_slices,
            nodes_checked: a.nodes_checked,
        })
        .collect();

        let reclaims = [
            (ReclaimCause::Terminal, &self.reclaim_terminal),
            (
                ReclaimCause::ActiveElsewhere,
                &self.reclaim_active_elsewhere,
            ),
            (ReclaimCause::Unknown, &self.reclaim_unknown),
        ]
        .into_iter()
        .map(|(cause, c)| (cause, c.load(Ordering::Relaxed)))
        .filter(|(_, count)| *count > 0)
        .map(|(cause, count)| ReclaimCauseSnapshot {
            cause: cause.as_str().to_string(),
            count,
        })
        .collect();

        ReconcileStatsSnapshot {
            rebuilds,
            reclaims,
            heartbeats: self.heartbeats.load(Ordering::Relaxed),
            heartbeats_empty: self.heartbeats_empty.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drift(under: usize, over: usize, cpus_under: u64, cpus_over: u64) -> AllocDrift {
        AllocDrift {
            nodes_checked: 10,
            nodes_undercharged: under,
            nodes_overcharged: over,
            cpus_undercharged: cpus_under,
            cpus_overcharged: cpus_over,
            ..Default::default()
        }
    }

    #[test]
    fn every_drift_dimension_reaches_the_snapshot() {
        let stats = ReconcileStatsCollector::new();
        stats.record_rebuild(
            RebuildTrigger::Restore,
            AllocDrift {
                nodes_checked: 12,
                unaccounted_slices: 3,
                nodes_undercharged: 1,
                nodes_overcharged: 2,
                cpus_undercharged: 4,
                cpus_overcharged: 5,
                memory_undercharged_mb: 6,
                memory_overcharged_mb: 7,
                devices_undercharged: 8,
                devices_overcharged: 9,
            },
        );

        let r = &stats.snapshot().rebuilds[0];
        assert_eq!(r.nodes_checked, 12);
        assert_eq!(r.unaccounted_slices, 3);
        assert_eq!(r.nodes_undercharged, 1);
        assert_eq!(r.nodes_overcharged, 2);
        assert_eq!(r.cpus_undercharged, 4);
        assert_eq!(r.cpus_overcharged, 5);
        assert_eq!(r.memory_undercharged_mb, 6);
        assert_eq!(r.memory_overcharged_mb, 7);
        assert_eq!(r.devices_undercharged, 8);
        assert_eq!(r.devices_overcharged, 9);
    }

    #[test]
    fn the_last_pass_gauges_replace_rather_than_accumulate() {
        let stats = ReconcileStatsCollector::new();
        for slices in [5, 2] {
            stats.record_rebuild(
                RebuildTrigger::Restore,
                AllocDrift {
                    nodes_checked: 12,
                    unaccounted_slices: slices,
                    ..Default::default()
                },
            );
        }

        let r = &stats.snapshot().rebuilds[0];
        assert_eq!(
            r.unaccounted_slices, 2,
            "gauge reports the most recent pass"
        );
        assert_eq!(r.nodes_checked, 12);
    }

    #[test]
    fn the_two_drift_directions_accumulate_separately() {
        let stats = ReconcileStatsCollector::new();
        stats.record_rebuild(RebuildTrigger::Restore, drift(1, 0, 8, 0));
        stats.record_rebuild(RebuildTrigger::Restore, drift(2, 1, 0, 3));

        let snap = stats.snapshot();
        let r = &snap.rebuilds[0];
        assert_eq!(r.trigger, "restore");
        assert_eq!(r.rebuilds, 2);
        assert_eq!(r.nodes_undercharged, 3);
        assert_eq!(r.nodes_overcharged, 1);
        assert_eq!(r.cpus_undercharged, 8);
        assert_eq!(r.cpus_overcharged, 3);
    }

    #[test]
    fn last_undercharged_tracks_the_most_recent_rebuild() {
        let stats = ReconcileStatsCollector::new();
        stats.record_rebuild(RebuildTrigger::LeadershipGain, drift(5, 0, 0, 0));
        stats.record_rebuild(RebuildTrigger::LeadershipGain, drift(0, 0, 0, 0));

        let snap = stats.snapshot();
        assert_eq!(snap.rebuilds[0].nodes_undercharged, 5);
        assert_eq!(snap.rebuilds[0].last_nodes_undercharged, 0);
    }

    #[test]
    fn triggers_accumulate_independently() {
        let stats = ReconcileStatsCollector::new();
        stats.record_rebuild(RebuildTrigger::Restore, drift(1, 0, 1, 0));
        stats.record_rebuild(RebuildTrigger::LeadershipGain, drift(2, 0, 2, 0));

        let snap = stats.snapshot();
        assert_eq!(snap.rebuilds.len(), 2);
        assert_eq!(snap.rebuilds[0].trigger, "restore");
        assert_eq!(snap.rebuilds[0].nodes_undercharged, 1);
        assert_eq!(snap.rebuilds[1].trigger, "leadership_gain");
        assert_eq!(snap.rebuilds[1].nodes_undercharged, 2);
    }

    #[test]
    fn a_clean_rebuild_still_counts_as_a_rebuild() {
        let stats = ReconcileStatsCollector::new();
        stats.record_rebuild(RebuildTrigger::Restore, AllocDrift::default());

        let snap = stats.snapshot();
        assert_eq!(snap.rebuilds[0].rebuilds, 1);
        assert_eq!(snap.rebuilds[0].nodes_undercharged, 0);
        assert_eq!(snap.rebuilds[0].nodes_overcharged, 0);
    }

    #[test]
    fn heartbeat_counts_split_empty_reports() {
        let stats = ReconcileStatsCollector::new();
        stats.record_heartbeat(2);
        stats.record_heartbeat(0);
        stats.record_heartbeat(0);

        let snap = stats.snapshot();
        assert_eq!(snap.heartbeats, 3);
        assert_eq!(snap.heartbeats_empty, 2);
    }

    #[test]
    fn reclaims_are_counted_per_cause_and_omit_causes_that_never_fired() {
        let stats = ReconcileStatsCollector::new();
        stats.record_reclaim(ReclaimCause::Terminal);
        stats.record_reclaim(ReclaimCause::Terminal);
        stats.record_reclaim(ReclaimCause::Unknown);

        let snap = stats.snapshot();
        assert_eq!(snap.reclaims.len(), 2);
        assert_eq!(snap.reclaims[0].cause, "terminal");
        assert_eq!(snap.reclaims[0].count, 2);
        assert_eq!(snap.reclaims[1].cause, "unknown");
        assert_eq!(snap.reclaims[1].count, 1);
    }
}
