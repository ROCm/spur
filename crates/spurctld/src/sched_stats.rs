// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! In-memory accumulator for scheduler cycle and lifecycle statistics.

use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use spur_metrics::SchedStatsSnapshot;

#[derive(Debug, Default)]
struct CycleAccum {
    cycles: u64,
    cycle_total_time_us: u64,
    cycle_last_time_us: u64,
    schedule_total_time_us: u64,
    schedule_last_time_us: u64,
    jobs_started_last_cycle: u64,
}

/// Leader-side scheduler statistics since process start or the last reset.
#[derive(Debug)]
pub struct SchedStatsCollector {
    plugin: String,
    cycle: Mutex<CycleAccum>,
    jobs_submitted: AtomicU64,
    jobs_started: AtomicU64,
    jobs_finalized: AtomicU64,
    exit_end: AtomicU64,
    exit_max_depth: AtomicU64,
}

impl SchedStatsCollector {
    pub fn new(plugin: impl Into<String>) -> Self {
        Self {
            plugin: plugin.into(),
            cycle: Mutex::new(CycleAccum::default()),
            jobs_submitted: AtomicU64::new(0),
            jobs_started: AtomicU64::new(0),
            jobs_finalized: AtomicU64::new(0),
            exit_end: AtomicU64::new(0),
            exit_max_depth: AtomicU64::new(0),
        }
    }

    /// Record one scheduling cycle. `hit_depth_limit` is true when more jobs
    /// were pending than `scheduler.max_jobs_per_cycle` allowed considering.
    pub fn record_cycle(
        &self,
        cycle_time_us: u64,
        schedule_time_us: u64,
        jobs_started: u64,
        hit_depth_limit: bool,
    ) {
        let mut accum = self.cycle.lock();
        accum.cycles = accum.cycles.saturating_add(1);
        accum.cycle_total_time_us = accum.cycle_total_time_us.saturating_add(cycle_time_us);
        accum.cycle_last_time_us = cycle_time_us;
        accum.schedule_total_time_us = accum
            .schedule_total_time_us
            .saturating_add(schedule_time_us);
        accum.schedule_last_time_us = schedule_time_us;
        accum.jobs_started_last_cycle = jobs_started;
        drop(accum);
        if jobs_started > 0 {
            self.jobs_started.fetch_add(jobs_started, Ordering::Relaxed);
        }
        if hit_depth_limit {
            self.exit_max_depth.fetch_add(1, Ordering::Relaxed);
        } else {
            self.exit_end.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_submitted(&self, count: u64) {
        if count > 0 {
            self.jobs_submitted.fetch_add(count, Ordering::Relaxed);
        }
    }

    pub fn record_finalized(&self) {
        self.jobs_finalized.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> SchedStatsSnapshot {
        let accum = self.cycle.lock();
        SchedStatsSnapshot {
            plugin: self.plugin.clone(),
            cycles: accum.cycles,
            cycle_total_time_us: accum.cycle_total_time_us,
            cycle_last_time_us: accum.cycle_last_time_us,
            schedule_total_time_us: accum.schedule_total_time_us,
            schedule_last_time_us: accum.schedule_last_time_us,
            jobs_submitted: self.jobs_submitted.load(Ordering::Relaxed),
            jobs_started: self.jobs_started.load(Ordering::Relaxed),
            jobs_finalized: self.jobs_finalized.load(Ordering::Relaxed),
            jobs_started_last_cycle: accum.jobs_started_last_cycle,
            exit_end: self.exit_end.load(Ordering::Relaxed),
            exit_max_depth: self.exit_max_depth.load(Ordering::Relaxed),
        }
    }

    pub fn reset(&self) {
        *self.cycle.lock() = CycleAccum::default();
        self.jobs_submitted.store(0, Ordering::Relaxed);
        self.jobs_started.store(0, Ordering::Relaxed);
        self.jobs_finalized.store(0, Ordering::Relaxed);
        self.exit_end.store(0, Ordering::Relaxed);
        self.exit_max_depth.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_cycle_accumulates_timing_and_started_jobs() {
        let stats = SchedStatsCollector::new("backfill");
        stats.record_cycle(1000, 200, 2, false);
        stats.record_cycle(500, 100, 1, false);

        let snap = stats.snapshot();
        assert_eq!(snap.plugin, "backfill");
        assert_eq!(snap.cycles, 2);
        assert_eq!(snap.cycle_total_time_us, 1500);
        assert_eq!(snap.cycle_last_time_us, 500);
        assert_eq!(snap.cycle_avg_time_us(), 750);
        assert_eq!(snap.schedule_total_time_us, 300);
        assert_eq!(snap.schedule_last_time_us, 100);
        assert_eq!(snap.schedule_avg_time_us(), 150);
        assert_eq!(snap.jobs_started, 3);
        assert_eq!(snap.jobs_started_last_cycle, 1);
    }

    #[test]
    fn lifecycle_counters_accumulate() {
        let stats = SchedStatsCollector::new("backfill");
        stats.record_submitted(3);
        stats.record_cycle(0, 0, 2, false);
        stats.record_finalized();

        let snap = stats.snapshot();
        assert_eq!(snap.jobs_submitted, 3);
        assert_eq!(snap.jobs_started, 2);
        assert_eq!(snap.jobs_finalized, 1);
    }

    #[test]
    fn exit_reason_counters_split_by_depth_limit() {
        let stats = SchedStatsCollector::new("backfill");
        stats.record_cycle(100, 50, 1, false);
        stats.record_cycle(100, 50, 1, true);
        stats.record_cycle(100, 50, 1, true);

        let snap = stats.snapshot();
        assert_eq!(snap.exit_end, 1);
        assert_eq!(snap.exit_max_depth, 2);
    }

    #[test]
    fn reset_clears_accumulators() {
        let stats = SchedStatsCollector::new("backfill");
        stats.record_cycle(100, 50, 1, true);
        stats.record_submitted(1);
        stats.reset();
        let snap = stats.snapshot();
        assert_eq!(snap.cycles, 0);
        assert_eq!(snap.jobs_submitted, 0);
        assert_eq!(snap.jobs_started, 0);
        assert_eq!(snap.exit_end, 0);
        assert_eq!(snap.exit_max_depth, 0);
        assert_eq!(snap.plugin, "backfill");
    }
}
