// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Completion rendezvous for supervised steps.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use spur_core::step::StepId;
use tokio::sync::{oneshot, Mutex};

pub type JobId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepOutcome {
    pub exit_code: i32,
    pub signal: i32,
}

/// Comfortably longer than the controller's re-attach window, so a caller that
/// lost its call while the step was settling still finds the answer waiting.
/// `stepd` keeps a settled step's durable record for the same span, so the
/// memo and the disk it is a fast path for expire together.
pub(crate) const SETTLED_RETENTION: Duration = Duration::from_secs(600);
/// A hard ceiling independent of retention, so step churn cannot grow the memo
/// or the durable records it is a fast path for.
pub(crate) const SETTLED_CAPACITY: usize = 1024;
/// How long a session nothing can finalize is kept for triage. It holds the
/// job's environment, so it cannot be retained on the chance someone looks.
pub(crate) const ORPHAN_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

struct SettledStep {
    key: (JobId, StepId),
    outcome: StepOutcome,
    settled_at: Instant,
}

/// A supervised step is owned by `spurstepd`, not by a child handle the agent
/// can wait on, so the RPC that launched one parks here until the supervisor's
/// completion notification arrives over the agent socket.
#[derive(Clone, Default)]
pub struct StepCompletions {
    waiters: Arc<Mutex<Waiters>>,
    settled: Arc<Mutex<VecDeque<SettledStep>>>,
}

type Waiters = HashMap<(JobId, StepId), oneshot::Sender<StepOutcome>>;

/// Oldest first, so both bounds are satisfied by dropping from the front.
fn evict_settled(settled: &mut VecDeque<SettledStep>, now: Instant) {
    while settled
        .front()
        .is_some_and(|oldest| now.saturating_duration_since(oldest.settled_at) > SETTLED_RETENTION)
    {
        settled.pop_front();
    }
    while settled.len() > SETTLED_CAPACITY {
        settled.pop_front();
    }
}

