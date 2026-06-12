// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! T60: Job suspend/resume state machine & accounting.
//!
//! Covers suspend/resume guards and run-time accounting that exclude
//! suspended intervals. The basic Running→Suspended→Running happy path is
//! already covered by t50_9; these tests focus on guards and accounting.

#[cfg(test)]
mod tests {
    use crate::harness::*;
    use spur_core::job::*;

    // ── T60.1: Suspend running then resume ───────────────────────

    #[test]
    fn t60_1_suspend_running_then_resume() {
        let mut job = make_job("test");
        assert_transition_ok(&mut job, JobState::Running);
        assert_transition_ok(&mut job, JobState::Suspended);
        assert!(job.state.is_active());
        assert!(!job.state.is_terminal());
        assert_transition_ok(&mut job, JobState::Running);
    }

    // ── T60.2: Cannot suspend pending ────────────────────────────

    #[test]
    fn t60_2_cannot_suspend_pending() {
        let mut job = make_job("test");
        assert_transition_err(&mut job, JobState::Suspended);
    }

    // ── T60.3: Cannot suspend completed ──────────────────────────

    #[test]
    fn t60_3_cannot_suspend_completed() {
        let mut job = make_job("test");
        assert_transition_ok(&mut job, JobState::Running);
        assert_transition_ok(&mut job, JobState::Completed);
        assert_transition_err(&mut job, JobState::Suspended);
    }

    // ── T60.4: Suspended can be cancelled ────────────────────────

    #[test]
    fn t60_4_suspended_can_be_cancelled() {
        let mut job = make_job("test");
        assert_transition_ok(&mut job, JobState::Running);
        assert_transition_ok(&mut job, JobState::Suspended);
        assert_transition_ok(&mut job, JobState::Cancelled);
        assert!(job.state.is_terminal());
    }

    // ── T60.5: Cannot resume a running job (Running→Running) ──────

    #[test]
    fn t60_5_cannot_resume_running() {
        let mut job = make_job("test");
        assert_transition_ok(&mut job, JobState::Running);
        assert_transition_err(&mut job, JobState::Running);
    }

    // ── T60.6: Run time excludes suspended interval ──────────────

    #[test]
    fn t60_6_run_time_excludes_suspended() {
        let mut job = make_job("test");
        let start = chrono::Utc::now();
        job.start_time = Some(start);
        job.end_time = Some(start + chrono::Duration::seconds(100));
        job.suspended_secs = 30;
        let rt = job.run_time().unwrap().num_seconds();
        assert!((68..=72).contains(&rt), "expected ~70s, got {rt}");
    }

    // ── T60.7: Suspended process dying out-of-band can finalize ──

    #[test]
    fn t60_7_suspended_can_finalize_on_death() {
        // A suspended job whose process dies (OOM, external kill, node loss)
        // must finalize rather than strand in SUSPENDED.
        for terminal in [
            JobState::Completed,
            JobState::Failed,
            JobState::NodeFail,
            JobState::Timeout,
        ] {
            let mut job = make_job("test");
            assert_transition_ok(&mut job, JobState::Running);
            assert_transition_ok(&mut job, JobState::Suspended);
            assert_transition_ok(&mut job, terminal);
            assert!(job.state.is_terminal());
        }
    }
}
