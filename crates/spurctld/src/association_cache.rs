// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use sqlx::PgPool;
use tracing::{info, warn};

use spur_core::accounting::AccountLimits;

struct Snapshot {
    default_qos: HashMap<(String, String), String>,
    default_account: HashMap<String, String>,
    memberships: HashSet<(String, String)>,
    limits: HashMap<(String, String), AccountLimits>,
    allowed_qos: HashMap<(String, String), HashSet<String>>,
    loaded: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AccountMembership {
    CacheUnavailable,
    Member,
    NotMember(Vec<String>),
}

/// Controller-side cache of user/account association defaults. Mirrors
/// `FairshareCache`/`QosCache`: one lock guards one atomic snapshot, so a
/// refresh can never be observed half-applied.
pub struct AssociationCache {
    snapshot: RwLock<Snapshot>,
}

/// Whether `qos` is usable given an association's allow-list and pinned
/// default: unrestricted if neither is set, otherwise it must be a member
/// of the allow-list or match the default exactly.
pub(crate) fn qos_permitted(allowed: &HashSet<String>, default: Option<&str>, qos: &str) -> bool {
    (allowed.is_empty() && default.is_none()) || allowed.contains(qos) || default == Some(qos)
}

impl AssociationCache {
    pub fn new() -> Self {
        Self {
            snapshot: RwLock::new(Snapshot {
                default_qos: HashMap::new(),
                default_account: HashMap::new(),
                memberships: HashSet::new(),
                limits: HashMap::new(),
                allowed_qos: HashMap::new(),
                loaded: false,
            }),
        }
    }

    /// True after a successful load from the accounting database.
    pub fn is_loaded(&self) -> bool {
        self.snapshot.read().loaded
    }

    pub fn account_membership(&self, user: &str, account: &str) -> AccountMembership {
        let snapshot = self.snapshot.read();
        if !snapshot.loaded {
            return AccountMembership::CacheUnavailable;
        }
        if snapshot
            .memberships
            .contains(&(user.to_owned(), account.to_owned()))
        {
            return AccountMembership::Member;
        }

        let mut accounts: Vec<_> = snapshot
            .memberships
            .iter()
            .filter(|(member_user, _)| member_user == user)
            .map(|(_, account)| account.clone())
            .collect();
        accounts.sort_unstable();
        AccountMembership::NotMember(accounts)
    }

    /// Check `qos` is usable by this concrete (user, account) association:
    /// the association must exist, and if it's been scoped to an allow-list
    /// and/or a pinned default QOS, `qos` must be one of them. Membership
    /// and QOS are read under one lock so a concurrent refresh can't
    /// validate one against the other's stale snapshot (see `resolve()`).
    /// Permissive while the cache hasn't loaded — at startup before the
    /// first fetch completes, or while the accounting DB stays
    /// unreachable/erroring.
    pub fn check_qos_authorized(&self, user: &str, account: &str, qos: &str) -> Result<(), String> {
        let snapshot = self.snapshot.read();
        if !snapshot.loaded {
            return Ok(());
        }
        let key = (user.to_owned(), account.to_owned());
        if !snapshot.memberships.contains(&key) {
            return Err(format!(
                "user '{user}' is not associated with account '{account}'"
            ));
        }
        let empty = HashSet::new();
        let allowed = snapshot.allowed_qos.get(&key).unwrap_or(&empty);
        let default = snapshot.default_qos.get(&key);
        if qos_permitted(allowed, default.map(String::as_str), qos) {
            Ok(())
        } else {
            Err(format!(
                "QOS '{qos}' is not permitted for user '{user}' under account '{account}'"
            ))
        }
    }

