// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use spur_core::config::SchedulerConfig;
use spur_core::partition::VictimOrderKey;

use super::{db, fairshare};

/// What accounting says about each account, as the optional victim order reads it.
#[derive(Debug, Default, Clone)]
pub(crate) struct StandingSnapshot {
    /// Decayed CPU plus GPU seconds the account consumed in finished runs, whatever
    /// QOS they ran in and unaffected by that QOS's `usage_factor`.
    pub usage: HashMap<String, f64>,
    /// Effective fair-share fraction of the cluster; an account with no entry or no
    /// share has no meaningful usage-to-share ratio.
    pub shares: HashMap<String, f64>,
    /// Node-seconds lost to preemption inside the burden window, weighted by recency.
    pub burden: HashMap<String, f64>,
    /// Half-life applied to `usage`, so usage not yet in accounting decays alike.
    pub halflife_days: u32,
}

impl StandingSnapshot {
    /// Weight of usage that is `age_secs` old, the same decay `usage` was summed with.
    pub fn decay(&self, age_secs: f64) -> f64 {
        fairshare::decay_weight(age_secs, self.halflife_days)
    }
}

/// Decayed capacity per account from hourly buckets, each aged by its start hour as
/// the fair-share usage rows are.
fn decayed_usage(
    buckets: &[db::CapacityBucket],
    halflife_days: u32,
    now: DateTime<Utc>,
) -> HashMap<String, f64> {
    let mut usage = HashMap::new();
    for bucket in buckets {
        let age = (now - bucket.period_start).num_seconds().max(0) as f64;
        *usage.entry(bucket.account.clone()).or_insert(0.0) +=
            bucket.capacity_seconds * fairshare::decay_weight(age, halflife_days);
    }
    usage
}

/// Node-seconds lost per account, each run weighted by how recently it ended: in full
/// when it has just ended, falling linearly to nothing at the edge of the window.
fn weighted_burden(
    runs: &[db::PreemptedRun],
    window_secs: f64,
    now: DateTime<Utc>,
) -> HashMap<String, f64> {
    let mut burden = HashMap::new();
    for run in runs {
        let age = (now - run.ended).num_seconds().max(0) as f64;
        let weight = 1.0 - age / window_secs;
        if weight <= 0.0 {
            continue;
        }
        let ran = (run.ended - run.started).num_seconds().max(0) as f64;
        *burden.entry(run.account.clone()).or_insert(0.0) += f64::from(run.nodes) * ran * weight;
    }
    burden
}

/// The reads a load has to make for the configured keys, so a key that is not
/// configured costs no query.
struct Plan {
    halflife_days: u32,
    /// Earliest run start the usage read covers: the fair-share lookback.
    usage_since: Option<DateTime<Utc>>,
    burden_window_secs: Option<f64>,
}

fn plan(sched: &SchedulerConfig, now: DateTime<Utc>) -> Plan {
    let wants = |key| sched.preempt_victim_order.contains(&key);
    let halflife_days = fairshare::effective_halflife_days(sched.fairshare_halflife_days);
    Plan {
        halflife_days,
        usage_since: wants(VictimOrderKey::UsageShare)
            .then(|| now - chrono::Duration::days(i64::from(halflife_days) * 4)),
        burden_window_secs: wants(VictimOrderKey::PreemptBurden)
            .then(|| f64::from(sched.preempt_burden_window_hours) * 3600.0),
    }
}

/// What `load` read from the database, per read that was planned.
struct Reads {
    usage: Option<(Vec<db::CapacityBucket>, Vec<db::AccountRecord>)>,
    preempted: Option<Vec<db::PreemptedRun>>,
}

fn assemble(plan: &Plan, reads: Reads, now: DateTime<Utc>) -> StandingSnapshot {
    let (usage, shares) = reads
        .usage
        .map_or_else(Default::default, |(buckets, accounts)| {
            (
                decayed_usage(&buckets, plan.halflife_days, now),
                fairshare::build_effective_shares(&accounts),
            )
        });
    let burden = match (reads.preempted, plan.burden_window_secs) {
        (Some(runs), Some(window_secs)) => weighted_burden(&runs, window_secs, now),
        _ => HashMap::new(),
    };
    StandingSnapshot {
        usage,
        shares,
        burden,
        halflife_days: plan.halflife_days,
    }
}

