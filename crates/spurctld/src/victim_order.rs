// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Optional secondary order among preemption victims of equal QOS priority.
//!
//! Left alone, victims that tie on QOS priority are taken in node-name order. When
//! `scheduler.preempt_victim_order` lists keys, they rank the tied nodes first and
//! node name only breaks what is still tied. The keys never decide who may be
//! preempted: eligibility, QOS priority and the exempt time are settled before any
//! of this runs.
//!
//! A node is as protected as its most protected occupant, the same convention the
//! QOS-priority cost already follows, because evacuating a node evicts everything on
//! it.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use sqlx::PgPool;
use tracing::warn;

use spur_core::job::{effective_gpus, Job, JobId};
use spur_core::partition::VictimOrderKey;

use crate::accounting::StandingSnapshot;
use crate::cluster::ClusterManager;

/// Longest a refresh may take before the last snapshot is kept instead.
const LOAD_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-account figures cached from accounting and refreshed in the background, like
/// the fair-share cache. Empty until the first successful load; until then every key
/// reads an owner as unknown.
#[derive(Default)]
pub(crate) struct OwnerStandingCache {
    snapshot: RwLock<Option<Arc<StandingSnapshot>>>,
}

impl OwnerStandingCache {
    /// Standing of the owner of every job in `running`, or `None` when no key is
    /// configured and the order is off.
    pub(crate) fn standings(
        &self,
        keys: &[VictimOrderKey],
        running: &[Job],
        now: DateTime<Utc>,
    ) -> Option<HashMap<JobId, OwnerStanding>> {
        if keys.is_empty() {
            return None;
        }
        let snapshot = self.snapshot.read().clone();
        Some(owner_standings(snapshot.as_deref(), running, now))
    }

    fn replace(&self, snapshot: StandingSnapshot) {
        *self.snapshot.write() = Some(Arc::new(snapshot));
    }

    /// Set the snapshot directly, bypassing the DB refresh loop; for tests only.
    #[cfg(test)]
    pub(crate) fn set_for_test(&self, snapshot: StandingSnapshot) {
        self.replace(snapshot);
    }

    /// Replace the snapshot with what `load` returns, or keep the last one when the
    /// load fails or runs past its deadline.
    async fn refresh(&self, load: impl Future<Output = anyhow::Result<StandingSnapshot>>) {
        match tokio::time::timeout(LOAD_TIMEOUT, load).await {
            Ok(Ok(snapshot)) => self.replace(snapshot),
            Ok(Err(e)) => warn!(error = %e, "owner standing refresh failed, retaining stale data"),
            Err(_) => warn!("owner standing refresh timed out, retaining stale data"),
        }
    }

    /// Refresh on every tick. The configuration is read afresh each time, so enabling a
    /// key or changing the burden window applies from the next tick, and nothing is
    /// queried while no key needs accounting.
    pub(crate) fn spawn_refresh_loop(
        self: &Arc<Self>,
        pool: PgPool,
        cluster: Arc<ClusterManager>,
        refresh_interval_secs: u64,
    ) {
        let cache = Arc::clone(self);
        let interval = Duration::from_secs(refresh_interval_secs.max(10));

        tokio::spawn(async move {
            loop {
                let config = cluster.config();
                if needs_accounting(&config.scheduler.preempt_victim_order) {
                    let load = crate::accounting::load_standing(&pool, &config.scheduler);
                    cache.refresh(load).await;
                }
                tokio::time::sleep(interval).await;
            }
        });
    }
}

/// Whether any key reads the accounting database; `hash` alone needs nothing.
fn needs_accounting(keys: &[VictimOrderKey]) -> bool {
    keys.iter().any(|key| {
        matches!(
            key,
            VictimOrderKey::UsageShare | VictimOrderKey::PreemptBurden
        )
    })
}

