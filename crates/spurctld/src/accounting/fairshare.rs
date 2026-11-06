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
        let user_cap = if *user_share > 0.0 {
            (*user_share * FAIRSHARE_CAP).max(1.0)
        } else {
            0.0
        };

        let usage_val = user_usage.get(key).copied().unwrap_or(0.0);
        let factor = if usage_val < epsilon {
            user_cap
        } else {
            let actual_share = usage_val / total_usage.max(epsilon);
            (user_share / actual_share).min(user_cap)
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

        let alice_factor = *factors.get(&("alice".into(), "research".into())).unwrap();
        let bob_factor = *factors.get(&("bob".into(), "engineering".into())).unwrap();

        // Each account has share 0.5 → per-user cap = 5.0.
        // Both records share the same period_start, so decay cancels.
        // alice: 150k, bob: 10k, total = 160k.
        // alice actual_share = 150k/160k = 0.9375 → factor = 0.5/0.9375 = 0.5333...
        // bob   actual_share =  10k/160k = 0.0625 → factor = 0.5/0.0625 = 8.0 → capped at 5.0.
        assert!(
            (bob_factor - 5.0).abs() < 1e-9,
            "bob should be exactly 5.0 (capped), got {bob_factor}"
        );
        let expected_alice = 0.5 / 0.9375;
        assert!(
            (alice_factor - expected_alice).abs() < 1e-9,
            "alice should be {expected_alice}, got {alice_factor}"
        );
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

    #[test]
    fn test_zero_usage_proportional_to_share() {
        let now = Utc::now();
        let accounts = vec![make_account("big", None, 3), make_account("small", None, 1)];
        let user_weights: HashMap<(String, String), i32> = vec![
            (("u_big".into(), "big".into()), 1),
            (("u_small".into(), "small".into()), 1),
        ]
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
        let big = factors[&("u_big".to_string(), "big".to_string())];
        let small = factors[&("u_small".to_string(), "small".to_string())];
        assert!(
            big > small,
            "idle user with bigger share must get higher factor: big={big}, small={small}"
        );
        let expected_big = (0.75_f64 * 10.0).max(1.0);
        let expected_small = (0.25_f64 * 10.0).max(1.0);
        assert!(
            (big - expected_big).abs() < 1e-9,
            "big share idle factor should be {expected_big}, got {big}"
        );
        assert!(
            (small - expected_small).abs() < 1e-9,
            "small share idle factor should be {expected_small}, got {small}"
        );
    }

    #[test]
    fn test_sole_user_gets_full_cap() {
        let now = Utc::now();
        let accounts = vec![make_account("funded", None, 1)];

        let user_weights: HashMap<(String, String), i32> =
            vec![(("u1".into(), "funded".into()), 1)]
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
        let u1 = factors[&("u1".to_string(), "funded".to_string())];
        assert_eq!(u1, 10.0, "sole user (share=1.0) idle factor must be 10.0");
    }

    #[test]
    fn test_unfunded_account_stays_zero() {
        let now = Utc::now();
        let accounts = vec![
            make_account("funded", None, 1),
            make_account("unfunded", None, 0),
        ];

        let user_weights: HashMap<(String, String), i32> = vec![
            (("u1".into(), "funded".into()), 1),
            (("u2".into(), "unfunded".into()), 1),
        ]
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
        let u2 = factors[&("u2".to_string(), "unfunded".to_string())];
        assert_eq!(u2, 0.0, "zero-share zero-usage user must stay at 0");
    }

    #[test]
    fn test_twenty_users_low_share() {
        let now = Utc::now();
        let accounts = vec![make_account("team", None, 1)];
        let mut user_weights = HashMap::new();
        for i in 0..20 {
            user_weights.insert((format!("u{i}"), "team".to_string()), 1);
        }
        // Each user share = 1.0/20 = 0.05 → user_cap = max(0.05*10, 1.0) = 1.0
        // total_usage = 100_000
        // u0: idle       → factor = user_cap = 1.0
        // u1: 1% (1000)  → actual_share = 1000/100000 = 0.01,  factor = 0.05/0.01 = 5.0 → cap 1.0
        // u2: 4% (4000)  → actual_share = 4000/100000 = 0.04,  factor = 0.05/0.04 = 1.25 → cap 1.0
        // u3: 5% (5000)  → actual_share = 5000/100000 = 0.05,  factor = 0.05/0.05 = 1.0
        // u4: 10% (10000)→ actual_share = 10000/100000 = 0.1,   factor = 0.05/0.1 = 0.5
        // u5: 80% (80000)→ actual_share = 80000/100000 = 0.8,   factor = 0.05/0.8 = 0.0625
        let record = |user: &str, secs: i64| UsageRecord {
            user_name: user.into(),
            account: "team".into(),
            cpu_seconds: secs,
            gpu_seconds: 0,
            job_count: 1,
            period_start: now,
        };
        let usage = vec![
            record("u1", 1_000),
            record("u2", 4_000),
            record("u3", 5_000),
            record("u4", 10_000),
            record("u5", 80_000),
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
        let f = |u: &str| factors[&(u.to_string(), "team".to_string())];

        assert!(
            (f("u0") - 1.0).abs() < 1e-9,
            "idle user (share 0.05) must get 1.0, got {}",
            f("u0")
        );
        assert!(
            (f("u1") - 1.0).abs() < 1e-9,
            "1% user must be capped at 1.0, got {}",
            f("u1")
        );
        assert!(
            (f("u2") - 1.0).abs() < 1e-9,
            "4% user must be capped at 1.0, got {}",
            f("u2")
        );
        assert!(
            (f("u3") - 1.0).abs() < 1e-9,
            "5% user (at share) must get 1.0, got {}",
            f("u3")
        );
        assert!(
            (f("u4") - 0.5).abs() < 1e-9,
            "10% user must get 0.5, got {}",
            f("u4")
        );
        assert!(
            (f("u5") - 0.0625).abs() < 1e-9,
            "80% user must get 0.0625, got {}",
            f("u5")
        );
    }

    #[test]
    fn test_tiny_share_idle_equals_near_zero_usage() {
        let now = Utc::now();
        let mut accounts = vec![make_account("team", None, 1)];
        for i in 0..19 {
            accounts.push(make_account(&format!("other{i}"), None, 1));
        }
        // 20 accounts × 100 users in "team" = share 0.05/100 = 0.0005
        let mut user_weights = HashMap::new();
        for i in 0..100 {
            user_weights.insert((format!("u{i}"), "team".to_string()), 1);
        }
        let record = |user: &str, account: &str, secs: i64| UsageRecord {
            user_name: user.into(),
            account: account.into(),
            cpu_seconds: secs,
            gpu_seconds: 0,
            job_count: 1,
            period_start: now - Duration::hours(1),
        };
        let usage = vec![
            record("heavy", "other0", 1_000_000_000),
            record("u1", "team", 1),
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
        let idle_user = factors[&("u0".to_string(), "team".to_string())];
        let one_sec = factors[&("u1".to_string(), "team".to_string())];
        // share = 0.0005, cap = max(0.0005*10, 1.0) = 1.0
        assert_eq!(
            idle_user, 1.0,
            "tiny-share idle user must get floor cap 1.0, got {idle_user}"
        );
        assert_eq!(
            idle_user, one_sec,
            "idle and 1-second user must rank equally at the cap: idle={idle_user}, one_sec={one_sec}"
        );
    }
}
