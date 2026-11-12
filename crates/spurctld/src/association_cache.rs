// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use sqlx::PgPool;
use tokio::sync::Notify;
use tracing::{info, warn};

use spur_core::accounting::AccountLimits;

#[derive(Default)]
struct Snapshot {
    default_qos: HashMap<(String, String), String>,
    default_account: HashMap<String, String>,
    memberships: HashSet<(String, String)>,
    limits: HashMap<(String, String), AccountLimits>,
    allowed_qos: HashMap<(String, String), HashSet<String>>,
    admin_level: HashMap<String, String>,
    coordinators: HashSet<(String, String)>,
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
    kick: Notify,
}

/// Whether `qos` is usable. `None` means no allow-list applies anywhere above
/// the user, so any QOS is allowed; the default only fills in a job that did
/// not name one. Once a list applies, the QOS has to be on it, and an empty
/// list allows nothing.
pub(crate) fn qos_permitted(allowed: Option<&HashSet<String>>, qos: &str) -> bool {
    allowed.is_none_or(|list| list.contains(qos))
}

/// Direct coordinator assignments plus every account under them. Slurm gives
/// a coordinator of an account the sub-accounts as well, and a cycle in the
/// parent links must not walk forever.
pub(crate) fn with_sub_accounts(
    direct: impl IntoIterator<Item = (String, String)>,
    parent_of: &HashMap<String, String>,
) -> HashSet<(String, String)> {
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    for (child, parent) in parent_of {
        if !parent.is_empty() {
            children
                .entry(parent.as_str())
                .or_default()
                .push(child.as_str());
        }
    }
    let mut out = HashSet::new();
    for (user, account) in direct {
        let mut stack = vec![account];
        let mut seen = HashSet::new();
        while let Some(name) = stack.pop() {
            if !seen.insert(name.clone()) {
                continue;
            }
            if let Some(kids) = children.get(name.as_str()) {
                stack.extend(kids.iter().copied().map(str::to_owned));
            }
            out.insert((user.clone(), name));
        }
    }
    out
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
                admin_level: HashMap::new(),
                coordinators: HashSet::new(),
                loaded: false,
            }),
            kick: Notify::new(),
        }
    }

    /// True after a successful load from the accounting database.
    pub fn is_loaded(&self) -> bool {
        self.snapshot.read().loaded
    }

    pub fn is_admin(&self, user: &str) -> bool {
        let snapshot = self.snapshot.read();
        snapshot.loaded
            && snapshot
                .admin_level
                .get(user)
                .is_some_and(|lvl| crate::accounting::admin_level_is_admin(lvl))
    }

    /// Whether `user` holds Operator or Administrator accounting level. Fails closed
    /// when the cache has not loaded.
    pub fn is_operator(&self, user: &str) -> bool {
        matches!(
            self.admin_level(user).as_deref(),
            Some("Operator") | Some("Administrator")
        )
    }

    pub fn admin_level(&self, user: &str) -> Option<String> {
        let snapshot = self.snapshot.read();
        if !snapshot.loaded {
            return None;
        }
        snapshot.admin_level.get(user).cloned()
    }

    /// Accounts `user` belongs to. `None` when the cache has not loaded, so a
    /// caller can hide account-scoped records instead of treating everyone as
    /// a member.
    pub fn accounts_for(&self, user: &str) -> Option<Vec<String>> {
        let snapshot = self.snapshot.read();
        if !snapshot.loaded {
            return None;
        }
        let mut accounts: Vec<String> = snapshot
            .memberships
            .iter()
            .filter(|(member, _)| member == user)
            .map(|(_, account)| account.clone())
            .collect();
        accounts.sort_unstable();
        Some(accounts)
    }

    /// Accounts `user` coordinates, including sub-accounts of a direct
    /// assignment. `None` when the cache has not loaded.
    pub fn coordinated_accounts(&self, user: &str) -> Option<Vec<String>> {
        let snapshot = self.snapshot.read();
        if !snapshot.loaded {
            return None;
        }
        let mut accounts: Vec<String> = snapshot
            .coordinators
            .iter()
            .filter(|(coordinator, _)| coordinator == user)
            .map(|(_, account)| account.clone())
            .collect();
        accounts.sort_unstable();
        Some(accounts)
    }

    /// Whether `user` is associated with `account`. An unloaded cache reports `CacheUnavailable`
    /// rather than guessing; see `validate_user_account` in `cluster.rs` for how callers must treat that.
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
    /// the association must exist, and if an allow-list applies, `qos` must
    /// be on it. A pinned default does not restrict the list. Membership
    /// and the list are read under one lock so a concurrent refresh can't
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
        if qos_permitted(snapshot.allowed_qos.get(&key), qos) {
            Ok(())
        } else {
            Err(format!(
                "QOS '{qos}' is not permitted for user '{user}' under account '{account}'"
            ))
        }
    }

    /// The effective account (given, or the user's default), that
    /// association's default QOS, and its allow-list. `None` for the list
    /// means none applies (any QOS is allowed); `Some` of an empty set allows
    /// no QOS. Resolved under a single read lock so a concurrent refresh can't
    /// yield a torn combination.
    pub fn resolve(
        &self,
        user: &str,
        account: Option<&str>,
    ) -> (Option<String>, Option<String>, Option<HashSet<String>>) {
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
            .cloned();
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

    /// Every cached association as (account, user) with its limits, sorted. Built
    /// from memberships rather than from the limits map so an association with no
    /// limits set still appears — the operator view reports what exists, and an
    /// association with nothing capped is a finding in itself.
    pub fn all(&self) -> Vec<(String, String, AccountLimits)> {
        let snap = self.snapshot.read();
        let mut all: Vec<(String, String, AccountLimits)> = snap
            .memberships
            .iter()
            .map(|(user, account)| {
                let limits = snap
                    .limits
                    .get(&(user.clone(), account.clone()))
                    .cloned()
                    .unwrap_or_default();
                (account.clone(), user.clone(), limits)
            })
            .collect();
        all.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        all
    }

    fn replace(&self, snapshot: Snapshot) {
        *self.snapshot.write() = snapshot;
    }

    /// Test-only seam: returns the cache to its pre-first-fetch state, as a freshly
    /// started controller sees it while its predecessor's queue is already durable.
    #[cfg(test)]
    pub(crate) fn reset(&self) {
        *self.snapshot.write() = Snapshot::default();
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
    pub(crate) fn insert_coordinator(&self, user: &str, account: &str) {
        let mut snap = self.snapshot.write();
        snap.coordinators
            .insert((user.to_owned(), account.to_owned()));
        snap.loaded = true;
    }

    #[cfg(test)]
    pub(crate) fn insert_admin_level(&self, user: &str, level: &str) {
        let mut snap = self.snapshot.write();
        snap.admin_level.insert(user.to_owned(), level.to_owned());
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

    /// Wake the refresh loop so role bindings apply without waiting for the interval.
    pub fn kick_refresh(&self) {
        self.kick.notify_waiters();
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
                Ok(Ok((
                    qos,
                    accounts,
                    memberships,
                    limits,
                    allowed_qos,
                    admin_level,
                    coordinators,
                ))) => {
                    info!(
                        default_qos = qos.len(),
                        default_account = accounts.len(),
                        memberships = memberships.len(),
                        limits = limits.len(),
                        allowed_qos = allowed_qos.len(),
                        admin_level = admin_level.len(),
                        coordinators = coordinators.len(),
                        "association cache initialized"
                    );
                    cache.replace(Snapshot {
                        default_qos: qos,
                        default_account: accounts,
                        memberships,
                        limits,
                        allowed_qos,
                        admin_level,
                        coordinators,
                        loaded: true,
                    });
                }
                Ok(Err(e)) => {
                    warn!(error = %e, "initial association fetch failed, will retry in background");
                }
                Err(_) => {
                    warn!("initial association fetch timed out, will retry in background");
                }
            }

            loop {
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {}
                    _ = cache.kick.notified() => {}
                }

                match tokio::time::timeout(
                    Duration::from_secs(10),
                    crate::accounting::association_maps(&pool),
                )
                .await
                {
                    Ok(Ok((
                        qos,
                        accounts,
                        memberships,
                        limits,
                        allowed_qos,
                        admin_level,
                        coordinators,
                    ))) => cache.replace(Snapshot {
                        default_qos: qos,
                        default_account: accounts,
                        memberships,
                        limits,
                        allowed_qos,
                        admin_level,
                        coordinators,
                        loaded: true,
                    }),
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
            (Some("research".into()), None, None)
        );
    }

    #[test]
    fn accounts_for_is_none_until_the_cache_loads() {
        let cache = AssociationCache::new();
        assert!(cache.accounts_for("alice").is_none());

        cache.set_loaded_without_associations();
        assert_eq!(cache.accounts_for("alice").as_deref(), Some([].as_slice()));

        cache.insert_association("alice", "physics");
        cache.insert_association("alice", "ml");
        assert_eq!(
            cache.accounts_for("alice").as_deref(),
            Some(["ml".to_string(), "physics".to_string()].as_slice())
        );
        assert_eq!(cache.accounts_for("bob").as_deref(), Some([].as_slice()));
    }

    #[test]
    fn is_admin_false_on_cold_cache() {
        let cache = AssociationCache::new();
        assert!(!cache.is_admin("carol"));
        assert!(!cache.is_operator("carol"));
    }

    /// Every spelling Slurm's parser maps to super-user must resolve here, or a row stored as
    /// `Administrator` (what `sacctmgr show user` prints) would confer nothing.
    #[test]
    fn is_admin_accepts_every_slurm_admin_spelling() {
        let cache = AssociationCache::new();
        for (user, level) in [
            ("carol", "admin"),
            ("frank", "Admin"),
            ("grace", "Administrator"),
            ("heidi", "SuperUser"),
        ] {
            cache.insert_admin_level(user, level);
            assert!(cache.is_admin(user), "level {level:?} must be admin");
        }

        cache.insert_admin_level("dave", "Operator");
        assert!(!cache.is_admin("dave"));
        assert!(!cache.is_admin("erin"));
    }

    #[test]
    fn resolve_unknown_user_with_no_account_given_resolves_nothing() {
        let cache = AssociationCache::new();
        assert_eq!(cache.resolve("alice", None), (None, None, None));
    }

    #[test]
    fn resolve_hit_after_insert() {
        let cache = AssociationCache::new();
        cache.insert_default_qos("alice", "research", "highprio");
        assert_eq!(
            cache.resolve("alice", Some("research")),
            (Some("research".into()), Some("highprio".into()), None)
        );
        assert_eq!(cache.resolve("alice", Some("other")), (None, None, None));
    }

    #[test]
    fn resolve_returns_allowed_qos_for_the_effective_account() {
        let cache = AssociationCache::new();
        cache.insert_allowed_qos("alice", "research", &["a", "b"]);
        let (account, _, allowed) = cache.resolve("alice", Some("research"));
        assert_eq!(account.as_deref(), Some("research"));
        assert_eq!(
            allowed,
            Some(HashSet::from(["a".to_string(), "b".to_string()]))
        );
    }

    #[test]
    fn resolve_distinguishes_an_empty_list_from_no_list() {
        let cache = AssociationCache::new();
        cache.insert_allowed_qos("alice", "research", &[]);
        cache.insert_association("bob", "research");
        assert_eq!(
            cache.resolve("alice", Some("research")).2,
            Some(HashSet::new())
        );
        assert_eq!(cache.resolve("bob", Some("research")).2, None);
        assert!(cache
            .check_qos_authorized("alice", "research", "normal")
            .is_err());
        assert!(cache
            .check_qos_authorized("bob", "research", "normal")
            .is_ok());
    }

    #[test]
    fn replace_swaps_the_whole_snapshot() {
        let cache = AssociationCache::new();
        cache.insert_default_qos("alice", "research", "old");
        cache.replace(Snapshot {
            default_qos: HashMap::from([(
                ("bob".to_string(), "eng".to_string()),
                "new".to_string(),
            )]),
            default_account: HashMap::from([("bob".to_string(), "eng".to_string())]),
            memberships: HashSet::from([("bob".to_string(), "eng".to_string())]),
            loaded: true,
            ..Snapshot::default()
        });
        assert_eq!(cache.resolve("alice", Some("research")), (None, None, None));
        assert_eq!(
            cache.resolve("bob", Some("eng")),
            (Some("eng".into()), Some("new".into()), None)
        );
        assert_eq!(
            cache.resolve("bob", None),
            (Some("eng".into()), Some("new".into()), None)
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
        cache.replace(Snapshot {
            default_qos: HashMap::from([(
                ("bob".to_string(), "eng".to_string()),
                "new".to_string(),
            )]),
            memberships: HashSet::from([("bob".to_string(), "eng".to_string())]),
            loaded: true,
            ..Snapshot::default()
        });
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
    fn check_qos_authorized_default_does_not_restrict_other_qos() {
        // A default only fills in a job that named none. It is not an allow-list.
        let cache = AssociationCache::new();
        cache.insert_default_qos("alice", "research", "highprio");
        assert!(cache
            .check_qos_authorized("alice", "research", "highprio")
            .is_ok());
        assert!(cache
            .check_qos_authorized("alice", "research", "other-teams-qos")
            .is_ok());
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
    fn check_qos_authorized_rejects_a_default_that_is_not_on_the_list() {
        let cache = AssociationCache::new();
        cache.insert_allowed_qos("alice", "research", &["burst"]);
        cache.insert_default_qos("alice", "research", "normal");
        assert!(cache
            .check_qos_authorized("alice", "research", "normal")
            .is_err());
        assert!(cache
            .check_qos_authorized("alice", "research", "burst")
            .is_ok());
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
        cache.replace(Snapshot {
            limits: HashMap::from([(
                ("bob".to_string(), "eng".to_string()),
                AccountLimits {
                    max_submit_jobs: Some(2),
                    ..Default::default()
                },
            )]),
            loaded: true,
            ..Snapshot::default()
        });
        assert_eq!(
            cache.limits("alice", "research"),
            AccountLimits::default(),
            "old limits must not survive the swap"
        );
        assert_eq!(cache.limits("bob", "eng").max_submit_jobs, Some(2));
    }

    #[test]
    fn a_coordinator_also_covers_sub_accounts() {
        let parents = HashMap::from([
            ("physics".to_string(), "science".to_string()),
            ("gpu".to_string(), "physics".to_string()),
            ("chem".to_string(), "science".to_string()),
        ]);
        let got = with_sub_accounts(
            [
                ("alice".to_string(), "science".to_string()),
                ("bob".to_string(), "physics".to_string()),
            ],
            &parents,
        );
        assert_eq!(
            got,
            HashSet::from([
                ("alice".to_string(), "science".to_string()),
                ("alice".to_string(), "physics".to_string()),
                ("alice".to_string(), "gpu".to_string()),
                ("alice".to_string(), "chem".to_string()),
                ("bob".to_string(), "physics".to_string()),
                ("bob".to_string(), "gpu".to_string()),
            ])
        );

        let cycle = HashMap::from([
            ("a".to_string(), "b".to_string()),
            ("b".to_string(), "a".to_string()),
        ]);
        let got = with_sub_accounts([("carol".to_string(), "a".to_string())], &cycle);
        assert_eq!(
            got,
            HashSet::from([
                ("carol".to_string(), "a".to_string()),
                ("carol".to_string(), "b".to_string()),
            ])
        );
    }
}
