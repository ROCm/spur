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

    // ── T60.8: Cannot suspend a cancelled job (B3) ───────────────

    #[test]
    fn t60_8_cannot_suspend_cancelled() {
        let mut job = make_job("test");
        assert_transition_ok(&mut job, JobState::Running);
        assert_transition_ok(&mut job, JobState::Cancelled);
        assert_transition_err(&mut job, JobState::Suspended);
    }

    // ── T60.9: Cannot suspend a failed job (B4) ──────────────────

    #[test]
    fn t60_9_cannot_suspend_failed() {
        let mut job = make_job("test");
        assert_transition_ok(&mut job, JobState::Running);
        assert_transition_ok(&mut job, JobState::Failed);
        assert_transition_err(&mut job, JobState::Suspended);
    }

    // ── T60.10: Cannot resume (reach Running from) a terminal job (B7)
    //
    // Note: resume-of-pending (B6) is guarded at the cluster method layer
    // (resume_job checks state == Suspended), since Pending->Running is itself
    // a valid *start* transition. See cluster.rs resume_*_rejects_* tests.

    #[test]
    fn t60_10_cannot_resume_terminal() {
        for terminal in [JobState::Completed, JobState::Failed, JobState::Cancelled] {
            let mut job = make_job("test");
            assert_transition_ok(&mut job, JobState::Running);
            assert_transition_ok(&mut job, terminal);
            // Resume == reach Running; a terminal job must not return to Running.
            assert_transition_err(&mut job, JobState::Running);
        }
    }

    // ── T60.11: Cannot suspend a completing job (L4) ─────────────

    #[test]
    fn t60_11_cannot_suspend_completing() {
        let mut job = make_job("test");
        assert_transition_ok(&mut job, JobState::Running);
        assert_transition_ok(&mut job, JobState::Completing);
        assert_transition_err(&mut job, JobState::Suspended);
    }

    // ── T60.12: Cancel while suspended folds the open window (D7) ─

    #[test]
    fn t60_12_cancel_while_suspended_folds_open_window() {
        // A job suspended-but-never-resumed, then cancelled, must still account
        // run_time correctly: the open suspension window (suspended_at .. end)
        // is excluded even though suspended_secs was never accumulated.
        let mut job = make_job("test");
        let start = chrono::Utc::now() - chrono::Duration::seconds(100);
        job.start_time = Some(start);
        // Ran 60s, then suspended at start+60, never resumed.
        job.suspended_at = Some(start + chrono::Duration::seconds(60));
        // Cancel "now" (== start+100): end_time set by transition.
        job.end_time = Some(start + chrono::Duration::seconds(100));
        // run_time = 100s span - 40s open suspension = 60s.
        let rt = job.run_time().unwrap().num_seconds();
        assert_eq!(rt, 60, "expected 60s effective run time, got {rt}");
    }

    // ── T60.13: Multiple suspensions accumulate (D4) ─────────────

    #[test]
    fn t60_13_multiple_suspensions_accumulate() {
        // suspended_secs is the running total across cycles; run_time excludes
        // the full accumulated amount.
        let mut job = make_job("test");
        let start = chrono::Utc::now();
        job.start_time = Some(start);
        job.end_time = Some(start + chrono::Duration::seconds(100));
        job.suspended_secs = 15 + 25; // two prior suspend/resume cycles
        let rt = job.run_time().unwrap().num_seconds();
        assert_eq!(rt, 60, "expected 100 - 40 = 60s, got {rt}");
    }
}
