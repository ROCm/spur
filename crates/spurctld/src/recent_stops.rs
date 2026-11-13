// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Leader-local record of the stops a controller delivered to agents, so the
//! heartbeat reclaim does not re-send a stop a run is still working through.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use parking_lot::Mutex;
use spur_core::job::JobId;

type StopKey = (JobId, u32, String);

/// When a CancelJob RPC last succeeded per (job, run attempt, node). Never
/// persisted or replicated: a new leader starts empty and may re-send once,
/// which the agent absorbs.
#[derive(Default)]
pub(crate) struct RecentStops {
    delivered: Mutex<HashMap<StopKey, DateTime<Utc>>>,
}

impl RecentStops {
    /// Records a delivery at `at`, dropping entries older than `retain` so the
    /// map stays bounded by the runs stopped within that span.
    pub(crate) fn record(
        &self,
        job_id: JobId,
        run_attempt: u32,
        node: &str,
        at: DateTime<Utc>,
        retain: Duration,
    ) {
        let mut delivered = self.delivered.lock();
        delivered.retain(|_, when| at - *when <= retain);
        delivered.insert((job_id, run_attempt, node.to_string()), at);
    }

    /// Whether a stop reached `node` for this run strictly less than `window`
    /// before `now`.
    pub(crate) fn delivered_within(
        &self,
        job_id: JobId,
        run_attempt: u32,
        node: &str,
        window: Duration,
        now: DateTime<Utc>,
    ) -> bool {
        let key = (job_id, run_attempt, node.to_string());
        self.delivered
            .lock()
            .get(&key)
            .is_some_and(|when| now - *when < window)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.delivered.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + secs, 0).unwrap()
    }

    fn secs(n: i64) -> Duration {
        Duration::seconds(n)
    }

    #[test]
    fn a_recorded_stop_is_found_inside_its_window() {
        let stops = RecentStops::default();
        stops.record(7, 2, "n1", at(0), secs(600));

        assert!(stops.delivered_within(7, 2, "n1", secs(66), at(10)));
    }

    #[test]
    fn nothing_is_found_for_a_stop_never_recorded() {
        let stops = RecentStops::default();

        assert!(!stops.delivered_within(7, 2, "n1", secs(66), at(0)));
    }

    #[test]
    fn the_window_is_exclusive_at_its_boundary() {
        let stops = RecentStops::default();
        stops.record(7, 2, "n1", at(0), secs(600));

        assert!(stops.delivered_within(7, 2, "n1", secs(66), at(65)));
        assert!(!stops.delivered_within(7, 2, "n1", secs(66), at(66)));
        assert!(!stops.delivered_within(7, 2, "n1", secs(66), at(67)));
    }

    #[test]
    fn another_run_attempt_job_or_node_is_not_matched() {
        let stops = RecentStops::default();
        stops.record(7, 2, "n1", at(0), secs(600));

        assert!(!stops.delivered_within(7, 1, "n1", secs(66), at(1)));
        assert!(!stops.delivered_within(7, 3, "n1", secs(66), at(1)));
        assert!(!stops.delivered_within(8, 2, "n1", secs(66), at(1)));
        assert!(!stops.delivered_within(7, 2, "n2", secs(66), at(1)));
    }

    #[test]
    fn a_later_delivery_restarts_the_window() {
        let stops = RecentStops::default();
        stops.record(7, 2, "n1", at(0), secs(600));
        stops.record(7, 2, "n1", at(100), secs(600));

        assert!(stops.delivered_within(7, 2, "n1", secs(66), at(150)));
        assert_eq!(stops.len(), 1);
    }

    #[test]
    fn recording_drops_entries_older_than_the_retention() {
        let stops = RecentStops::default();
        stops.record(1, 1, "n1", at(0), secs(100));
        stops.record(2, 1, "n1", at(50), secs(100));
        assert_eq!(stops.len(), 2);

        stops.record(3, 1, "n1", at(101), secs(100));

        assert_eq!(stops.len(), 2, "the entry from t=0 is past the retention");
        assert!(!stops.delivered_within(1, 1, "n1", secs(1000), at(101)));
        assert!(stops.delivered_within(2, 1, "n1", secs(1000), at(101)));
        assert!(stops.delivered_within(3, 1, "n1", secs(1000), at(101)));
    }
}