    /// The effective account (given, or the user's default), that
    /// association's default QOS, and its allow-list (empty if unscoped),
    /// resolved under a single read lock so a concurrent refresh can't
    /// yield a torn old/new combination.
    pub fn resolve(
        &self,
        user: &str,
        account: Option<&str>,
    ) -> (Option<String>, Option<String>, HashSet<String>) {
        let snapshot = self.snapshot.read();
        let effective_account = account
            .filter(|a| !a.is_empty())
            .map(str::to_owned)
            .or_else(|| snapshot.default_account.get(user).cloned())
            .filter(|acct| {
                !snapshot.loaded
                    || snapshot
                        .memberships
                        .contains(&(user.to_owned(), acct.clone()))
            });
        let default_qos = effective_account.as_ref().and_then(|acct| {
            snapshot
                .default_qos
                .get(&(user.to_owned(), acct.clone()))
                .cloned()
        });
        let allowed_qos = effective_account
            .as_ref()
            .and_then(|acct| snapshot.allowed_qos.get(&(user.to_owned(), acct.clone())))
            .cloned()
            .unwrap_or_default();
        (effective_account, default_qos, allowed_qos)
    }

    /// Resource limits for a (user, account) association; unset/unknown fields
    /// default to `None` (limitless), matching `resolve_qos`'s unknown-QoS default.
    pub fn limits(&self, user: &str, account: &str) -> AccountLimits {
        self.snapshot
            .read()
            .limits
            .get(&(user.to_owned(), account.to_owned()))
            .cloned()
            .unwrap_or_default()
    }

    fn replace(
        &self,
        default_qos: HashMap<(String, String), String>,
        default_account: HashMap<String, String>,
        memberships: HashSet<(String, String)>,
        limits: HashMap<(String, String), AccountLimits>,
        allowed_qos: HashMap<(String, String), HashSet<String>>,
    ) {
        *self.snapshot.write() = Snapshot {
            default_qos,
            default_account,
            memberships,
            limits,
            allowed_qos,
            loaded: true,
        };
    }

    /// Test-only seam: populates the cache without a database.
    #[cfg(test)]
    pub(crate) fn insert_association(&self, user: &str, account: &str) {
        let mut snap = self.snapshot.write();
        snap.memberships
            .insert((user.to_owned(), account.to_owned()));
        snap.loaded = true;
    }

    #[cfg(test)]
    pub(crate) fn insert_default_qos(&self, user: &str, account: &str, qos: &str) {
        let mut snap = self.snapshot.write();
        snap.memberships
            .insert((user.to_owned(), account.to_owned()));
        snap.default_qos
            .insert((user.to_owned(), account.to_owned()), qos.to_owned());
        snap.loaded = true;
    }

    #[cfg(test)]
    pub(crate) fn insert_allowed_qos(&self, user: &str, account: &str, qos: &[&str]) {
        let mut snap = self.snapshot.write();
        snap.memberships
            .insert((user.to_owned(), account.to_owned()));
        snap.allowed_qos.insert(
            (user.to_owned(), account.to_owned()),
            qos.iter().map(|q| q.to_string()).collect(),
        );
        snap.loaded = true;
    }

    #[cfg(test)]
    pub(crate) fn insert_default_account(&self, user: &str, account: &str) {
        let mut snap = self.snapshot.write();
        snap.memberships
            .insert((user.to_owned(), account.to_owned()));
        snap.default_account
            .insert(user.to_owned(), account.to_owned());
        snap.loaded = true;
    }

    #[cfg(test)]
    pub(crate) fn insert_limits(&self, user: &str, account: &str, limits: AccountLimits) {
        let mut snap = self.snapshot.write();
        snap.memberships
            .insert((user.to_owned(), account.to_owned()));
        snap.limits
            .insert((user.to_owned(), account.to_owned()), limits);
        snap.loaded = true;
    }

