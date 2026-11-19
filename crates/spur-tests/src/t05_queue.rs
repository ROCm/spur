// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! T05: Job queue viewing (squeue / spur queue).
//!
//! Corresponds to Slurm's test5.x series.
//! Tests job filtering, state matching, output formatting.

#[cfg(test)]
mod tests {
    use crate::harness::*;
    use spur_core::job::*;

    // ── T05.1: Job state classification ──────────────────────────

    #[test]
    fn t05_1_terminal_states() {
        assert!(JobState::Completed.is_terminal());
        assert!(JobState::Failed.is_terminal());
        assert!(JobState::Cancelled.is_terminal());
        assert!(JobState::Timeout.is_terminal());
        assert!(JobState::NodeFail.is_terminal());
    }

    #[test]
    fn t05_2_active_states() {
        assert!(JobState::Running.is_active());
        assert!(JobState::Completing.is_active());
        assert!(JobState::Suspended.is_active());
    }

    #[test]
    fn t05_3_pending_not_terminal_or_active() {
        assert!(!JobState::Pending.is_terminal());
        assert!(!JobState::Pending.is_active());
    }

    // ── T05.7: Pending reason ────────────────────────────────────

    #[test]
    fn t05_7_pending_reason_displayed() {
        // Initial pending reason is None (not Priority).
        // The scheduler sets the actual reason after evaluating the job.
        let job = make_job("test");
        assert_eq!(job.pending_reason.display(), "None");
    }
}
