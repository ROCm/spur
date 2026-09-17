// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! An agent cuts its ledger at an instant the controller cannot observe, so a cut
//! taken mid-launch omits a run Raft records. Watch the overlap, don't filter after.

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
    watches: HashMap<u64, WatchState>,
    next_watch_id: u64,
}

struct WatchState {
    node: String,
    observed: HashSet<JobId>,
}

impl DispatchTracker {
    /// Mark a launch to `node` as on the wire. Every watch open on that node —
    /// now or later in this launch's lifetime — records it.
    pub(crate) fn begin(self: &Arc<Self>, node: &str, job_id: JobId) -> DispatchInFlight {
        let mut state = self.state.lock();
        *state
            .in_flight
            .entry(node.to_string())
            .or_default()
            .entry(job_id)
            .or_insert(0) += 1;
        for watch in state.watches.values_mut().filter(|w| w.node == node) {
            watch.observed.insert(job_id);
        }
        DispatchInFlight {
            tracker: self.clone(),
            node: node.to_string(),
            job_id,
        }
    }

    /// Start observing launches to `node`. Open this before asking the agent for
    /// a cut; the cut cannot be trusted about anything the watch goes on to see.
    pub(crate) fn watch(self: &Arc<Self>, node: &str) -> DispatchWatch {
        let mut state = self.state.lock();
        let id = state.next_watch_id;
        state.next_watch_id += 1;
        let observed = state
            .in_flight
            .get(node)
            .map(|jobs| jobs.keys().copied().collect())
            .unwrap_or_default();
        state.watches.insert(
            id,
            WatchState {
                node: node.to_string(),
                observed,
            },
        );
        DispatchWatch {
            tracker: self.clone(),
            id,
        }
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

/// The launches that overlapped one reconcile. Deregisters on drop, so the
/// tracker holds nothing once the reconcile that opened it has finished.
pub(crate) struct DispatchWatch {
    tracker: Arc<DispatchTracker>,
    id: u64,
}

impl DispatchWatch {
    /// Jobs whose launch to the watched node overlapped this watch. A cut taken
    /// during the watch may predate any of them, so its silence proves nothing.
    pub(crate) fn observed(&self) -> HashSet<JobId> {
        self.tracker
            .state
            .lock()
            .watches
            .get(&self.id)
            .map(|w| w.observed.clone())
            .unwrap_or_default()
    }
}

impl Drop for DispatchWatch {
    fn drop(&mut self) {
        self.tracker.state.lock().watches.remove(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_launch_that_starts_and_finishes_inside_a_watch_is_still_observed() {
        // The case a "what is in flight right now" filter misses: by the time the
        // diff runs the launch is long done, but the cut may still predate it.
        let tracker = Arc::new(DispatchTracker::default());
        let watch = tracker.watch("n1");

        drop(tracker.begin("n1", 7));

        assert!(watch.observed().contains(&7));
    }

    #[test]
    fn a_launch_already_on_the_wire_when_the_watch_opens_is_observed() {
        let tracker = Arc::new(DispatchTracker::default());
        let in_flight = tracker.begin("n1", 7);

        let watch = tracker.watch("n1");
        drop(in_flight);

        assert!(watch.observed().contains(&7));
    }

    #[test]
    fn a_watch_ignores_launches_to_other_nodes() {
        let tracker = Arc::new(DispatchTracker::default());
        let watch = tracker.watch("n1");

        drop(tracker.begin("n2", 7));

        assert!(watch.observed().is_empty());
    }

    #[test]
    fn a_launch_that_ended_before_the_watch_opened_is_not_observed() {
        // Otherwise every node accumulates permanently unreconcilable jobs.
        let tracker = Arc::new(DispatchTracker::default());
        drop(tracker.begin("n1", 7));

        let watch = tracker.watch("n1");

        assert!(watch.observed().is_empty());
    }

    #[test]
    fn overlapping_launches_for_one_job_release_independently() {
        let tracker = Arc::new(DispatchTracker::default());
        let first = tracker.begin("n1", 7);
        let second = tracker.begin("n1", 7);

        drop(first);
        let watch = tracker.watch("n1");
        drop(second);

        assert!(
            watch.observed().contains(&7),
            "the second launch is still on the wire"
        );
        assert!(
            tracker.watch("n1").observed().is_empty(),
            "both launches are done, so nothing is left in flight"
        );
    }
}