/// Load the figures the configured keys need. Usage is measured from the runs
/// themselves, because `usage_factor` is a billing knob and not capacity consumed.
pub(crate) async fn load(
    pool: &PgPool,
    sched: &SchedulerConfig,
) -> anyhow::Result<StandingSnapshot> {
    let now = Utc::now();
    let plan = plan(sched, now);
    let usage = match plan.usage_since {
        Some(since) => Some((
            db::consumed_capacity_by_hour(pool, since).await?,
            db::list_accounts(pool).await?,
        )),
        None => None,
    };
    let preempted = match plan.burden_window_secs {
        Some(window_secs) => {
            let since = now - chrono::Duration::seconds(window_secs as i64);
            Some(db::preempted_runs_since(pool, since).await?)
        }
        None => None,
    };
    Ok(assemble(&plan, Reads { usage, preempted }, now))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn bucket(
        account: &str,
        age: Duration,
        capacity_seconds: f64,
        now: DateTime<Utc>,
    ) -> db::CapacityBucket {
        db::CapacityBucket {
            account: account.into(),
            period_start: now - age,
            capacity_seconds,
        }
    }

    fn run(
        account: &str,
        nodes: u32,
        ran: Duration,
        ended_ago: Duration,
        now: DateTime<Utc>,
    ) -> db::PreemptedRun {
        db::PreemptedRun {
            account: account.into(),
            nodes,
            started: now - ended_ago - ran,
            ended: now - ended_ago,
        }
    }

    #[test]
    fn usage_halves_every_half_life_and_sums_per_account() {
        let now = Utc::now();
        let buckets = [
            bucket("a", Duration::zero(), 1000.0, now),
            bucket("a", Duration::days(14), 1000.0, now),
            bucket("b", Duration::days(28), 800.0, now),
        ];
        let usage = decayed_usage(&buckets, 14, now);
        assert!((usage["a"] - 1500.0).abs() < 1e-6, "a: full plus half");
        assert!((usage["b"] - 200.0).abs() < 1e-6, "b: a quarter");
    }

    #[test]
    fn a_bucket_in_the_future_is_not_boosted_past_full_weight() {
        let now = Utc::now();
        let buckets = [bucket("a", Duration::hours(-3), 100.0, now)];
        assert_eq!(decayed_usage(&buckets, 14, now)["a"], 100.0);
    }

    #[test]
    fn burden_weighs_a_loss_by_recency_and_drops_it_at_the_window_edge() {
        let now = Utc::now();
        let window = 24.0 * 3600.0;
        let runs = [
            run("a", 4, Duration::minutes(10), Duration::zero(), now),
            run("a", 2, Duration::minutes(10), Duration::hours(12), now),
            run("b", 8, Duration::hours(1), Duration::hours(24), now),
            run("b", 8, Duration::hours(1), Duration::hours(30), now),
        ];
        let burden = weighted_burden(&runs, window, now);
        // 4 nodes x 600 s in full, plus 2 nodes x 600 s at half weight.
        assert!((burden["a"] - (2400.0 + 600.0)).abs() < 1e-6);
        assert!(
            !burden.contains_key("b"),
            "at or past the edge counts for nothing"
        );
    }

    #[test]
    fn burden_never_counts_a_run_that_ends_before_it_starts() {
        let now = Utc::now();
        let skewed = db::PreemptedRun {
            account: "a".into(),
            nodes: 4,
            started: now,
            ended: now - Duration::minutes(5),
        };
        assert_eq!(weighted_burden(&[skewed], 3600.0, now)["a"], 0.0);
    }

    fn sched_with(keys: &[VictimOrderKey]) -> SchedulerConfig {
        SchedulerConfig {
            preempt_victim_order: keys.to_vec(),
            ..Default::default()
        }
    }

    #[test]
    fn a_key_that_is_not_configured_plans_no_read() {
        let now = Utc::now();
        let none = plan(&sched_with(&[VictimOrderKey::Hash]), now);
        assert!(none.usage_since.is_none() && none.burden_window_secs.is_none());

        let usage = plan(&sched_with(&[VictimOrderKey::UsageShare]), now);
        assert_eq!(
            usage.usage_since,
            Some(now - Duration::days(56)),
            "four half-lives"
        );
        assert!(usage.burden_window_secs.is_none());

        let burden = plan(&sched_with(&[VictimOrderKey::PreemptBurden]), now);
        assert!(burden.usage_since.is_none());
        assert_eq!(burden.burden_window_secs, Some(24.0 * 3600.0));
    }

    #[test]
    fn the_plan_follows_the_configured_half_life_and_window() {
        let mut sched = sched_with(&[VictimOrderKey::UsageShare, VictimOrderKey::PreemptBurden]);
        sched.fairshare_halflife_days = 7;
        sched.preempt_burden_window_hours = 72;
        let now = Utc::now();
        let planned = plan(&sched, now);
        assert_eq!(planned.halflife_days, 7);
        assert_eq!(planned.usage_since, Some(now - Duration::days(28)));
        assert_eq!(planned.burden_window_secs, Some(72.0 * 3600.0));
    }

    fn account(name: &str, weight: i32) -> db::AccountRecord {
        db::AccountRecord {
            name: name.into(),
            description: String::new(),
            organization: String::new(),
            parent: None,
            fairshare_weight: weight,
            max_running_jobs: None,
            grp_tres: None,
        }
    }

    #[test]
    fn a_snapshot_holds_what_was_read_and_nothing_for_what_was_not() {
        let now = Utc::now();
        let sched = sched_with(&[VictimOrderKey::UsageShare, VictimOrderKey::PreemptBurden]);
        let planned = plan(&sched, now);
        let reads = Reads {
            usage: Some((
                vec![bucket("a", Duration::zero(), 500.0, now)],
                vec![account("a", 3), account("b", 1)],
            )),
            preempted: Some(vec![run(
                "a",
                2,
                Duration::seconds(100),
                Duration::zero(),
                now,
            )]),
        };
        let snapshot = assemble(&planned, reads, now);
        assert_eq!(snapshot.usage["a"], 500.0);
        assert_eq!(
            snapshot.shares["a"], 0.75,
            "weight 3 of 4 among top-level accounts"
        );
        assert_eq!(snapshot.shares["b"], 0.25);
        assert_eq!(snapshot.burden["a"], 200.0);
        assert_eq!(snapshot.halflife_days, 14);

        let empty = assemble(
            &planned,
            Reads {
                usage: None,
                preempted: None,
            },
            now,
        );
        assert!(empty.usage.is_empty() && empty.shares.is_empty() && empty.burden.is_empty());
    }
}
