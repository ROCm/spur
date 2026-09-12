// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Completion rendezvous for supervised steps.

use std::collections::HashMap;
use std::sync::Arc;

use spur_core::step::StepId;
use tokio::sync::{oneshot, Mutex};

pub type JobId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepOutcome {
    pub exit_code: i32,
    pub signal: i32,
}

/// A supervised step is owned by `spurstepd`, not by a child handle the agent
/// can wait on, so the RPC that launched one parks here until the supervisor's
/// completion notification arrives over the agent socket.
#[derive(Clone, Default)]
pub struct StepCompletions {
    waiters: Arc<Mutex<Waiters>>,
}

type Waiters = HashMap<(JobId, StepId), oneshot::Sender<StepOutcome>>;

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
    /// delivered outcome apart from one nobody is listening for.
    pub async fn complete(&self, job_id: JobId, step_id: StepId, outcome: StepOutcome) -> bool {
        let Some(sender) = self.waiters.lock().await.remove(&(job_id, step_id)) else {
            return false;
        };
        sender.send(outcome).is_ok()
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
}