/// One owner's figures, as the order reads them for each candidate victim.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct OwnerStanding {
    /// Share of all recent usage divided by the owner's share of the cluster, so
    /// `1.0` is exactly at share and larger is heavier.
    pub usage_share: f64,
    /// Node-seconds the owner recently lost to preemption, weighted by recency.
    pub burden: f64,
}

impl OwnerStanding {
    /// An owner accounting says nothing about: at its share, with no recorded loss.
    pub(crate) const NEUTRAL: Self = Self {
        usage_share: 1.0,
        burden: 0.0,
    };
}

/// Standing of the owner of every job in `running`, keyed by job id.
///
/// Usage is capacity actually consumed (CPU plus GPU seconds) in every QOS, and
/// deliberately ignores `usage_factor`: that is a billing and pending-priority knob,
/// and discounting a QOS for billing must not move its holders in the preemption
/// order. Finished runs come from the snapshot; what an account is running now is
/// added from the running jobs themselves, decayed as it will be once finished, so a
/// long-running owner stays visible. The ratio uses raw usage and shares, not the
/// capped fair-share factor: the factor is highest for an owner with no usage, which
/// would rank an idle owner as the heaviest.
///
/// Anything unknown is neutral: no snapshot yet, an account with no positive share,
/// or no usage anywhere to compare against.
fn owner_standings(
    snapshot: Option<&StandingSnapshot>,
    running: &[Job],
    now: DateTime<Utc>,
) -> HashMap<JobId, OwnerStanding> {
    let Some(snapshot) = snapshot else {
        return running
            .iter()
            .map(|job| (job.job_id, OwnerStanding::NEUTRAL))
            .collect();
    };
    let account_of = |job: &Job| job.spec.account.as_deref().unwrap_or("").to_owned();

    let mut live: HashMap<String, f64> = HashMap::new();
    for job in running {
        let Some(started) = job.start_time else {
            continue;
        };
        let elapsed = (now - started).num_seconds().max(0) as f64;
        let per_second = f64::from(job.spec.num_tasks) * f64::from(job.spec.cpus_per_task)
            + effective_gpus(&job.spec, job.spec.num_nodes) as f64;
        *live.entry(account_of(job)).or_insert(0.0) +=
            elapsed * per_second * snapshot.decay(elapsed);
    }
    let total = snapshot.usage.values().sum::<f64>() + live.values().sum::<f64>();

    running
        .iter()
        .map(|job| {
            let account = account_of(job);
            let usage_share = match snapshot.shares.get(&account) {
                Some(&share) if share > 0.0 && total > 0.0 => {
                    let used = snapshot.usage.get(&account).copied().unwrap_or(0.0)
                        + live.get(&account).copied().unwrap_or(0.0);
                    used / total / share
                }
                _ => OwnerStanding::NEUTRAL.usage_share,
            };
            let burden = snapshot.burden.get(&account).copied().unwrap_or(0.0);
            (
                job.job_id,
                OwnerStanding {
                    usage_share,
                    burden,
                },
            )
        })
        .collect()
}

/// How the nodes of one preemptor's victim search rank against each other.
pub(crate) struct VictimOrder<'a> {
    keys: &'a [VictimOrderKey],
    standing: &'a HashMap<JobId, OwnerStanding>,
    /// Seeds the hash key: one preemptor sees one stable order, different
    /// preemptors spread over different nodes.
    preemptor: JobId,
}

impl<'a> VictimOrder<'a> {
    pub(crate) fn new(
        keys: &'a [VictimOrderKey],
        standing: &'a HashMap<JobId, OwnerStanding>,
        preemptor: JobId,
    ) -> Self {
        Self {
            keys,
            standing,
            preemptor,
        }
    }

