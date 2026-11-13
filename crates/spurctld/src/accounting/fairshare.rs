// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};

use spur_core::config::BillingWeights;

use super::db::UsageRecord;

/// Compute per-(user, account) fair-share factors using hierarchical BFS shares,
/// per-user association weights, and decay-weighted billable usage.
pub(super) fn compute_fairshare(
    usage: &[UsageRecord],
    accounts: &[super::db::AccountRecord],
    user_weights: &HashMap<(String, String), i32>,
    halflife_days: u32,
    now: DateTime<Utc>,
    billing_weights: &BillingWeights,
    offsets: &HashMap<(String, String), (i64, i64)>,
) -> HashMap<(String, String), f64> {
    if accounts.is_empty() {
        return HashMap::new();
    }

    let effective_shares = build_effective_shares(accounts);
    if effective_shares.is_empty() {
        return HashMap::new();
    }

    let halflife = Duration::days(halflife_days as i64);
    let decay_rate = 2.0_f64.ln() / halflife.num_seconds() as f64;

    let mut user_usage: HashMap<(String, String), f64> = HashMap::new();
    for record in usage {
        let age = (now - record.period_start).num_seconds().max(0) as f64;
        let decay = (-decay_rate * age).exp();
        let billable = billing_weights.bill(record.cpu_seconds, record.gpu_seconds) * decay;

        *user_usage
            .entry((record.user_name.clone(), record.account.clone()))
            .or_insert(0.0) += billable;
    }

    for ((user, account), (off_cpu, off_gpu)) in offsets {
        let offset_billable = billing_weights.bill(*off_cpu, *off_gpu);
        let entry = user_usage
            .entry((user.clone(), account.clone()))
            .or_insert(0.0);
        *entry = (*entry + offset_billable).max(0.0);
    }

    let total_usage: f64 = user_usage.values().sum();
    let epsilon = 0.001;

    let mut all_pairs: std::collections::HashSet<(String, String)> =
        user_usage.keys().cloned().collect();
    for key in user_weights.keys() {
        all_pairs.insert(key.clone());
    }

    let mut users_per_account: HashMap<String, Vec<String>> = HashMap::new();
    for (user, account) in &all_pairs {
        users_per_account
            .entry(account.clone())
            .or_default()
            .push(user.clone());
    }

    let mut user_shares: HashMap<(String, String), f64> = HashMap::new();
    for (account, users) in &users_per_account {
        let account_share = effective_shares.get(account).copied().unwrap_or(0.0);
        if account_share <= 0.0 {
            for user in users {
                user_shares.insert((user.clone(), account.clone()), 0.0);
            }
            continue;
        }

        let weight_sum: i32 = users
            .iter()
            .map(|u| {
                user_weights
                    .get(&(u.clone(), account.clone()))
                    .copied()
                    .unwrap_or(1)
            })
            .sum();
        let weight_sum = weight_sum.max(1) as f64;

        for user in users {
            let w = user_weights
                .get(&(user.clone(), account.clone()))
                .copied()
                .unwrap_or(1) as f64;
            let share = (w / weight_sum) * account_share;
            user_shares.insert((user.clone(), account.clone()), share);
        }
    }

    // Cap matches priority.rs::effective_priority's .min(10.0) so sshare,
    // sprio, and the scheduler all agree on the effective range.
    const FAIRSHARE_CAP: f64 = 10.0;

    let mut factors = HashMap::new();
    for (key, user_share) in &user_shares {
        let usage_val = user_usage.get(key).copied().unwrap_or(0.0);
        let factor = if usage_val < epsilon {
            (*user_share / epsilon).min(FAIRSHARE_CAP)
        } else {
            let actual_share = usage_val / total_usage.max(epsilon);
            (user_share / actual_share).min(FAIRSHARE_CAP)
        };
        factors.insert(key.clone(), factor);
    }

    factors
}

