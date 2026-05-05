use chrono::{DateTime, Duration, Utc};

use crate::db::UsageRecord;

/// Compute fair-share factors for all users from usage data.
///
/// Returns a map of (user, account) → fair-share factor.
pub fn compute_fairshare(
    usage: &[UsageRecord],
    account_weights: &std::collections::HashMap<String, f64>,
    halflife_days: u32,
    now: DateTime<Utc>,
) -> std::collections::HashMap<(String, String), f64> {
    let halflife = Duration::days(halflife_days as i64);
    let decay_rate = 2.0_f64.ln() / halflife.num_seconds() as f64;

    let mut user_usage: std::collections::HashMap<(String, String), f64> =
        std::collections::HashMap::new();

    // Sum decayed usage per user+account
    for record in usage {
        let age = (now - record.period_start).num_seconds().max(0) as f64;
        let decay = (-decay_rate * age).exp();
        let weighted_usage = record.cpu_seconds as f64 * decay;

        *user_usage
            .entry((record.user_name.clone(), record.account.clone()))
            .or_insert(0.0) += weighted_usage;
    }

    // Compute total usage across all users
    let total_usage: f64 = user_usage.values().sum();
    let epsilon = 0.001;

    // Compute fair-share factors
    let mut factors = std::collections::HashMap::new();

    for ((user, account), usage) in &user_usage {
        let target_share = account_weights.get(account).copied().unwrap_or(1.0);

        // Normalize: actual_share = user_usage / total_usage
        let actual_share = usage / total_usage.max(epsilon);

        // fair_share = target_share / actual_share
        // High value = underusing allocation → higher priority
        // Low value = overusing allocation → lower priority
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
                account: "research".into(),
                cpu_seconds: 10_000,
                gpu_seconds: 0,
                job_count: 2,
                period_start: now - Duration::days(1),
            },
        ];

        let mut weights = HashMap::new();
        weights.insert("research".into(), 1.0);

        let factors = compute_fairshare(&usage, &weights, 14, now);

        // Bob should have higher factor (used less)
        let alice_factor = factors.get(&("alice".into(), "research".into())).unwrap();
        let bob_factor = factors.get(&("bob".into(), "research".into())).unwrap();
        assert!(bob_factor > alice_factor);
    }
}