    /// `Less` when the node `a` should be evicted before `b`: the first key that
    /// tells them apart decides, and `Equal` leaves the tie to the caller.
    pub(crate) fn compare(&self, a: (&str, &[JobId]), b: (&str, &[JobId])) -> Ordering {
        self.keys
            .iter()
            .map(|key| match key {
                // Heaviest first; a node's value is that of its lightest occupant.
                VictimOrderKey::UsageShare => self
                    .node_usage_share(b.1)
                    .total_cmp(&self.node_usage_share(a.1)),
                // Least burdened first; a node's value is that of its most burdened occupant.
                VictimOrderKey::PreemptBurden => {
                    self.node_burden(a.1).total_cmp(&self.node_burden(b.1))
                }
                VictimOrderKey::Hash => {
                    stable_hash(self.preemptor, a.0).cmp(&stable_hash(self.preemptor, b.0))
                }
            })
            .find(|ordering| ordering.is_ne())
            .unwrap_or(Ordering::Equal)
    }

    fn standing_of(&self, job: JobId) -> OwnerStanding {
        self.standing
            .get(&job)
            .copied()
            .unwrap_or(OwnerStanding::NEUTRAL)
    }

    fn node_usage_share(&self, occupants: &[JobId]) -> f64 {
        occupants
            .iter()
            .map(|id| self.standing_of(*id).usage_share)
            .min_by(f64::total_cmp)
            .unwrap_or(OwnerStanding::NEUTRAL.usage_share)
    }

    fn node_burden(&self, occupants: &[JobId]) -> f64 {
        occupants
            .iter()
            .map(|id| self.standing_of(*id).burden)
            .max_by(f64::total_cmp)
            .unwrap_or(OwnerStanding::NEUTRAL.burden)
    }
}

