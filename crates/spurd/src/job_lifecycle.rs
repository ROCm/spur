// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Per-job-id serialization of the phases that create and destroy a job's state.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

type Entry = Arc<AsyncMutex<()>>;
type Registry = Arc<Mutex<HashMap<u32, Entry>>>;

/// Serializes setup and teardown for a given job id. A job's cgroup, spool dir and rootfs
/// all derive from its id and the controller reuses that id on re-dispatch, so without
/// this a launch and the previous run's teardown each act on the other's files.
#[derive(Clone, Default)]
pub(crate) struct JobLifecycle {
    entries: Registry,
}

impl JobLifecycle {
    pub(crate) async fn acquire(&self, job_id: u32) -> JobLifecycleGuard {
        let entry = lock_registry(&self.entries)
            .entry(job_id)
            .or_default()
            .clone();
        let held = entry.lock_owned().await;
        JobLifecycleGuard {
            job_id,
            entries: Arc::clone(&self.entries),
            held: Some(held),
        }
    }
}

/// Poisoning means some holder panicked, not that the map is torn: it is only ever
/// inserted into and removed from. Refusing it here would wedge every later launch.
fn lock_registry(entries: &Registry) -> MutexGuard<'_, HashMap<u32, Entry>> {
    entries.lock().unwrap_or_else(|e| e.into_inner())
}

/// Held for as long as the caller owns the job id's state. Nothing else may set up or
/// tear down that id until it drops.
#[must_use = "the lifecycle is serialized only while this guard is held"]
pub(crate) struct JobLifecycleGuard {
    job_id: u32,
    entries: Registry,
    held: Option<OwnedMutexGuard<()>>,
}

impl Drop for JobLifecycleGuard {
    fn drop(&mut self) {
        // Release first so this guard's own reference is not counted below.
        self.held.take();
        let mut entries = lock_registry(&self.entries);
        // The sole remaining reference is the map's, so no one is waiting on this id
        // and the entry can go rather than accumulating one per job the node ever ran.
        if entries
            .get(&self.job_id)
            .is_some_and(|e| Arc::strong_count(e) == 1)
        {
            entries.remove(&self.job_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::JobLifecycle;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn a_second_acquire_of_the_same_id_waits_for_the_first() {
        let lifecycle = JobLifecycle::default();
        let first = lifecycle.acquire(7).await;

        let second_ran = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn({
            let (lifecycle, ran) = (lifecycle.clone(), Arc::clone(&second_ran));
            async move {
                let _guard = lifecycle.acquire(7).await;
                ran.store(true, Ordering::SeqCst);
            }
        });

        tokio::task::yield_now().await;
        assert!(
            !second_ran.load(Ordering::SeqCst),
            "teardown must not run while a launch of the same id holds the id"
        );

        drop(first);
        task.await.expect("the waiter runs once the id is free");
        assert!(second_ran.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn different_ids_do_not_block_each_other() {
        let lifecycle = JobLifecycle::default();
        let _held = lifecycle.acquire(1).await;
        // Would hang if the registry serialized across ids rather than per id.
        let _other = lifecycle.acquire(2).await;
    }

    #[tokio::test]
    async fn a_released_id_leaves_nothing_behind() {
        let lifecycle = JobLifecycle::default();
        for job_id in 0..64 {
            drop(lifecycle.acquire(job_id).await);
        }
        assert!(
            super::lock_registry(&lifecycle.entries).is_empty(),
            "a node that ran many jobs must not keep an entry per id"
        );
    }

    #[tokio::test]
    async fn an_id_with_a_waiter_keeps_its_entry() {
        let lifecycle = JobLifecycle::default();
        let held = lifecycle.acquire(9).await;
        let waiter = tokio::spawn({
            let lifecycle = lifecycle.clone();
            async move { lifecycle.acquire(9).await }
        });
        tokio::task::yield_now().await;

        drop(held);
        let still_held = waiter.await.expect("the waiter acquires");
        assert_eq!(
            super::lock_registry(&lifecycle.entries).len(),
            1,
            "pruning must not drop an entry another holder is using"
        );
        drop(still_held);
        assert!(super::lock_registry(&lifecycle.entries).is_empty());
    }
}
