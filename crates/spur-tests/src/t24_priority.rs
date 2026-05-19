// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! T24: Priority and fair-share.
//!
//! Corresponds to Slurm's test24.x series.

#[cfg(test)]
mod tests {
    use spur_sched::priority;

    // ── T24.4: Effective priority ────────────────────────────────

    #[test]
    fn t24_4_base_priority() {
        let p = priority::effective_priority(1000, 1.0, 0, 1);
        assert_eq!(p, 1000);
    }

    #[test]
    fn t24_5_age_bonus() {
        let p_new = priority::effective_priority(1000, 1.0, 0, 1);
        let p_old = priority::effective_priority(1000, 1.0, 10080, 1); // 7 days
        assert!(p_old > p_new, "older jobs should get priority boost");
        assert_eq!(p_old, 2000); // Max age bonus = 2x at 7 days
    }

    #[test]
    fn t24_6_partition_tier() {
        let p_low = priority::effective_priority(1000, 1.0, 0, 1);
        let p_high = priority::effective_priority(1000, 1.0, 0, 3);
        assert_eq!(p_high, p_low * 3);
    }

    #[test]
    fn t24_7_fair_share_boost() {
        let p_overuser = priority::effective_priority(1000, 0.5, 0, 1);
        let p_underuser = priority::effective_priority(1000, 2.0, 0, 1);
        assert!(p_underuser > p_overuser);
    }

    #[test]
    fn t24_8_priority_never_zero() {
        let p = priority::effective_priority(0, 0.001, 0, 1);
        assert!(p >= 1);
    }

    // ── T24.9: Fair-share cap ────────────────────────────────────

    #[test]
    fn t24_9_fair_share_capped() {
        // Even with zero usage, fair-share factor should be capped
        let p = priority::effective_priority(1000, 100.0, 0, 1);
        // Cap is 10.0 in the function
        assert_eq!(p, 10_000);
    }

    // ── T24.10: Combined factors ─────────────────────────────────

    #[test]
    fn t24_10_combined_priority() {
        // fair_share=2.0, age=3 days (3*24*60=4320 mins → factor 1.428), tier=2
        let p = priority::effective_priority(1000, 2.0, 4320, 2);
        // 1000 * 2.0 * (1 + 4320/10080) * 2 = 1000 * 2.0 * 1.4286 * 2 ≈ 5714
        assert!(p > 5000 && p < 6000);
    }
}
