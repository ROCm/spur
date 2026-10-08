// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Bounded in-memory replay cache for native user-RPC credentials.
//!
//! Entries live until the credential's `expires_at` (plus skew). Insert is
//! atomic with the lookup so two concurrent presentations of the same nonce
//! cannot both succeed.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::native_cred::{CredentialError, UserRpcCredential, NONCE_LEN};

pub const DEFAULT_CAPACITY: usize = 65_536;

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct ReplayKey {
    cluster_id: String,
    audience: String,
    epoch: u64,
    key_id: String,
    nonce: [u8; NONCE_LEN],
}

impl ReplayKey {
    pub(crate) fn new(
        cluster_id: impl Into<String>,
        audience: impl Into<String>,
        epoch: u64,
        key_id: impl Into<String>,
        nonce: [u8; NONCE_LEN],
    ) -> Self {
        Self {
            cluster_id: cluster_id.into(),
            audience: audience.into(),
            epoch,
            key_id: key_id.into(),
            nonce,
        }
    }
}

struct Inner {
    seen: HashMap<ReplayKey, u64>,
    capacity: usize,
}

/// Shared by every clone of a [`crate::auth::NativeAuth`] verifier.
pub struct ReplayCache {
    inner: Mutex<Inner>,
}

impl ReplayCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                seen: HashMap::new(),
                capacity: capacity.max(1),
            }),
        }
    }

    /// Reject a nonce already accepted for this audience. Otherwise record it
    /// until `expires_at + skew_secs`.
    pub fn check_and_insert(
        &self,
        cred: &UserRpcCredential,
        now: u64,
        skew_secs: u64,
    ) -> Result<(), CredentialError> {
        self.insert(
            ReplayKey::new(
                cred.cluster_id.clone(),
                cred.audience.clone(),
                cred.audience_epoch,
                cred.key_id.clone(),
                cred.nonce,
            ),
            cred.expires_at.saturating_add(skew_secs),
            now,
        )
    }

    /// Record a nonce that is not a user-RPC credential (peer forwarding).
    pub(crate) fn check_and_insert_raw(
        &self,
        key: ReplayKey,
        retain_until: u64,
        now: u64,
    ) -> Result<(), CredentialError> {
        self.insert(key, retain_until, now)
    }

    fn insert(&self, key: ReplayKey, retain_until: u64, now: u64) -> Result<(), CredentialError> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.evict_expired(now);
        if inner.seen.contains_key(&key) {
            return Err(CredentialError::Replay);
        }
        if inner.seen.len() >= inner.capacity {
            inner.evict_earliest();
        }
        inner.seen.insert(key, retain_until);
        Ok(())
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .seen
            .len()
    }
}

impl Inner {
    fn evict_expired(&mut self, now: u64) {
        self.seen.retain(|_, exp| *exp > now);
    }

    fn evict_earliest(&mut self) {
        let Some(victim) = self
            .seen
            .iter()
            .min_by_key(|(_, exp)| *exp)
            .map(|(k, _)| k.clone())
        else {
            return;
        };
        self.seen.remove(&victim);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_cred::UserRpcCredential;

    fn cred(nonce: u8, audience: &str, epoch: u64, expires_at: u64) -> UserRpcCredential {
        UserRpcCredential {
            cluster_id: "c".into(),
            issuer_host: "login".into(),
            audience: audience.into(),
            audience_epoch: epoch,
            user: "alice".into(),
            uid: 1,
            gid: 1,
            issued_at: 10,
            expires_at,
            nonce: [nonce; NONCE_LEN],
            key_id: "k1".into(),
        }
    }

    #[test]
    fn second_presentation_of_the_same_nonce_is_replay() {
        let cache = ReplayCache::new(8);
        let c = cred(1, "aud-a", 7, 100);
        cache.check_and_insert(&c, 50, 5).unwrap();
        assert!(matches!(
            cache.check_and_insert(&c, 50, 5),
            Err(CredentialError::Replay)
        ));
    }

    #[test]
    fn same_nonce_is_independent_per_audience() {
        let cache = ReplayCache::new(8);
        cache
            .check_and_insert(&cred(1, "aud-a", 7, 100), 50, 5)
            .unwrap();
        cache
            .check_and_insert(&cred(1, "aud-b", 7, 100), 50, 5)
            .unwrap();
    }

    #[test]
    fn expired_entries_are_forgotten() {
        let cache = ReplayCache::new(8);
        let c = cred(1, "aud-a", 7, 100);
        cache.check_and_insert(&c, 50, 5).unwrap();
        // retain_until = 105; after that the slot is free.
        cache.check_and_insert(&c, 106, 5).unwrap();
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn capacity_evicts_the_soonest_expiry() {
        let cache = ReplayCache::new(2);
        cache.check_and_insert(&cred(1, "a", 1, 20), 10, 0).unwrap();
        cache.check_and_insert(&cred(2, "a", 1, 50), 10, 0).unwrap();
        cache.check_and_insert(&cred(3, "a", 1, 80), 10, 0).unwrap();
        assert_eq!(cache.len(), 2);
        // nonce 1 (expires 20) was dropped; presenting it again succeeds.
        cache.check_and_insert(&cred(1, "a", 1, 90), 10, 0).unwrap();
    }
}