impl StepCompletions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register before spawning the supervisor: a step that finishes faster
    /// than the launching RPC can park would otherwise report to nobody.
    pub async fn register(&self, job_id: JobId, step_id: StepId) -> oneshot::Receiver<StepOutcome> {
        let (sender, receiver) = oneshot::channel();
        self.waiters.lock().await.insert((job_id, step_id), sender);
        receiver
    }

    /// Returns whether a waiter was still parked, so the caller can tell a
    /// delivered outcome apart from one nobody is listening for. Either way the
    /// outcome is remembered: a caller that arrives late must not be told the
    /// step is unknown when its exit is settled and sitting right here.
    pub async fn complete(&self, job_id: JobId, step_id: StepId, outcome: StepOutcome) -> bool {
        let now = Instant::now();
        let mut settled = self.settled.lock().await;
        settled.retain(|entry| entry.key != (job_id, step_id));
        settled.push_back(SettledStep {
            key: (job_id, step_id),
            outcome,
            settled_at: now,
        });
        evict_settled(&mut settled, now);
        drop(settled);

        let Some(sender) = self.waiters.lock().await.remove(&(job_id, step_id)) else {
            return false;
        };
        sender.send(outcome).is_ok()
    }

    /// The outcome of a step that already settled, for a caller whose
    /// rendezvous is gone — the agent restarted, or nobody was parked when the
    /// supervisor's exit was consumed.
    pub async fn settled(&self, job_id: JobId, step_id: StepId) -> Option<StepOutcome> {
        let settled = self.settled.lock().await;
        settled
            .iter()
            .rev()
            .find(|entry| entry.key == (job_id, step_id))
            .map(|entry| entry.outcome)
    }

    /// Take over a step whose waiter is gone, for a client reconnecting after it
    /// lost the call that launched it. Refuses while a waiter is still parked.
    pub async fn reregister(
        &self,
        job_id: JobId,
        step_id: StepId,
    ) -> Option<oneshot::Receiver<StepOutcome>> {
        let mut waiters = self.waiters.lock().await;
        if waiters
            .get(&(job_id, step_id))
            .is_some_and(|parked| !parked.is_closed())
        {
            return None;
        }
        let (sender, receiver) = oneshot::channel();
        waiters.insert((job_id, step_id), sender);
        Some(receiver)
    }

    pub async fn deregister(&self, job_id: JobId, step_id: StepId) {
        self.waiters.lock().await.remove(&(job_id, step_id));
    }

    #[cfg(test)]
    pub async fn parked(&self) -> usize {
        self.waiters.lock().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUTCOME: StepOutcome = StepOutcome {
        exit_code: 7,
        signal: 0,
    };

    #[tokio::test]
    async fn a_registered_step_receives_its_outcome() {
        let completions = StepCompletions::new();
        let waiter = completions.register(42, 0).await;

        assert!(completions.complete(42, 0, OUTCOME).await);

        assert_eq!(waiter.await.expect("outcome delivered"), OUTCOME);
    }

    #[tokio::test]
    async fn completing_an_unregistered_step_reports_no_waiter() {
        let completions = StepCompletions::new();

        assert!(!completions.complete(42, 0, OUTCOME).await);
    }

    #[tokio::test]
    async fn a_step_id_is_scoped_to_its_job() {
        let completions = StepCompletions::new();
        let waiter = completions.register(42, 0).await;

        assert!(!completions.complete(43, 0, OUTCOME).await);

        assert!(completions.complete(42, 0, OUTCOME).await);
        assert_eq!(waiter.await.expect("outcome delivered"), OUTCOME);
    }

    #[tokio::test]
    async fn two_steps_of_one_job_settle_independently() {
        let completions = StepCompletions::new();
        let first = completions.register(42, 0).await;
        let second = completions.register(42, 1).await;

        assert!(completions.complete(42, 1, OUTCOME).await);

        assert_eq!(second.await.expect("outcome delivered"), OUTCOME);
        assert_eq!(completions.parked().await, 1);
        drop(first);
    }

    #[tokio::test]
    async fn deregistering_abandons_the_waiter() {
        let completions = StepCompletions::new();
        let waiter = completions.register(42, 0).await;

        completions.deregister(42, 0).await;

        assert!(!completions.complete(42, 0, OUTCOME).await);
        assert!(waiter.await.is_err(), "abandoned waiter must not resolve");
    }

    #[tokio::test]
    async fn a_dropped_waiter_leaves_the_slot_reclaimable() {
        let completions = StepCompletions::new();
        drop(completions.register(42, 0).await);

        assert!(
            !completions.complete(42, 0, OUTCOME).await,
            "a dropped receiver must not read as a delivered outcome"
        );
        assert_eq!(completions.parked().await, 0);
    }

    #[tokio::test]
    async fn a_reconnecting_client_takes_over_an_abandoned_slot() {
        let completions = StepCompletions::new();
        drop(completions.register(42, 0).await);

        let mut resumed = completions
            .reregister(42, 0)
            .await
            .expect("an abandoned slot is available");

        assert!(completions.complete(42, 0, OUTCOME).await);
        assert_eq!(resumed.try_recv().expect("outcome delivered"), OUTCOME);
    }

    #[tokio::test]
    async fn reregistering_never_displaces_a_live_waiter() {
        let completions = StepCompletions::new();
        let mut original = completions.register(42, 0).await;

        assert!(
            completions.reregister(42, 0).await.is_none(),
            "a parked waiter must keep its slot"
        );

        assert!(completions.complete(42, 0, OUTCOME).await);
        assert_eq!(original.try_recv().expect("outcome delivered"), OUTCOME);
    }

    #[tokio::test]
    async fn a_step_nobody_ever_awaited_can_be_registered_by_a_reconnect() {
        let completions = StepCompletions::new();

        assert!(completions.reregister(42, 0).await.is_some());
    }

    #[tokio::test]
    async fn an_outcome_nobody_was_parked_for_is_still_answerable() {
        let completions = StepCompletions::new();

        assert!(!completions.complete(42, 0, OUTCOME).await);

        assert_eq!(completions.settled(42, 0).await, Some(OUTCOME));
    }

    #[tokio::test]
    async fn a_delivered_outcome_stays_answerable_for_a_reconnect() {
        let completions = StepCompletions::new();
        let waiter = completions.register(42, 0).await;

        assert!(completions.complete(42, 0, OUTCOME).await);
        assert_eq!(waiter.await.expect("outcome delivered"), OUTCOME);

        assert_eq!(completions.settled(42, 0).await, Some(OUTCOME));
    }

    #[tokio::test]
    async fn a_step_that_never_settled_is_not_answerable() {
        let completions = StepCompletions::new();
        completions.register(42, 0).await;

        assert_eq!(completions.settled(42, 0).await, None);
    }

    #[tokio::test]
    async fn a_rerun_of_a_key_replaces_the_remembered_outcome() {
        let completions = StepCompletions::new();
        completions.complete(42, 0, OUTCOME).await;

        let rerun = StepOutcome {
            exit_code: 0,
            signal: 0,
        };
        completions.complete(42, 0, rerun).await;

        assert_eq!(completions.settled(42, 0).await, Some(rerun));
    }

    fn settled_at(key: (JobId, StepId), settled_at: Instant) -> SettledStep {
        SettledStep {
            key,
            outcome: OUTCOME,
            settled_at,
        }
    }

    #[test]
    fn eviction_caps_the_number_of_remembered_outcomes() {
        let now = Instant::now();
        let mut settled: VecDeque<_> = (0..SETTLED_CAPACITY as u32 + 10)
            .map(|step_id| settled_at((42, step_id), now))
            .collect();

        evict_settled(&mut settled, now);

        assert_eq!(settled.len(), SETTLED_CAPACITY);
        assert_eq!(settled.front().map(|entry| entry.key), Some((42, 10)));
    }

    #[test]
    fn eviction_drops_outcomes_past_their_retention() {
        let now = Instant::now();
        let mut settled = VecDeque::from(vec![
            settled_at((42, 0), now - SETTLED_RETENTION - Duration::from_secs(1)),
            settled_at((42, 1), now),
        ]);

        evict_settled(&mut settled, now);

        assert_eq!(settled.len(), 1);
        assert_eq!(settled.front().map(|entry| entry.key), Some((42, 1)));
    }
}
