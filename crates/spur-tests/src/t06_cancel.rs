// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! T06: Job cancellation and control.
//!
//! Corresponds to Slurm's test6.x series.
//! Tests hold, release, cancel, state transitions.

#[cfg(test)]
mod tests {
    use crate::harness::*;
    use spur_core::job::*;

    // ── T06.1: Cancel pending job ────────────────────────────────

    #[test]
    fn t06_1_cancel_pending() {
        let mut job = make_job("test");
        assert_transition_ok(&mut job, JobState::Cancelled);
        assert!(job.state.is_terminal());
    }

    // ── T06.2: Cancel running job ────────────────────────────────

    #[test]
    fn t06_2_cancel_running() {
        let mut job = make_job("test");
        assert_transition_ok(&mut job, JobState::Running);
        assert_transition_ok(&mut job, JobState::Cancelled);
        assert!(job.state.is_terminal());
    }

    // ── T06.3: Cannot cancel completed ───────────────────────────

    #[test]
    fn t06_3_cannot_cancel_completed() {
        let mut job = make_job("test");
        assert_transition_ok(&mut job, JobState::Running);
        assert_transition_ok(&mut job, JobState::Completed);
        assert_transition_err(&mut job, JobState::Cancelled);
    }

    // ── T06.6: Deadline from pending ─────────────────────────────

    #[test]
    fn t06_6_deadline_from_pending() {
        let mut job = make_job("test");
        assert_transition_ok(&mut job, JobState::Deadline);
        assert!(job.state.is_terminal());
        assert!(!job.state.is_active());
    }

    // ── T06.7: Cannot deadline a running job ─────────────────────

    #[test]
    fn t06_7_cannot_deadline_running() {
        let mut job = make_job("test");
        assert_transition_ok(&mut job, JobState::Running);
        assert_transition_err(&mut job, JobState::Deadline);
    }

    // ── T06.8: Cannot deadline a completed job ───────────────────

    #[test]
    fn t06_8_cannot_deadline_completed() {
        let mut job = make_job("test");
        assert_transition_ok(&mut job, JobState::Running);
        assert_transition_ok(&mut job, JobState::Completed);
        assert_transition_err(&mut job, JobState::Deadline);
    }

    // ── T06.4: Hold sets pending reason ──────────────────────────

    #[test]
    fn t06_4_hold_job() {
        let job = Job::new(
            1,
            JobSpec {
                name: "held".into(),
                user: "test".into(),
                hold: true,
                ..Default::default()
            },
        );
        assert_eq!(job.state, JobState::Pending);
        assert_eq!(job.pending_reason, PendingReason::Held);
        assert_eq!(job.priority, 0);
    }

    // ── T06.5: Held jobs not in schedulable pending ──────────────

    #[test]
    fn t06_5_held_not_schedulable() {
        let mut held = make_job("held");
        held.pending_reason = PendingReason::Held;
        held.priority = 0;
        let normal = make_job("normal");

        let jobs = [held.clone(), normal.clone()];
        let schedulable: Vec<_> = jobs
            .iter()
            .filter(|j| j.state == JobState::Pending && j.pending_reason != PendingReason::Held)
            .collect();
        assert_eq!(schedulable.len(), 1);
        assert_eq!(schedulable[0].spec.name, "normal");
    }
}
