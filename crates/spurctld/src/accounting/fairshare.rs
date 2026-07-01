// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use chrono::{DateTime, Duration, Utc};

use super::db::UsageRecord;

/// Compute fair-share factors for all users from usage data.
///
/// Returns a map of (user, account) → fair-share factor.
pub(super) fn compute_fairshare(
    usage: &[UsageRecord],
    account_weights: &std::collections::HashMap<String, f64>,
    halflife_days: u32,
    now: DateTime<Utc>,
) -> std::collections::HashMap<(String, String), f64> {
    let total_weight: f64 = account_weights.values().sum();
    if total_weight <= 0.0 {
        return std::collections::HashMap::new();
    }

    let halflife = Duration::days(halflife_days as i64);
    let decay_rate = 2.0_f64.ln() / halflife.num_seconds() as f64;

    let mut user_usage: std::collections::HashMap<(String, String), f64> =
        std::collections::HashMap::new();

    for record in usage {
        let age = (now - record.period_start).num_seconds().max(0) as f64;
        let decay = (-decay_rate * age).exp();
        let weighted_usage = record.cpu_seconds as f64 * decay;

        *user_usage
            .entry((record.user_name.clone(), record.account.clone()))
            .or_insert(0.0) += weighted_usage;
    }

    let total_usage: f64 = user_usage.values().sum();
    let epsilon = 0.001;

    let mut factors = std::collections::HashMap::new();

    for ((user, account), usage) in &user_usage {
        let target_share = account_weights.get(account).copied().unwrap_or(1.0) / total_weight;
        let actual_share = usage / total_usage.max(epsilon);

        // factor > 1.0 = underusing allocation (boosted priority)
        // factor < 1.0 = overusing allocation (penalized)
        let factor = target_share / actual_share.max(epsilon);

        factors.insert((user.clone(), account.clone()), factor.min(100.0));
    }

    factors
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_compute_fairshare() {
        let now = Utc::now();
        let usage = vec![
            UsageRecord {
                user_name: "alice".into(),
                account: "research".into(),
                cpu_seconds: 100_000,
                gpu_seconds: 0,
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

        let mut weights = HashMap::new();
        weights.insert("research".into(), 1.0);
        weights.insert("engineering".into(), 1.0);

        let factors = compute_fairshare(&usage, &weights, 14, now);

        let alice_factor = factors.get(&("alice".into(), "research".into())).unwrap();
        let bob_factor = factors.get(&("bob".into(), "engineering".into())).unwrap();
        // Bob used less than his share → boosted above 1.0
        assert!(*bob_factor > 1.0);
        // Alice used more than her share → penalized below 1.0
        assert!(*alice_factor < 1.0);
        assert!(bob_factor > alice_factor);
    }
}