    #[cfg(test)]
    pub(crate) fn set_loaded_without_associations(&self) {
        self.snapshot.write().loaded = true;
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
                Ok(Ok((qos, accounts, memberships, limits, allowed_qos))) => {
                    info!(
                        default_qos = qos.len(),
                        default_account = accounts.len(),
                        memberships = memberships.len(),
                        limits = limits.len(),
                        allowed_qos = allowed_qos.len(),
                        "association cache initialized"
                    );
                    cache.replace(qos, accounts, memberships, limits, allowed_qos);
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
                    Ok(Ok((qos, accounts, memberships, limits, allowed_qos))) => {
                        cache.replace(qos, accounts, memberships, limits, allowed_qos)
                    }
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
        assert_eq!(
            cache.resolve("alice", Some("research")),
            (Some("research".into()), None, HashSet::new())
        );
    }

    #[test]
    fn resolve_unknown_user_with_no_account_given_resolves_nothing() {
        let cache = AssociationCache::new();
        assert_eq!(cache.resolve("alice", None), (None, None, HashSet::new()));
    }

    #[test]
    fn resolve_hit_after_insert() {
        let cache = AssociationCache::new();
        cache.insert_default_qos("alice", "research", "highprio");
        assert_eq!(
            cache.resolve("alice", Some("research")),
            (
                Some("research".into()),
                Some("highprio".into()),
                HashSet::new()
            )
        );
        assert_eq!(
            cache.resolve("alice", Some("other")),
            (None, None, HashSet::new())
        );
    }

    #[test]
    fn resolve_returns_allowed_qos_for_the_effective_account() {
        let cache = AssociationCache::new();
        cache.insert_allowed_qos("alice", "research", &["a", "b"]);
        let (account, _, allowed) = cache.resolve("alice", Some("research"));
        assert_eq!(account.as_deref(), Some("research"));
        assert_eq!(allowed, HashSet::from(["a".to_string(), "b".to_string()]));
    }

    #[test]
    fn replace_swaps_the_whole_snapshot() {
        let cache = AssociationCache::new();
        cache.insert_default_qos("alice", "research", "old");
        cache.replace(
            HashMap::from([(("bob".to_string(), "eng".to_string()), "new".to_string())]),
            HashMap::from([("bob".to_string(), "eng".to_string())]),
            HashSet::from([("bob".to_string(), "eng".to_string())]),
            HashMap::new(),
            HashMap::new(),
        );
        assert_eq!(
            cache.resolve("alice", Some("research")),
            (None, None, HashSet::new())
        );
        assert_eq!(
            cache.resolve("bob", Some("eng")),
            (Some("eng".into()), Some("new".into()), HashSet::new())
        );
        assert_eq!(
            cache.resolve("bob", None),
            (Some("eng".into()), Some("new".into()), HashSet::new())
        );
    }

    #[test]
    fn resolve_uses_given_account_over_default_account() {
        let cache = AssociationCache::new();
        cache.insert_default_account("alice", "research");
        cache.insert_default_qos("alice", "other", "highprio");
        let (account, qos, _) = cache.resolve("alice", Some("other"));
        assert_eq!(account.as_deref(), Some("other"));
        assert_eq!(qos.as_deref(), Some("highprio"));
    }

    #[test]
    fn resolve_falls_back_to_default_account_when_none_given() {
        let cache = AssociationCache::new();
        cache.insert_default_account("alice", "research");
        cache.insert_default_qos("alice", "research", "highprio");
        let (account, qos, _) = cache.resolve("alice", None);
        assert_eq!(account.as_deref(), Some("research"));
        assert_eq!(qos.as_deref(), Some("highprio"));
    }

    #[test]
    fn resolve_reads_account_and_qos_from_the_same_snapshot() {
        let cache = AssociationCache::new();
        cache.insert_default_account("alice", "research");
        cache.replace(
            HashMap::from([(("bob".to_string(), "eng".to_string()), "new".to_string())]),
            HashMap::new(),
            HashSet::from([("bob".to_string(), "eng".to_string())]),
            HashMap::new(),
            HashMap::new(),
        );
        let (account, qos, _) = cache.resolve("alice", None);
        assert_eq!(
            account, None,
            "old default_account must not survive the swap"
        );
        assert_eq!(qos, None);
    }

    #[test]
    fn account_membership_returns_sorted_accounts_for_non_member() {
        let cache = AssociationCache::new();
        assert_eq!(
            cache.account_membership("alice", "research"),
            AccountMembership::CacheUnavailable
        );
        cache.insert_association("alice", "research-z");
        cache.insert_association("alice", "research-a");
        cache.insert_association("bob", "other");
        assert_eq!(
            cache.account_membership("alice", "research-a"),
            AccountMembership::Member
        );
        assert_eq!(
            cache.account_membership("alice", "missing"),
            AccountMembership::NotMember(vec!["research-a".into(), "research-z".into()])
        );
        assert_eq!(
            cache.account_membership("carol", "missing"),
            AccountMembership::NotMember(Vec::new())
        );
    }

    #[test]
    fn check_qos_authorized_unloaded_cache_is_permissive() {
        let cache = AssociationCache::new();
        assert!(cache
            .check_qos_authorized("alice", "research", "anything")
            .is_ok());
    }

    #[test]
    fn check_qos_authorized_matches_pinned_default() {
        let cache = AssociationCache::new();
        cache.insert_default_qos("alice", "research", "highprio");
        assert!(cache
            .check_qos_authorized("alice", "research", "highprio")
            .is_ok());
        assert!(cache
            .check_qos_authorized("alice", "research", "other-teams-qos")
            .is_err());
    }

    #[test]
    fn check_qos_authorized_permissive_when_association_has_no_default_pinned() {
        let cache = AssociationCache::new();
        cache.insert_association("alice", "research");
        assert!(cache
            .check_qos_authorized("alice", "research", "anything")
            .is_ok());
    }

    #[test]
    fn check_qos_authorized_rejects_non_member_account_on_loaded_cache() {
        // A bogus or unaffiliated account must not be a back door: unlike a
        // missing default QOS, a missing *association* is a hard reject.
        let cache = AssociationCache::new();
        cache.insert_default_qos("bob", "eng", "highprio");
        let err = cache
            .check_qos_authorized("alice", "research", "anything")
            .unwrap_err();
        assert!(err.contains("not associated with account 'research'"));
    }

    #[test]
    fn check_qos_authorized_allows_any_member_of_the_allow_list() {
        let cache = AssociationCache::new();
        cache.insert_allowed_qos("alice", "research", &["a", "b", "c"]);
        assert!(cache.check_qos_authorized("alice", "research", "a").is_ok());
        assert!(cache.check_qos_authorized("alice", "research", "c").is_ok());
        assert!(cache
            .check_qos_authorized("alice", "research", "other-teams-qos")
            .is_err());
    }

    #[test]
    fn check_qos_authorized_default_qos_alone_still_scopes_to_one() {
        // Pinning only a default (no explicit allow-list) keeps PR #490's
        // original single-QOS behavior for associations never given a list.
        let cache = AssociationCache::new();
        cache.insert_default_qos("alice", "research", "highprio");
        assert!(cache
            .check_qos_authorized("alice", "research", "highprio")
            .is_ok());
        assert!(cache
            .check_qos_authorized("alice", "research", "other-teams-qos")
            .is_err());
    }

    #[test]
    fn limits_default_to_limitless_for_unknown_association() {
        let cache = AssociationCache::new();
        assert_eq!(cache.limits("alice", "research"), AccountLimits::default());
    }

    #[test]
    fn limits_hit_after_insert() {
        let cache = AssociationCache::new();
        let limits = AccountLimits {
            max_running_jobs: Some(3),
            ..Default::default()
        };
        cache.insert_limits("alice", "research", limits.clone());
        assert_eq!(cache.limits("alice", "research").max_running_jobs, Some(3));
        assert_eq!(cache.limits("alice", "other"), AccountLimits::default());
        assert_eq!(cache.limits("bob", "research"), AccountLimits::default());
    }

    #[test]
    fn replace_swaps_limits_too() {
        let cache = AssociationCache::new();
        cache.insert_limits(
            "alice",
            "research",
            AccountLimits {
                max_running_jobs: Some(1),
                ..Default::default()
            },
        );
        cache.replace(
            HashMap::new(),
            HashMap::new(),
            HashSet::new(),
            HashMap::from([(
                ("bob".to_string(), "eng".to_string()),
                AccountLimits {
                    max_submit_jobs: Some(2),
                    ..Default::default()
                },
            )]),
            HashMap::new(),
        );
        assert_eq!(
            cache.limits("alice", "research"),
            AccountLimits::default(),
            "old limits must not survive the swap"
        );
        assert_eq!(cache.limits("bob", "eng").max_submit_jobs, Some(2));
    }
}
