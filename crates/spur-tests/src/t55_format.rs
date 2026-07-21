// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! T55: CLI format string engine.
//!
//! Tests the Slurm-compatible %X format string parser and renderer.
//! Corresponds to format conformance testing in Slurm's test5.x and test4.x.

#[cfg(test)]
mod tests {
    // We need to reference format_engine from spur-cli, but since it's a binary
    // crate we can't directly depend on it. Instead, test the format engine
    // concepts through spur-core types and expected output patterns.

    // For now, test the format helpers available through spur-core.
    use spur_core::config;

    // ── T55.1: squeue default format parsing ─────────────────────

    #[test]
    fn t55_1_time_format_hours() {
        assert_eq!(config::format_time(Some(90)), "01:30:00");
    }

    #[test]
    fn t55_2_time_format_days() {
        assert_eq!(config::format_time(Some(1500)), "1-01:00:00");
    }

    #[test]
    fn t55_3_time_format_unlimited() {
        assert_eq!(config::format_time(None), "UNLIMITED");
    }

    #[test]
    fn t55_4_time_format_zero() {
        assert_eq!(config::format_time(Some(0)), "00:00:00");
    }

    #[test]
    fn t55_5_time_format_one_day() {
        assert_eq!(config::format_time(Some(1440)), "1-00:00:00");
    }

    #[test]
    fn t55_6_time_format_many_days() {
        assert_eq!(config::format_time(Some(14400)), "10-00:00:00");
    }

    // ── T55.7: State display conformance ─────────────────────────

    #[test]
    fn t55_7_job_state_display_matches_slurm() {
        use spur_core::job::JobState;
        let expected: [(&str, &str); JobState::COUNT] = [
            ("PD", "PENDING"),
            ("R", "RUNNING"),
            ("CG", "COMPLETING"),
            ("CD", "COMPLETED"),
            ("F", "FAILED"),
            ("CA", "CANCELLED"),
            ("TO", "TIMEOUT"),
            ("NF", "NODE_FAIL"),
            ("PR", "PREEMPTED"),
            ("S", "SUSPENDED"),
            ("DL", "DEADLINE"),
            ("OOM", "OUT_OF_MEMORY"),
        ];
        assert_eq!(JobState::ALL.len(), expected.len());
        for (i, state) in JobState::ALL.iter().enumerate() {
            let (code, name) = expected[i];
            assert_eq!(state.code(), code, "code mismatch for {state:?}");
            assert_eq!(state.display(), name, "display mismatch for {state:?}");
        }
    }

    #[test]
    fn t55_8_node_state_display_matches_slurm() {
        use spur_core::node::NodeState;
        let expected: [&str; NodeState::COUNT] = [
            "idle",
            "allocated",
            "mixed",
            "down",
            "drained",
            "draining",
            "error",
            "unknown",
            "suspended",
        ];
        for (i, state) in NodeState::ALL.iter().enumerate() {
            assert_eq!(
                state.display(),
                expected[i],
                "display mismatch for {state:?}"
            );
        }
    }

    #[test]
    fn t55_9_node_state_short_matches_slurm() {
        use spur_core::node::NodeState;
        let expected: [&str; NodeState::COUNT] = [
            "idle", "alloc", "mix", "down", "drain", "drng", "err", "unk", "susp",
        ];
        for (i, state) in NodeState::ALL.iter().enumerate() {
            assert_eq!(state.short(), expected[i], "short mismatch for {state:?}");
        }
    }

    // ── T55.10: Pending reason display ───────────────────────────

    #[test]
    fn t55_10_pending_reasons_match_slurm() {
        use spur_core::job::PendingReason;
        let expected = vec![
            (PendingReason::None, "None"),
            (PendingReason::Priority, "Priority"),
            (PendingReason::Resources, "Resources"),
            (PendingReason::Dependency, "Dependency"),
            (PendingReason::Held, "JobHeldUser"),
            // Reason-code vocabulary parity (Slurm 25.11.6 job_reason_string).
            (PendingReason::Reservation, "Reservation"),
            (
                PendingReason::ReservedMaintenance,
                "ReqNodeNotAvail, Reserved for maintenance",
            ),
            (PendingReason::ReservationDeleted, "ReservationDeleted"),
            (PendingReason::JobHoldMaxRequeue, "JobHoldMaxRequeue"),
            (PendingReason::QosMaxCpuPerJobLimit, "QOSMaxCpuPerJobLimit"),
            (
                PendingReason::QosMaxWallDurationPerJobLimit,
                "QOSMaxWallDurationPerJobLimit",
            ),
            (PendingReason::QosMaxMemoryPerJob, "QOSMaxMemoryPerJob"),
            (
                PendingReason::QosMaxCpuPerUserLimit,
                "QOSMaxCpuPerUserLimit",
            ),
            (
                PendingReason::QosMaxSubmitJobPerUserLimit,
                "QOSMaxSubmitJobPerUserLimit",
            ),
            (
                PendingReason::QosMaxNodePerUserLimit,
                "QOSMaxNodePerUserLimit",
            ),
            (PendingReason::QosMaxMemoryPerUser, "QOSMaxMemoryPerUser"),
            (PendingReason::AssocMaxJobsLimit, "AssocMaxJobsLimit"),
            (
                PendingReason::AssocMaxSubmitJobLimit,
                "AssocMaxSubmitJobLimit",
            ),
            (
                PendingReason::AssocMaxCpuPerJobLimit,
                "AssocMaxCpuPerJobLimit",
            ),
            (
                PendingReason::AssocMaxNodePerJobLimit,
                "AssocMaxNodePerJobLimit",
            ),
            (PendingReason::AssocMaxMemPerJob, "AssocMaxMemPerJob"),
            (PendingReason::AssocGrpCpuLimit, "AssocGrpCpuLimit"),
            (PendingReason::AssocGrpNodeLimit, "AssocGrpNodeLimit"),
            (PendingReason::AssocGrpMemLimit, "AssocGrpMemLimit"),
            (
                PendingReason::AssocMaxWallDurationPerJobLimit,
                "AssocMaxWallDurationPerJobLimit",
            ),
        ];
        for (reason, display) in expected {
            assert_eq!(reason.display(), display);
        }
    }
}