/// BFS from root accounts, splitting parent share proportionally by sibling weight.
/// Accounts whose parent refers to a non-existent account are treated as roots
/// so they still participate in fair-share rather than being silently excluded.
fn build_effective_shares(accounts: &[super::db::AccountRecord]) -> HashMap<String, f64> {
    let weight_map: HashMap<&str, i32> = accounts
        .iter()
        .map(|a| (a.name.as_str(), a.fairshare_weight))
        .collect();

    let mut children_of: HashMap<Option<&str>, Vec<&str>> = HashMap::new();
    for a in accounts {
        let parent_key = match a.parent.as_deref() {
            Some(p) if weight_map.contains_key(p) => Some(p),
            Some(p) => {
                tracing::warn!(
                    account = a.name,
                    parent = p,
                    "account parent not found, treating as root"
                );
                None
            }
            None => None,
        };
        children_of.entry(parent_key).or_default().push(&a.name);
    }

    let mut effective: HashMap<String, f64> = HashMap::new();

    let mut queue: std::collections::VecDeque<(Option<&str>, f64)> =
        std::collections::VecDeque::new();
    queue.push_back((None, 1.0));

    while let Some((parent, parent_share)) = queue.pop_front() {
        let Some(kids) = children_of.get(&parent) else {
            continue;
        };
        let sibling_weight_sum: i32 = kids
            .iter()
            .map(|name| weight_map.get(name).copied().unwrap_or(1))
            .sum();
        let sibling_weight_sum = sibling_weight_sum.max(1) as f64;

        for name in kids {
            let w = weight_map.get(name).copied().unwrap_or(1) as f64;
            let share = (w / sibling_weight_sum) * parent_share;
            effective.insert(name.to_string(), share);
            queue.push_back((Some(name), share));
        }
    }

    // Accounts the walk never reached are caught in a parent cycle (A→B→A) or
    // point at themselves. Re-root them like a missing parent, so a
    // misconfigured hierarchy degrades to flat shares instead of silently
    // dropping the whole subtree's fair-share to zero.
    let unreached: Vec<&str> = accounts
        .iter()
        .map(|a| a.name.as_str())
        .filter(|name| !effective.contains_key(*name))
        .collect();
    if !unreached.is_empty() {
        let weight_sum: i32 = unreached
            .iter()
            .map(|name| weight_map.get(name).copied().unwrap_or(1))
            .sum();
        let weight_sum = weight_sum.max(1) as f64;
        for name in unreached {
            tracing::warn!(
                account = name,
                "account unreachable from any root (parent cycle?), treating as root"
            );
            let w = weight_map.get(name).copied().unwrap_or(1) as f64;
            effective.insert(name.to_string(), w / weight_sum);
        }
    }

    effective
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_account(
        name: &str,
        parent: Option<&str>,
        weight: i32,
    ) -> super::super::db::AccountRecord {
        super::super::db::AccountRecord {
            name: name.into(),
            description: String::new(),
            organization: String::new(),
            parent: parent.map(String::from),
            fairshare_weight: weight,
            max_running_jobs: None,
            grp_tres: None,
        }
    }

    #[test]
    fn test_compute_fairshare() {
        let now = Utc::now();
        let usage = vec![
            UsageRecord {
                user_name: "alice".into(),
                account: "research".into(),
                cpu_seconds: 100_000,
                gpu_seconds: 50_000,

                job_count: 10,
                period_start: now - Duration::days(1),
            },
            UsageRecord {
                user_name: "bob".into(),
                account: "engineering".into(),
                cpu_seconds: 10_000,
                gpu_seconds: 0,

                job_count: 2,
                period_start: now - Duration::days(1),
            },
        ];

        let accounts = vec![
            make_account("research", None, 1),
            make_account("engineering", None, 1),
        ];

        let mut user_weights = HashMap::new();
        user_weights.insert(("alice".into(), "research".into()), 1);
        user_weights.insert(("bob".into(), "engineering".into()), 1);

        let factors = compute_fairshare(
            &usage,
            &accounts,
            &user_weights,
            14,
            now,
            &BillingWeights::default(),
            &HashMap::new(),
        );

        let alice_factor = factors.get(&("alice".into(), "research".into())).unwrap();
        let bob_factor = factors.get(&("bob".into(), "engineering".into())).unwrap();
        assert!(*bob_factor > 1.0);
        assert!(*alice_factor < 1.0);
        assert!(bob_factor > alice_factor);
    }

    #[test]
    fn test_gpu_seconds_included_in_billable() {
        let now = Utc::now();
        let usage = vec![
            UsageRecord {
                user_name: "alice".into(),
                account: "research".into(),
                cpu_seconds: 10_000,
                gpu_seconds: 90_000,

                job_count: 5,
                period_start: now - Duration::days(1),
            },
            UsageRecord {
                user_name: "bob".into(),
                account: "research".into(),
                cpu_seconds: 10_000,
                gpu_seconds: 0,

                job_count: 5,
                period_start: now - Duration::days(1),
            },
        ];

        let accounts = vec![make_account("research", None, 1)];
        let mut user_weights = HashMap::new();
        user_weights.insert(("alice".into(), "research".into()), 1);
        user_weights.insert(("bob".into(), "research".into()), 1);

        let factors = compute_fairshare(
            &usage,
            &accounts,
            &user_weights,
            14,
            now,
            &BillingWeights::default(),
            &HashMap::new(),
        );
        let alice = factors.get(&("alice".into(), "research".into())).unwrap();
        let bob = factors.get(&("bob".into(), "research".into())).unwrap();
        assert!(bob > alice);
    }

    #[test]
    fn test_account_hierarchy() {
        let now = Utc::now();
        let accounts = vec![
            make_account("root", None, 1),
            make_account("child_a", Some("root"), 3),
            make_account("child_b", Some("root"), 1),
        ];

        let usage = vec![
            UsageRecord {
                user_name: "alice".into(),
                account: "child_a".into(),
                cpu_seconds: 10_000,
                gpu_seconds: 0,

                job_count: 1,
                period_start: now - Duration::days(1),
            },
            UsageRecord {
                user_name: "bob".into(),
                account: "child_b".into(),
                cpu_seconds: 10_000,
                gpu_seconds: 0,

                job_count: 1,
                period_start: now - Duration::days(1),
            },
        ];

        let mut user_weights = HashMap::new();
        user_weights.insert(("alice".into(), "child_a".into()), 1);
        user_weights.insert(("bob".into(), "child_b".into()), 1);

        let factors = compute_fairshare(
            &usage,
            &accounts,
            &user_weights,
            14,
            now,
            &BillingWeights::default(),
            &HashMap::new(),
        );
        let alice = factors.get(&("alice".into(), "child_a".into())).unwrap();
        let bob = factors.get(&("bob".into(), "child_b".into())).unwrap();
        assert!(alice > bob);
    }

    #[test]
    fn test_zero_usage_boost() {
        let now = Utc::now();
        let accounts = vec![make_account("research", None, 1)];
        let user_weights: HashMap<(String, String), i32> =
            vec![(("idle_user".into(), "research".into()), 1)]
                .into_iter()
                .collect();

        let factors = compute_fairshare(
            &[],
            &accounts,
            &user_weights,
            14,
            now,
            &BillingWeights::default(),
            &HashMap::new(),
        );
        let idle = factors
            .get(&("idle_user".into(), "research".into()))
            .unwrap();
        assert!(*idle > 1.0);
    }

    #[test]
    fn test_concrete_hierarchy_values() {
        let now = Utc::now();
        let accounts = vec![
            make_account("root", None, 1),
            make_account("child_a", Some("root"), 3),
            make_account("child_b", Some("root"), 1),
        ];

        let shares = super::build_effective_shares(&accounts);
        let a_share = shares.get("child_a").copied().unwrap();
        let b_share = shares.get("child_b").copied().unwrap();
        assert!(
            (a_share - 0.75).abs() < 1e-9,
            "child_a (weight 3/4) should get 0.75, got {a_share}"
        );
        assert!(
            (b_share - 0.25).abs() < 1e-9,
            "child_b (weight 1/4) should get 0.25, got {b_share}"
        );

        let usage = vec![
            UsageRecord {
                user_name: "alice".into(),
                account: "child_a".into(),
                cpu_seconds: 100_000,
                gpu_seconds: 0,

                job_count: 1,
                period_start: now - Duration::days(1),
            },
            UsageRecord {
                user_name: "bob".into(),
                account: "child_b".into(),
                cpu_seconds: 100_000,
                gpu_seconds: 0,

                job_count: 1,
                period_start: now - Duration::days(1),
            },
        ];

        let mut user_weights = HashMap::new();
        user_weights.insert(("alice".into(), "child_a".into()), 1);
        user_weights.insert(("bob".into(), "child_b".into()), 1);

        let factors = compute_fairshare(
            &usage,
            &accounts,
            &user_weights,
            14,
            now,
            &BillingWeights::default(),
            &HashMap::new(),
        );
        let alice = *factors.get(&("alice".into(), "child_a".into())).unwrap();
        let bob = *factors.get(&("bob".into(), "child_b".into())).unwrap();
        // Same usage but alice has 3× the share → alice's factor should be ~3× bob's.
        assert!(
            (alice / bob - 3.0).abs() < 0.1,
            "alice/bob ratio should be ~3.0, got {:.3}",
            alice / bob
        );
    }

    #[test]
    fn test_two_account_cycle_treated_as_roots() {
        let accounts = vec![
            make_account("a", Some("b"), 1),
            make_account("b", Some("a"), 1),
        ];

        let shares = super::build_effective_shares(&accounts);
        assert!(
            shares.contains_key("a"),
            "cyclic account 'a' must still get a share"
        );
        assert!(
            shares.contains_key("b"),
            "cyclic account 'b' must still get a share"
        );
        let a = shares.get("a").copied().unwrap();
        let b = shares.get("b").copied().unwrap();
        assert!(
            (a - 0.5).abs() < 1e-9,
            "equal-weight cycle members should each get 0.5, got a={a}"
        );
        assert!(
            (b - 0.5).abs() < 1e-9,
            "equal-weight cycle members should each get 0.5, got b={b}"
        );
    }

    #[test]
    fn test_self_parent_treated_as_root() {
        let accounts = vec![
            make_account("self_ref", Some("self_ref"), 1),
            make_account("normal", None, 1),
        ];

        let shares = super::build_effective_shares(&accounts);
        assert!(
            shares.contains_key("self_ref"),
            "self-parent account must still get a share"
        );
        let sr = shares.get("self_ref").copied().unwrap();
        assert!(sr > 0.0, "self-parent share must be positive, got {sr}");
    }

    #[test]
    fn test_gpu_seconds_affect_concrete_factor() {
        let now = Utc::now();
        let accounts = vec![make_account("dev", None, 1)];
        let mut user_weights = HashMap::new();
        user_weights.insert(("gpu_user".into(), "dev".into()), 1);
        user_weights.insert(("cpu_user".into(), "dev".into()), 1);

        // gpu_user: 10k CPU + 90k GPU = 100k billable
        // cpu_user: 10k CPU + 0 GPU = 10k billable
        let usage = vec![
            UsageRecord {
                user_name: "gpu_user".into(),
                account: "dev".into(),
                cpu_seconds: 10_000,
                gpu_seconds: 90_000,

                job_count: 5,
                period_start: now - Duration::days(1),
            },
            UsageRecord {
                user_name: "cpu_user".into(),
                account: "dev".into(),
                cpu_seconds: 10_000,
                gpu_seconds: 0,

                job_count: 5,
                period_start: now - Duration::days(1),
            },
        ];

        let factors = compute_fairshare(
            &usage,
            &accounts,
            &user_weights,
            14,
            now,
            &BillingWeights::default(),
            &HashMap::new(),
        );
        let gpu = *factors.get(&("gpu_user".into(), "dev".into())).unwrap();
        let cpu = *factors.get(&("cpu_user".into(), "dev".into())).unwrap();
        // cpu_user used 10× less → factor should be ~10× higher
        assert!(
            cpu / gpu > 5.0,
            "cpu_user factor should be much higher than gpu_user, ratio={:.2}",
            cpu / gpu
        );
    }

    #[test]
    fn gpu_only_weights_ignore_cpu_gaming() {
        let now = Utc::now();
        let accounts = vec![make_account("dev", None, 1)];
        let mut user_weights = HashMap::new();
        user_weights.insert(("honest".into(), "dev".into()), 1);
        user_weights.insert(("gamer".into(), "dev".into()), 1);

        // Both users hold 8 GPUs for the same duration.
        // "honest" declares 236 CPUs; "gamer" declares 1.
        let usage = vec![
            UsageRecord {
                user_name: "honest".into(),
                account: "dev".into(),
                cpu_seconds: 20_390_400,
                gpu_seconds: 691_200,

                job_count: 1,
                period_start: now - Duration::days(1),
            },
            UsageRecord {
                user_name: "gamer".into(),
                account: "dev".into(),
                cpu_seconds: 86_400,
                gpu_seconds: 691_200,

                job_count: 1,
                period_start: now - Duration::days(1),
            },
        ];

        // With default 1:1 weights, "honest" is heavily penalized
        let factors_default = compute_fairshare(
            &usage,
            &accounts,
            &user_weights,
            14,
            now,
            &BillingWeights::default(),
            &HashMap::new(),
        );
        let honest_default = *factors_default
            .get(&("honest".into(), "dev".into()))
            .unwrap();
        let gamer_default = *factors_default
            .get(&("gamer".into(), "dev".into()))
            .unwrap();
        assert!(
            gamer_default / honest_default > 5.0,
            "with default weights, gamer should have much higher factor (gaming works), ratio={:.2}",
            gamer_default / honest_default
        );

        // With GPU-only weights, both users are equal
        let gpu_only = BillingWeights::parse("CPU=0.0,GRES/gpu=1.0");
        let factors_gpu = compute_fairshare(
            &usage,
            &accounts,
            &user_weights,
            14,
            now,
            &gpu_only,
            &HashMap::new(),
        );
        let honest_gpu = *factors_gpu.get(&("honest".into(), "dev".into())).unwrap();
        let gamer_gpu = *factors_gpu.get(&("gamer".into(), "dev".into())).unwrap();
        let ratio = (honest_gpu / gamer_gpu).abs();
        assert!(
            (ratio - 1.0).abs() < 0.01,
            "with GPU-only weights, both users should have equal factors, ratio={:.4}",
            ratio
        );
    }

    #[test]
    fn test_fairshare_offset_zeroes_usage() {
        let now = Utc::now();
        let usage = vec![UsageRecord {
            user_name: "alice".into(),
            account: "research".into(),
            period_start: now - Duration::hours(1),
            cpu_seconds: 10000,
            gpu_seconds: 0,
            job_count: 1,
        }];
        let accounts = vec![super::super::db::AccountRecord {
            name: "research".into(),
            description: String::new(),
            organization: String::new(),
            parent: None,
            fairshare_weight: 100,
            max_running_jobs: None,
            grp_tres: None,
        }];
        let mut user_weights = HashMap::new();
        user_weights.insert(("alice".into(), "research".into()), 1);

        let mut offsets = HashMap::new();
        offsets.insert(("alice".into(), "research".into()), (-10000_i64, 0_i64));

        let factors = compute_fairshare(
            &usage,
            &accounts,
            &user_weights,
            14,
            now,
            &BillingWeights::default(),
            &offsets,
        );
        let factor = *factors.get(&("alice".into(), "research".into())).unwrap();
        assert!(
            factor >= 9.0,
            "offset should zero out usage, giving max fairshare factor; got {factor}"
        );
    }

    #[test]
    fn test_fairshare_offset_partial() {
        let now = Utc::now();
        let usage = vec![
            UsageRecord {
                user_name: "alice".into(),
                account: "research".into(),
                period_start: now - Duration::hours(1),
                cpu_seconds: 10000,
                gpu_seconds: 0,
                job_count: 1,
            },
            UsageRecord {
                user_name: "bob".into(),
                account: "research".into(),
                period_start: now - Duration::hours(1),
                cpu_seconds: 10000,
                gpu_seconds: 0,
                job_count: 1,
            },
        ];
        let accounts = vec![super::super::db::AccountRecord {
            name: "research".into(),
            description: String::new(),
            organization: String::new(),
            parent: None,
            fairshare_weight: 100,
            max_running_jobs: None,
            grp_tres: None,
        }];
        let mut user_weights = HashMap::new();
        user_weights.insert(("alice".into(), "research".into()), 1);
        user_weights.insert(("bob".into(), "research".into()), 1);

        let no_offset = compute_fairshare(
            &usage,
            &accounts,
            &user_weights,
            14,
            now,
            &BillingWeights::default(),
            &HashMap::new(),
        );
        let mut offsets = HashMap::new();
        offsets.insert(("alice".into(), "research".into()), (-5000_i64, 0_i64));
        let with_offset = compute_fairshare(
            &usage,
            &accounts,
            &user_weights,
            14,
            now,
            &BillingWeights::default(),
            &offsets,
        );

        let f_no = *no_offset.get(&("alice".into(), "research".into())).unwrap();
        let f_with = *with_offset
            .get(&("alice".into(), "research".into()))
            .unwrap();
        assert!(
            f_with > f_no,
            "partial offset should improve alice's fairshare factor; no_offset={f_no}, with_offset={f_with}"
        );
    }
}
