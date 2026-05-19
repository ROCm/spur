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
        // These must match Slurm's output exactly
        let expected = vec![
            (JobState::Pending, "PD", "PENDING"),
            (JobState::Running, "R", "RUNNING"),
            (JobState::Completing, "CG", "COMPLETING"),
            (JobState::Completed, "CD", "COMPLETED"),
            (JobState::Failed, "F", "FAILED"),
            (JobState::Cancelled, "CA", "CANCELLED"),
            (JobState::Timeout, "TO", "TIMEOUT"),
            (JobState::NodeFail, "NF", "NODE_FAIL"),
            (JobState::Preempted, "PR", "PREEMPTED"),
            (JobState::Suspended, "S", "SUSPENDED"),
        ];
        for (state, code, name) in expected {
            assert_eq!(state.code(), code, "code mismatch for {:?}", state);
            assert_eq!(state.display(), name, "display mismatch for {:?}", state);
        }
    }

    #[test]
    fn t55_8_node_state_display_matches_slurm() {
        use spur_core::node::NodeState;
        let expected = vec![
            (NodeState::Idle, "idle"),
            (NodeState::Allocated, "allocated"),
            (NodeState::Mixed, "mixed"),
            (NodeState::Down, "down"),
            (NodeState::Drain, "drained"),
            (NodeState::Draining, "draining"),
            (NodeState::Error, "error"),
            (NodeState::Unknown, "unknown"),
        ];
        for (state, name) in expected {
            assert_eq!(state.display(), name);
        }
    }

    #[test]
    fn t55_9_node_state_short_matches_slurm() {
        use spur_core::node::NodeState;
        let expected = vec![
            (NodeState::Idle, "idle"),
            (NodeState::Allocated, "alloc"),
            (NodeState::Mixed, "mix"),
            (NodeState::Down, "down"),
            (NodeState::Drain, "drain"),
            (NodeState::Draining, "drng"),
        ];
        for (state, short) in expected {
            assert_eq!(state.short(), short);
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
        ];
        for (reason, display) in expected {
            assert_eq!(reason.display(), display);
        }
    }
}
