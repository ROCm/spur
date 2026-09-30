// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Leader-local record of launches on the wire, so `abort_orphaned_placements`
//! can tell a genuinely-abandoned reservation from one still in flight.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use parking_lot::Mutex;
use spur_core::job::JobId;

/// Leader-local record of in-flight launches per node. Never persisted: a new
/// leader has issued no launches, so it has nothing to carry over.
#[derive(Default)]
pub(crate) struct DispatchTracker {
    state: Mutex<TrackerState>,
}

#[derive(Default)]
struct TrackerState {
    in_flight: HashMap<String, HashMap<JobId, usize>>,
}

impl DispatchTracker {
    /// Mark a launch to `node` as on the wire.
    pub(crate) fn begin(self: &Arc<Self>, node: &str, job_id: JobId) -> DispatchInFlight {
        let mut state = self.state.lock();
        *state
            .in_flight
            .entry(node.to_string())
            .or_default()
            .entry(job_id)
            .or_insert(0) += 1;
        DispatchInFlight {
            tracker: self.clone(),
            node: node.to_string(),
            job_id,
        }
    }

    /// Read by the sweep that gives up a reservation nobody is dispatching: a launch still
    /// on the wire is one whose own path will finish or abort it.
    pub(crate) fn jobs_in_flight(&self) -> HashSet<JobId> {
        self.state
            .lock()
            .in_flight
            .values()
            .flat_map(|jobs| jobs.keys().copied())
            .collect()
    }
}

/// Holds a launch open in the tracker. Released on drop so a panic or an early
/// return cannot leave a node's jobs permanently exempt from reconciliation.
pub(crate) struct DispatchInFlight {
    tracker: Arc<DispatchTracker>,
    node: String,
    job_id: JobId,
}

impl Drop for DispatchInFlight {
    fn drop(&mut self) {
        let mut state = self.tracker.state.lock();
        let Some(jobs) = state.in_flight.get_mut(&self.node) else {
            return;
        };
        if let Some(count) = jobs.get_mut(&self.job_id) {
            *count -= 1;
            if *count == 0 {
                jobs.remove(&self.job_id);
            }
        }
        if jobs.is_empty() {
            state.in_flight.remove(&self.node);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_launch_on_the_wire_is_in_flight() {
        let tracker = Arc::new(DispatchTracker::default());
        let in_flight = tracker.begin("n1", 7);

        assert!(tracker.jobs_in_flight().contains(&7));

        drop(in_flight);
        assert!(!tracker.jobs_in_flight().contains(&7));
    }

    #[test]
    fn a_launch_to_one_node_does_not_mark_another_nodes_job_in_flight() {
        let tracker = Arc::new(DispatchTracker::default());
        let _in_flight = tracker.begin("n1", 7);

        // jobs_in_flight is a flat set across every node by design (the sweep
        // only needs "is this job's dispatch outstanding anywhere"), but a
        // different job on a different node must not show up here.
        assert!(!tracker.jobs_in_flight().contains(&8));
    }

    #[test]
    fn overlapping_launches_for_one_job_release_independently() {
        let tracker = Arc::new(DispatchTracker::default());
        let first = tracker.begin("n1", 7);
        let second = tracker.begin("n1", 7);

        drop(first);
        assert!(
            tracker.jobs_in_flight().contains(&7),
            "the second launch is still on the wire"
        );

        drop(second);
        assert!(
            !tracker.jobs_in_flight().contains(&7),
            "both launches are done, so nothing is left in flight"
        );
    }
}
