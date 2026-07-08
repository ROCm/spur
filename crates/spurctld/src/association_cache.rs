// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use sqlx::PgPool;
use tracing::{info, warn};

struct Snapshot {
    default_qos: HashMap<(String, String), String>,
    default_account: HashMap<String, String>,
}

/// Controller-side cache of user/account association defaults. Mirrors
/// `FairshareCache`/`QosCache`: one lock guards one atomic snapshot, so a
/// refresh can never be observed half-applied.
pub struct AssociationCache {
    snapshot: RwLock<Snapshot>,
}

impl AssociationCache {
    pub fn new() -> Self {
        Self {
            snapshot: RwLock::new(Snapshot {
                default_qos: HashMap::new(),
                default_account: HashMap::new(),
            }),
        }
    }

    /// The effective account (given, or the user's default) and that
    /// association's default QOS, resolved under a single read lock so a
    /// concurrent refresh can't yield a torn old/new combination.
    pub fn resolve(&self, user: &str, account: Option<&str>) -> (Option<String>, Option<String>) {
        let snapshot = self.snapshot.read();
        let effective_account = account
            .map(str::to_owned)
            .or_else(|| snapshot.default_account.get(user).cloned());
        let default_qos = effective_account.as_ref().and_then(|acct| {
            snapshot
                .default_qos
                .get(&(user.to_owned(), acct.clone()))
                .cloned()
        });
        (effective_account, default_qos)
    }

    fn replace(
        &self,
        default_qos: HashMap<(String, String), String>,
        default_account: HashMap<String, String>,
    ) {
        *self.snapshot.write() = Snapshot {
            default_qos,
            default_account,
        };
    }

    /// Test-only seam: populates the cache without a database.
    #[cfg(test)]
    pub(crate) fn insert_default_qos(&self, user: &str, account: &str, qos: &str) {
        self.snapshot
            .write()
            .default_qos
            .insert((user.to_owned(), account.to_owned()), qos.to_owned());
    }

    #[cfg(test)]
    pub(crate) fn insert_default_account(&self, user: &str, account: &str) {
        self.snapshot
            .write()
            .default_account
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
    fn resolve_unknown_association_has_no_default_qos() {
        let cache = AssociationCache::new();
        // The given account is echoed back even for an unknown user; only
        // the qos half is None when there's no matching association.
        assert_eq!(
            cache.resolve("alice", Some("research")),
            (Some("research".into()), None)
        );
    }

    #[test]
    fn resolve_unknown_user_with_no_account_given_resolves_nothing() {
        let cache = AssociationCache::new();
        assert_eq!(cache.resolve("alice", None), (None, None));
    }

    #[test]
    fn resolve_hit_after_insert() {
        let cache = AssociationCache::new();
        cache.insert_default_qos("alice", "research", "highprio");
        assert_eq!(
            cache.resolve("alice", Some("research")),
            (Some("research".into()), Some("highprio".into()))
        );
        // Different account, same user: no qos match, but the given
        // account is still echoed back as the resolved one.
        assert_eq!(
            cache.resolve("alice", Some("other")),
            (Some("other".into()), None)
        );
    }

    #[test]
    fn replace_swaps_the_whole_snapshot() {
        let cache = AssociationCache::new();
        cache.insert_default_qos("alice", "research", "old");
        cache.replace(
            HashMap::from([(("bob".to_string(), "eng".to_string()), "new".to_string())]),
            HashMap::from([("bob".to_string(), "eng".to_string())]),
        );
        assert_eq!(
            cache.resolve("alice", Some("research")),
            (Some("research".into()), None)
        );
        assert_eq!(
            cache.resolve("bob", Some("eng")),
            (Some("eng".into()), Some("new".into()))
        );
        assert_eq!(
            cache.resolve("bob", None),
            (Some("eng".into()), Some("new".into()))
        );
    }

    #[test]
    fn resolve_uses_given_account_over_default_account() {
        let cache = AssociationCache::new();
        cache.insert_default_account("alice", "research");
        cache.insert_default_qos("alice", "other", "highprio");
        let (account, qos) = cache.resolve("alice", Some("other"));
        assert_eq!(account.as_deref(), Some("other"));
        assert_eq!(qos.as_deref(), Some("highprio"));
    }

    #[test]
    fn resolve_falls_back_to_default_account_when_none_given() {
        let cache = AssociationCache::new();
        cache.insert_default_account("alice", "research");
        cache.insert_default_qos("alice", "research", "highprio");
        let (account, qos) = cache.resolve("alice", None);
        assert_eq!(account.as_deref(), Some("research"));
        assert_eq!(qos.as_deref(), Some("highprio"));
    }

    #[test]
    fn resolve_reads_account_and_qos_from_the_same_snapshot() {
        // Structural guarantee, not a concurrency test: a stale account must
        // not survive a replace() that wiped it, proving both halves come
        // from the same snapshot rather than two independently-locked maps.
        let cache = AssociationCache::new();
        cache.insert_default_account("alice", "research");
        cache.replace(
            HashMap::from([(("bob".to_string(), "eng".to_string()), "new".to_string())]),
            HashMap::new(), // default_account wiped in the same swap
        );
        let (account, qos) = cache.resolve("alice", None);
        assert_eq!(
            account, None,
            "old default_account must not survive the swap"
        );
        assert_eq!(qos, None);
    }
}
