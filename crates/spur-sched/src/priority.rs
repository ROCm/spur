// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

/// Calculate effective job priority.
///
/// effective_priority = base_priority * fair_share_factor * age_factor * partition_tier
pub fn effective_priority(
    base_priority: u32,
    fair_share: f64,
    age_minutes: i64,
    partition_tier: u32,
) -> u32 {
    let age_factor = 1.0 + (age_minutes as f64 / 10080.0).min(1.0); // Max age bonus at 7 days
    let raw = base_priority as f64
        * fair_share.min(10.0) // Cap fair-share boost
        * age_factor
        * partition_tier.max(1) as f64;

    (raw as u32).max(1) // Never zero
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_priority_cases() {
        let cases = [
            ((1000, 1.0, 0, 1), 1000),
            ((1000, 1.0, 10080, 1), 2000),
            ((1000, 1.0, 0, 2), 2000),
            ((1000, 100.0, 0, 1), 10_000),
            ((0, 0.001, 0, 1), 1),
        ];

        for ((base, fs, age, tier), expected) in cases {
            let got = effective_priority(base, fs, age, tier);
            assert_eq!(
                got, expected,
                "effective_priority({base}, {fs}, {age}, {tier})"
            );
        }
    }
}
