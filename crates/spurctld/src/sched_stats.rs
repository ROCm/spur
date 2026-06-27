// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! In-memory accumulator for scheduler cycle and lifecycle statistics.

use parking_lot::Mutex;
use spur_metrics::SchedStatsSnapshot;

/// Leader-side scheduler statistics since process start or the last reset.
#[derive(Debug)]
pub struct SchedStatsCollector {
    plugin: String,
    inner: Mutex<SchedAccum>,
}

#[derive(Debug, Default)]
struct SchedAccum {
    cycles: u64,
    cycle_total_time_us: u64,
    cycle_last_time_us: u64,
    schedule_total_time_us: u64,
    schedule_last_time_us: u64,
    jobs_submitted: u64,
    jobs_started: u64,
    jobs_completed: u64,
    jobs_started_last_cycle: u64,
}

impl SchedStatsCollector {
    pub fn new(plugin: impl Into<String>) -> Self {
        Self {
            plugin: plugin.into(),
            inner: Mutex::new(SchedAccum::default()),
        }
    }

    pub fn record_cycle(&self, cycle_time_us: u64, schedule_time_us: u64, jobs_started: u64) {
        let mut accum = self.inner.lock();
        accum.cycles += 1;
        accum.cycle_total_time_us = accum.cycle_total_time_us.saturating_add(cycle_time_us);
        accum.cycle_last_time_us = cycle_time_us;
        accum.schedule_total_time_us = accum
            .schedule_total_time_us
            .saturating_add(schedule_time_us);
        accum.schedule_last_time_us = schedule_time_us;
        accum.jobs_started_last_cycle = jobs_started;
    }

    pub fn record_submitted(&self, count: u64) {
        if count > 0 {
            let mut accum = self.inner.lock();
            accum.jobs_submitted = accum.jobs_submitted.saturating_add(count);
        }
    }

    pub fn record_started(&self) {
        self.inner.lock().jobs_started += 1;
    }

    pub fn record_completed(&self) {
        self.inner.lock().jobs_completed += 1;
    }

    pub fn snapshot(&self) -> SchedStatsSnapshot {
        let accum = self.inner.lock();
        SchedStatsSnapshot {
            plugin: self.plugin.clone(),
            cycles: accum.cycles,
            cycle_total_time_us: accum.cycle_total_time_us,
            cycle_last_time_us: accum.cycle_last_time_us,
            schedule_total_time_us: accum.schedule_total_time_us,
            schedule_last_time_us: accum.schedule_last_time_us,
            jobs_submitted: accum.jobs_submitted,
            jobs_started: accum.jobs_started,
            jobs_completed: accum.jobs_completed,
            jobs_started_last_cycle: accum.jobs_started_last_cycle,
        }
    }

    pub fn reset(&self) {
        *self.inner.lock() = SchedAccum::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_cycle_accumulates_timing() {
        let stats = SchedStatsCollector::new("backfill");
        stats.record_cycle(1000, 200, 2);
        stats.record_cycle(500, 100, 1);

        let snap = stats.snapshot();
        assert_eq!(snap.plugin, "backfill");
        assert_eq!(snap.cycles, 2);
        assert_eq!(snap.cycle_total_time_us, 1500);
        assert_eq!(snap.cycle_last_time_us, 500);
        assert_eq!(snap.cycle_avg_time_us(), 750);
        assert_eq!(snap.schedule_total_time_us, 300);
        assert_eq!(snap.schedule_last_time_us, 100);
        assert_eq!(snap.schedule_avg_time_us(), 150);
        assert_eq!(snap.jobs_started_last_cycle, 1);
    }

    #[test]
    fn lifecycle_counters_accumulate() {
        let stats = SchedStatsCollector::new("backfill");
        stats.record_submitted(3);
        stats.record_started();
        stats.record_started();
        stats.record_completed();

        let snap = stats.snapshot();
        assert_eq!(snap.jobs_submitted, 3);
        assert_eq!(snap.jobs_started, 2);
        assert_eq!(snap.jobs_completed, 1);
    }

    #[test]
    fn reset_clears_accumulators() {
        let stats = SchedStatsCollector::new("backfill");
        stats.record_cycle(100, 50, 1);
        stats.record_submitted(1);
        stats.reset();
        let snap = stats.snapshot();
        assert_eq!(snap.cycles, 0);
        assert_eq!(snap.jobs_submitted, 0);
        assert_eq!(snap.plugin, "backfill");
    }
}
