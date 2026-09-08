// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use anyhow::{ensure, Context, Result};
use nix::unistd::User;
use tokio::sync::Semaphore;

const MAX_LOOKUPS: usize = 4;

#[derive(Clone)]
pub(crate) struct IdentityResolver {
    slots: Arc<Semaphore>,
}

impl Default for IdentityResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl IdentityResolver {
    pub(crate) fn new() -> Self {
        Self {
            slots: Arc::new(Semaphore::new(MAX_LOOKUPS)),
        }
    }

    pub(crate) async fn resolve(&self, user: String) -> Result<u32> {
        self.bounded_lookup(move || {
            let record = User::from_name(&user)
                .context("NSS identity lookup failed")?
                .context("SSH user does not exist")?;
            validate_identity(&user, record)
        })
        .await
    }

    async fn bounded_lookup<F>(&self, lookup: F) -> Result<u32>
    where
        F: FnOnce() -> Result<u32> + Send + 'static,
    {
        let permit = self
            .slots
            .clone()
            .try_acquire_owned()
            .context("SSH identity resolver is busy")?;
        tokio::task::spawn_blocking(move || {
            // NSS cannot be cancelled: retain capacity even if its async waiter is dropped.
            let _permit = permit;
            lookup()
        })
        .await
        .context("SSH identity lookup task failed")?
    }
}

fn validate_identity(user: &str, record: User) -> Result<u32> {
    ensure!(record.name == user, "NSS returned a different SSH user");
    let uid = record.uid.as_raw();
    ensure!(uid != 0, "SSH admission refuses uid 0");
    Ok(uid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{mpsc, Barrier};
    use tokio::sync::oneshot;

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_waiters_keep_all_four_slots_until_blocking_work_finishes() {
        let resolver = IdentityResolver::new();
        let mut waiters = Vec::new();
        let mut releases = Vec::new();
        let barrier = Arc::new(Barrier::new(MAX_LOOKUPS + 1));

        for _ in 0..MAX_LOOKUPS {
            let clone = resolver.clone();
            let barrier = barrier.clone();
            let (started_tx, started_rx) = oneshot::channel();
            let (release_tx, release_rx) = mpsc::channel();
            releases.push(release_tx);
            waiters.push(tokio::spawn(async move {
                clone
                    .bounded_lookup(move || {
                        started_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        barrier.wait();
                        Ok(1000)
                    })
                    .await
            }));
            started_rx.await.unwrap();
        }

        let (responsive_tx, responsive_rx) = oneshot::channel();
        tokio::spawn(async move { responsive_tx.send(()).unwrap() });
        responsive_rx.await.unwrap();
        let busy_before_cancel = resolver.bounded_lookup(|| Ok(1001)).await;

        for waiter in waiters {
            waiter.abort();
            assert!(waiter.await.unwrap_err().is_cancelled());
        }
        let held_after_cancel = resolver.slots.available_permits();
        let busy_after_cancel = resolver.clone().bounded_lookup(|| Ok(1001)).await;

        for release in releases {
            release.send(()).unwrap();
        }
        let (finished_tx, finished_rx) = oneshot::channel();
        let barrier_waiter = tokio::task::spawn_blocking(move || {
            barrier.wait();
            finished_tx.send(()).unwrap();
        });
        finished_rx.await.unwrap();
        barrier_waiter.await.unwrap();
        // Acquiring every permit also synchronizes with the closures' final destructors.
        let all_slots = resolver
            .slots
            .clone()
            .acquire_many_owned(MAX_LOOKUPS as u32)
            .await
            .unwrap();
        drop(all_slots);

        assert!(busy_before_cancel.is_err());
        assert_eq!(held_after_cancel, 0);
        assert!(busy_after_cancel.is_err());
        assert_eq!(resolver.bounded_lookup(|| Ok(1001)).await.unwrap(), 1001);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn errors_and_panics_release_capacity() {
        let resolver = IdentityResolver::default();
        assert!(resolver
            .bounded_lookup(|| anyhow::bail!("lookup failed"))
            .await
            .is_err());
        assert_eq!(resolver.slots.available_permits(), MAX_LOOKUPS);
        assert!(resolver
            .bounded_lookup(|| panic!("lookup panicked"))
            .await
            .is_err());
        assert_eq!(resolver.slots.available_permits(), MAX_LOOKUPS);
        assert_eq!(resolver.bounded_lookup(|| Ok(1000)).await.unwrap(), 1000);
    }

    fn user(name: &str, uid: u32) -> User {
        User {
            name: name.into(),
            passwd: Default::default(),
            uid: nix::unistd::Uid::from_raw(uid),
            gid: nix::unistd::Gid::from_raw(1000),
            gecos: Default::default(),
            dir: "/home/alice".into(),
            shell: "/bin/sh".into(),
        }
    }

    #[test]
    fn requires_exact_name_and_non_root_uid() {
        assert_eq!(
            validate_identity("alice", user("alice", 1000)).unwrap(),
            1000
        );
        assert!(validate_identity("alice", user("bob", 1000)).is_err());
        assert!(validate_identity("alice", user("Alice", 1000)).is_err());
        assert!(validate_identity("alice", user("alice", 0)).is_err());
    }
}
