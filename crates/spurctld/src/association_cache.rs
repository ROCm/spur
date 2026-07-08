// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use sqlx::PgPool;
use tracing::{info, warn};

/// Controller-side cache of user/account association defaults. Mirrors
/// `FairshareCache`/`QosCache`.
pub struct AssociationCache {
    default_qos: RwLock<HashMap<(String, String), String>>,
    default_account: RwLock<HashMap<String, String>>,
}

impl AssociationCache {
    pub fn new() -> Self {
        Self {
            default_qos: RwLock::new(HashMap::new()),
            default_account: RwLock::new(HashMap::new()),
        }
    }

    /// The default QOS for a user's association with `account`, if set.
    pub fn default_qos(&self, user: &str, account: &str) -> Option<String> {
        let key = (user.to_owned(), account.to_owned());
        self.default_qos.read().get(&key).cloned()
    }

    /// The user's default account, if one is set (mirrors Slurm's
    /// `DefaultAccount`, used when a job omits `--account`).
    pub fn default_account(&self, user: &str) -> Option<String> {
        self.default_account.read().get(user).cloned()
    }

    fn replace(
        &self,
        default_qos: HashMap<(String, String), String>,
        default_account: HashMap<String, String>,
    ) {
        *self.default_qos.write() = default_qos;
        *self.default_account.write() = default_account;
    }

    /// Test-only seam: populates the cache without a database.
    #[cfg(test)]
    pub(crate) fn insert_default_qos(&self, user: &str, account: &str, qos: &str) {
        self.default_qos
            .write()
            .insert((user.to_owned(), account.to_owned()), qos.to_owned());
    }

    #[cfg(test)]
    pub(crate) fn insert_default_account(&self, user: &str, account: &str) {
        self.default_account
            .write()
            .insert(user.to_owned(), account.to_owned());
    }

    pub fn spawn_refresh_loop(self: &Arc<Self>, pool: PgPool, refresh_interval_secs: u64) {
        let cache = Arc::clone(self);
        let interval = Duration::from_secs(refresh_interval_secs.max(10));

        tokio::spawn(async move {
            match tokio::time::timeout(
                Duration::from_secs(5),
                crate::accounting::association_maps(&pool),
            )
            .await
            {
                Ok(Ok((qos, accounts))) => {
                    info!(
                        default_qos = qos.len(),
                        default_account = accounts.len(),
                        "association cache initialized"
                    );
                    cache.replace(qos, accounts);
                }
                Ok(Err(e)) => {
                    warn!(error = %e, "initial association fetch failed, will retry in background");
                }
                Err(_) => {
                    warn!("initial association fetch timed out, will retry in background");
                }
            }

            loop {
                tokio::time::sleep(interval).await;

                match tokio::time::timeout(
                    Duration::from_secs(10),
                    crate::accounting::association_maps(&pool),
                )
                .await
                {
                    Ok(Ok((qos, accounts))) => cache.replace(qos, accounts),
                    Ok(Err(e)) => {
                        warn!(error = %e, "association refresh failed, retaining stale data")
                    }
                    Err(_) => warn!("association refresh timed out, retaining stale data"),
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_qos_missing_returns_none() {
        let cache = AssociationCache::new();
        assert_eq!(cache.default_qos("alice", "research"), None);
    }

    #[test]
    fn default_qos_hit_after_insert() {
        let cache = AssociationCache::new();
        cache.insert_default_qos("alice", "research", "highprio");
        assert_eq!(
            cache.default_qos("alice", "research"),
            Some("highprio".into())
        );
        // Different account, same user: no match.
        assert_eq!(cache.default_qos("alice", "other"), None);
    }

    #[test]
    fn default_account_missing_returns_none() {
        let cache = AssociationCache::new();
        assert_eq!(cache.default_account("alice"), None);
    }

    #[test]
    fn default_account_hit_after_insert() {
        let cache = AssociationCache::new();
        cache.insert_default_account("alice", "research");
        assert_eq!(cache.default_account("alice"), Some("research".into()));
    }

    #[test]
    fn replace_swaps_both_maps_atomically_from_the_readers_perspective() {
        let cache = AssociationCache::new();
        cache.insert_default_qos("alice", "research", "old");
        cache.replace(
            HashMap::from([(("bob".to_string(), "eng".to_string()), "new".to_string())]),
            HashMap::from([("bob".to_string(), "eng".to_string())]),
        );
        assert_eq!(cache.default_qos("alice", "research"), None);
        assert_eq!(cache.default_qos("bob", "eng"), Some("new".into()));
        assert_eq!(cache.default_account("bob"), Some("eng".into()));
    }
}