/// A 64-bit hash of the preemptor and node that never changes between runs or
/// toolchains, unlike a std hasher: FNV-1a with fixed constants, then a finalizer
/// so names that differ in one trailing byte still land far apart.
fn stable_hash(preemptor: JobId, node: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in preemptor.to_le_bytes().iter().chain(node.as_bytes()) {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash ^= hash >> 30;
    hash = hash.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    hash ^= hash >> 27;
    hash = hash.wrapping_mul(0x94d0_49bb_1331_11eb);
    hash ^ (hash >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_core::job::JobSpec;

    fn standing(usage_share: f64, burden: f64) -> OwnerStanding {
        OwnerStanding {
            usage_share,
            burden,
        }
    }

    fn standings(entries: &[(JobId, OwnerStanding)]) -> HashMap<JobId, OwnerStanding> {
        entries.iter().copied().collect()
    }

    fn compare(
        keys: &[VictimOrderKey],
        standing: &HashMap<JobId, OwnerStanding>,
        a: (&str, &[JobId]),
        b: (&str, &[JobId]),
    ) -> Ordering {
        VictimOrder::new(keys, standing, 1).compare(a, b)
    }

    #[test]
    fn usage_share_takes_the_heaviest_owner_first() {
        let st = standings(&[(10, standing(0.2, 0.0)), (20, standing(3.0, 0.0))]);
        let keys = [VictimOrderKey::UsageShare];
        assert_eq!(
            compare(&keys, &st, ("n1", &[10]), ("n2", &[20])),
            Ordering::Greater,
            "the lighter owner's node ranks after the heavier owner's, whatever the names"
        );
        assert_eq!(
            compare(&keys, &st, ("n2", &[20]), ("n1", &[10])),
            Ordering::Less
        );
    }

    #[test]
    fn a_node_is_as_protected_as_its_most_protected_occupant() {
        // Node n2 holds a heavy owner's job beside a light owner's. The light one
        // protects the node, so n2 ranks as light as n1 for usage share.
        let st = standings(&[
            (10, standing(0.2, 0.0)),
            (20, standing(3.0, 0.0)),
            (21, standing(0.2, 0.0)),
        ]);
        let keys = [VictimOrderKey::UsageShare];
        assert_eq!(
            compare(&keys, &st, ("n1", &[10]), ("n2", &[20, 21])),
            Ordering::Equal
        );

        // The same convention for burden: the most burdened occupant protects.
        let st = standings(&[
            (10, standing(1.0, 5.0)),
            (20, standing(1.0, 0.0)),
            (21, standing(1.0, 5.0)),
        ]);
        let keys = [VictimOrderKey::PreemptBurden];
        assert_eq!(
            compare(&keys, &st, ("n1", &[10]), ("n2", &[20, 21])),
            Ordering::Equal
        );
    }

    #[test]
    fn preempt_burden_protects_the_owner_that_already_lost_the_most() {
        let st = standings(&[(10, standing(1.0, 90_000.0)), (20, standing(1.0, 0.0))]);
        let keys = [VictimOrderKey::PreemptBurden];
        assert_eq!(
            compare(&keys, &st, ("n1", &[10]), ("n2", &[20])),
            Ordering::Greater,
            "n1's owner has lost far more, so n2 is taken first"
        );
    }

    #[test]
    fn hash_is_stable_and_spreads_over_preemptors() {
        // Pinned values: the order must not change between toolchains or runs.
        assert_eq!(stable_hash(1, "n1"), 0xb023_3076_789d_b4ff);
        assert_eq!(stable_hash(1, "n2"), 0x0967_b5d5_e13c_6966);
        assert_eq!(stable_hash(2, "n1"), 0x090e_6d81_b765_6ce6);
        assert_eq!(stable_hash(2, "n2"), 0x27ce_7728_24ea_f7c2);

        let st = HashMap::new();
        let keys = [VictimOrderKey::Hash];
        let first = |preemptor: JobId| {
            VictimOrder::new(&keys, &st, preemptor).compare(("n1", &[]), ("n2", &[]))
        };
        assert_eq!(first(1), Ordering::Greater, "preemptor 1 takes n2 first");
        assert_eq!(first(2), Ordering::Less, "preemptor 2 takes n1 first");
        assert_eq!(first(1), first(1), "one preemptor always sees one order");
    }

    #[test]
    fn keys_apply_in_the_order_listed_and_later_keys_only_break_ties() {
        // n2's owner is heavier and has lost more; n1's owner is lighter and has lost
        // nothing. The listed order decides which fact wins.
        let st = standings(&[(10, standing(0.2, 0.0)), (20, standing(3.0, 9.0))]);
        let share_first = [VictimOrderKey::UsageShare, VictimOrderKey::PreemptBurden];
        let burden_first = [VictimOrderKey::PreemptBurden, VictimOrderKey::UsageShare];
        assert_eq!(
            compare(&share_first, &st, ("n1", &[10]), ("n2", &[20])),
            Ordering::Greater,
            "usage share listed first: the heavier n2 owner goes first"
        );
        assert_eq!(
            compare(&burden_first, &st, ("n1", &[10]), ("n2", &[20])),
            Ordering::Less,
            "burden listed first: the owner with no losses goes first"
        );

        // A tie on the first key falls through to the next.
        let st = standings(&[(10, standing(1.0, 5.0)), (20, standing(1.0, 0.0))]);
        assert_eq!(
            compare(&share_first, &st, ("n1", &[10]), ("n2", &[20])),
            Ordering::Greater,
            "equal usage share, so the less burdened n2 owner goes first"
        );

        // Equal on every listed key leaves the tie to the caller.
        let st = standings(&[(10, standing(1.0, 1.0)), (20, standing(1.0, 1.0))]);
        assert_eq!(
            compare(&share_first, &st, ("n1", &[10]), ("n2", &[20])),
            Ordering::Equal
        );
    }

    #[test]
    fn an_empty_key_list_never_separates_nodes() {
        let st = standings(&[(10, standing(0.1, 9.0)), (20, standing(9.0, 0.0))]);
        assert_eq!(
            compare(&[], &st, ("n1", &[10]), ("n2", &[20])),
            Ordering::Equal
        );
    }

    fn running_job(
        id: JobId,
        account: Option<&str>,
        started_secs_ago: Option<i64>,
        now: DateTime<Utc>,
    ) -> Job {
        let mut job = Job::new(
            id,
            JobSpec {
                name: format!("job-{id}"),
                user: "u".into(),
                account: account.map(String::from),
                num_nodes: 1,
                num_tasks: 1,
                cpus_per_task: 1,
                ..Default::default()
            },
        );
        job.start_time = started_secs_ago.map(|secs| now - chrono::Duration::seconds(secs));
        job
    }

    fn snapshot(
        usage: &[(&str, f64)],
        shares: &[(&str, f64)],
        burden: &[(&str, f64)],
    ) -> StandingSnapshot {
        let owned = |entries: &[(&str, f64)]| {
            entries
                .iter()
                .map(|(name, value)| ((*name).to_owned(), *value))
                .collect()
        };
        StandingSnapshot {
            usage: owned(usage),
            shares: owned(shares),
            burden: owned(burden),
            halflife_days: 14,
        }
    }

    #[test]
    fn usage_share_is_usage_fraction_over_share() {
        let now = Utc::now();
        let snap = snapshot(
            &[("a", 750.0), ("b", 250.0)],
            &[("a", 0.5), ("b", 0.5)],
            &[],
        );
        let jobs = [
            running_job(1, Some("a"), None, now),
            running_job(2, Some("b"), None, now),
        ];
        let st = owner_standings(Some(&snap), &jobs, now);
        assert!((st[&1].usage_share - 1.5).abs() < 1e-9, "{:?}", st[&1]);
        assert!((st[&2].usage_share - 0.5).abs() < 1e-9, "{:?}", st[&2]);
    }

    #[test]
    fn an_owner_with_no_usage_is_the_lightest_not_the_heaviest() {
        // The capped fair-share factor is highest for zero usage, which read as a
        // ranking would put an idle owner first in line.
        let now = Utc::now();
        let snap = snapshot(&[("busy", 1000.0)], &[("busy", 0.5), ("idle", 0.5)], &[]);
        let jobs = [
            running_job(1, Some("busy"), None, now),
            running_job(2, Some("idle"), None, now),
        ];
        let st = owner_standings(Some(&snap), &jobs, now);
        assert_eq!(st[&2].usage_share, 0.0);
        assert!(st[&1].usage_share > st[&2].usage_share);
    }

    #[test]
    fn running_jobs_count_toward_usage_before_they_reach_accounting() {
        let now = Utc::now();
        let snap = snapshot(&[("old", 1000.0)], &[("old", 0.5), ("new", 0.5)], &[]);
        // Alone, "new" has billed nothing. A 1000 s run of 1 cpu makes it as heavy as
        // "old" (decay over 1000 s of a 14 day half-life is under 0.1 %).
        let idle_only = [running_job(1, Some("new"), None, now)];
        let before = owner_standings(Some(&snap), &idle_only, now);
        assert_eq!(before[&1].usage_share, 0.0);

        let jobs = [running_job(1, Some("new"), Some(1000), now)];
        let after = owner_standings(Some(&snap), &jobs, now);
        let share = after[&1].usage_share;
        assert!((share - 1.0).abs() < 0.01, "got {share}");
    }

    #[test]
    fn running_usage_is_elapsed_time_times_allocated_cpus_and_gpus() {
        let now = Utc::now();
        let mut job = running_job(1, Some("b"), Some(1000), now);
        job.spec.num_tasks = 2;
        job.spec.cpus_per_task = 2;
        job.spec.gpus = Some(spur_core::gpu_request::GpuRequest::new(2, None));
        // The job's QOS plays no part: a discounted QOS counts in full.
        job.spec.qos = Some("discounted".into());

        // 2 tasks x 2 cpus plus 2 GPUs is 6 units for 1000 s. Give "a" exactly that
        // much finished usage, so each holds half of the total at an equal share.
        let mut snap = snapshot(&[], &[("a", 0.5), ("b", 0.5)], &[]);
        snap.usage
            .insert("a".into(), 6.0 * 1000.0 * snap.decay(1000.0));

        let st = owner_standings(Some(&snap), &[job], now);
        assert!((st[&1].usage_share - 1.0).abs() < 1e-9, "{:?}", st[&1]);
    }

    #[test]
    fn unknown_owners_and_missing_data_are_neutral() {
        let now = Utc::now();
        let snap = snapshot(&[("a", 100.0)], &[("a", 0.5), ("zero", 0.0)], &[("a", 7.0)]);
        let jobs = [
            running_job(1, Some("stranger"), None, now),
            running_job(2, None, None, now),
            running_job(3, Some("zero"), None, now),
            running_job(4, Some("a"), None, now),
        ];
        let st = owner_standings(Some(&snap), &jobs, now);
        for id in [1, 2, 3] {
            assert_eq!(st[&id], OwnerStanding::NEUTRAL, "job {id}");
        }
        assert_eq!(st[&4].burden, 7.0, "burden is read for a known account");

        let none = owner_standings(None, &jobs, now);
        assert!(none.values().all(|s| *s == OwnerStanding::NEUTRAL));

        // No usage at all gives nothing to compare against.
        let empty = snapshot(&[], &[("a", 0.5)], &[]);
        let st = owner_standings(Some(&empty), &jobs[3..], now);
        assert_eq!(st[&4].usage_share, 1.0);
    }

    fn marked(usage_share: f64) -> StandingSnapshot {
        StandingSnapshot {
            shares: [("a".to_string(), usage_share)].into_iter().collect(),
            halflife_days: 14,
            ..Default::default()
        }
    }

    fn loaded_share(cache: &OwnerStandingCache) -> Option<f64> {
        cache.snapshot.read().as_ref().map(|snap| snap.shares["a"])
    }

    #[tokio::test]
    async fn a_refresh_replaces_the_snapshot_and_a_failed_one_keeps_it() {
        let cache = OwnerStandingCache::default();
        assert!(
            cache.snapshot.read().is_none(),
            "unknown until the first load"
        );

        cache.refresh(async { Ok(marked(0.25)) }).await;
        assert_eq!(loaded_share(&cache), Some(0.25));

        cache
            .refresh(async { Err(anyhow::anyhow!("database down")) })
            .await;
        assert_eq!(loaded_share(&cache), Some(0.25), "stale data is retained");

        cache.refresh(async { Ok(marked(0.75)) }).await;
        assert_eq!(loaded_share(&cache), Some(0.75));
    }

    #[tokio::test(start_paused = true)]
    async fn a_refresh_that_never_finishes_keeps_the_snapshot() {
        let cache = OwnerStandingCache::default();
        cache.refresh(async { Ok(marked(0.25)) }).await;

        cache.refresh(std::future::pending()).await;
        assert_eq!(loaded_share(&cache), Some(0.25));
    }

    #[test]
    fn only_the_accounting_backed_keys_need_the_database() {
        assert!(!needs_accounting(&[]));
        assert!(!needs_accounting(&[VictimOrderKey::Hash]));
        assert!(needs_accounting(&[
            VictimOrderKey::Hash,
            VictimOrderKey::UsageShare
        ]));
        assert!(needs_accounting(&[VictimOrderKey::PreemptBurden]));
    }

    #[test]
    fn standings_are_computed_only_while_a_key_is_configured() {
        let cache = OwnerStandingCache::default();
        let jobs = [running_job(1, Some("a"), None, Utc::now())];
        assert!(cache.standings(&[], &jobs, Utc::now()).is_none());
        let on = cache.standings(&[VictimOrderKey::Hash], &jobs, Utc::now());
        assert_eq!(
            on.unwrap()[&1],
            OwnerStanding::NEUTRAL,
            "no snapshot is neutral"
        );
    }
}
