// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

/// Snapshot of controller scheduler statistics since process start or last reset.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchedStatsSnapshot {
    pub plugin: String,
    pub cycles: u64,
    pub cycle_total_time_us: u64,
    pub cycle_last_time_us: u64,
    pub schedule_total_time_us: u64,
    pub schedule_last_time_us: u64,
    pub jobs_submitted: u64,
    pub jobs_started: u64,
    pub jobs_completed: u64,
    pub jobs_started_last_cycle: u64,
}

impl SchedStatsSnapshot {
    pub fn cycle_avg_time_us(&self) -> u64 {
        self.cycle_total_time_us
            .checked_div(self.cycles)
            .unwrap_or(0)
    }

    pub fn schedule_avg_time_us(&self) -> u64 {
        self.schedule_total_time_us
            .checked_div(self.cycles)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avg_time_us_is_zero_when_cycles_zero() {
        let snap = SchedStatsSnapshot {
            cycle_total_time_us: 1000,
            schedule_total_time_us: 500,
            ..Default::default()
        };
        assert_eq!(snap.cycle_avg_time_us(), 0);
        assert_eq!(snap.schedule_avg_time_us(), 0);
    }

    #[test]
    fn avg_time_us_divides_total_by_cycles() {
        let snap = SchedStatsSnapshot {
            cycles: 4,
            cycle_total_time_us: 4000,
            schedule_total_time_us: 800,
            ..Default::default()
        };
        assert_eq!(snap.cycle_avg_time_us(), 1000);
        assert_eq!(snap.schedule_avg_time_us(), 200);
    }
}
