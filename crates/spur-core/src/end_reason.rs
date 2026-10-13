// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Why a job's run ended, as recorded in accounting.

use crate::job::{Job, JobId, JobState, PendingReason};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EndReason {
    #[default]
    None,
    NonZeroExitCode,
    RaisedSignal,
    TimeLimit,
    DeadLine,
    OutOfMemory,
    Preempted,
    Cancelled,
    Requeued,
    NodeDown,
    JobLaunchFailure,
    CompletingTimeout,
}

impl EndReason {
    /// Stored in accounting and grouped on by reports, so a rename breaks them.
    /// Spellings match `PendingReason::display` where both name the same cause.
    pub fn as_str(self) -> &'static str {
        match self {
            EndReason::None => "None",
            EndReason::NonZeroExitCode => "NonZeroExitCode",
            EndReason::RaisedSignal => "RaisedSignal",
            EndReason::TimeLimit => "TimeLimit",
            EndReason::DeadLine => "DeadLine",
            EndReason::OutOfMemory => "OutOfMemory",
            EndReason::Preempted => "Preempted",
            EndReason::Cancelled => "Cancelled",
            EndReason::Requeued => "Requeued",
            EndReason::NodeDown => "NodeDown",
            EndReason::JobLaunchFailure => "JobLaunchFailure",
            EndReason::CompletingTimeout => "CompletingTimeout",
        }
    }
}

impl Job {
    /// Why this run ended. `final_state` is the state accounting records,
    /// i.e. after `end_of_run_state`.
    pub fn end_reason(&self, final_state: JobState) -> (EndReason, Option<String>) {
        match final_state {
            JobState::Completed => (EndReason::None, None),
            JobState::Failed => self.failure_reason(),
            JobState::Timeout => (EndReason::TimeLimit, None),
            JobState::Deadline => (EndReason::DeadLine, None),
            JobState::OutOfMemory => (EndReason::OutOfMemory, None),
            JobState::Preempted => (
                EndReason::Preempted,
                Some(preemption_summary(
                    self.preempted_by,
                    self.preempt_qos.as_deref(),
                )),
            ),
            JobState::Cancelled => (EndReason::Cancelled, None),
            JobState::Requeued => (EndReason::Requeued, None),
            JobState::NodeFail if self.pending_reason == PendingReason::JobLaunchFailure => (
                EndReason::JobLaunchFailure,
                self.launch_failure_detail.clone(),
            ),
            JobState::NodeFail => (EndReason::NodeDown, None),
            JobState::Pending | JobState::Running | JobState::Completing | JobState::Suspended => {
                (EndReason::None, None)
            }
        }
    }

    fn failure_reason(&self) -> (EndReason, Option<String>) {
        if !self.unreported_nodes.is_empty() {
            let nodes = self.unreported_nodes.join(",");
            return (
                EndReason::CompletingTimeout,
                Some(format!("no completion reported by {nodes}")),
            );
        }
        if self.exit_signal != 0 {
            let signal = self.exit_signal;
            return (
                EndReason::RaisedSignal,
                Some(format!("signal {signal} ({})", signal_name(signal))),
            );
        }
        // Shell convention only: spurd also exits 127 when it cannot exec the
        // shell, and a script may exit 127 itself, so this is a hint, not a code.
        let hint = match self.exit_code {
            Some(126) => Some("exit 126: command not executable (shell convention)"),
            Some(127) => Some("exit 127: command not found (shell convention)"),
            _ => None,
        };
        (EndReason::NonZeroExitCode, hint.map(str::to_owned))
    }
}

/// Human-readable name for a terminating signal, spelled as Slurm prints it in
/// `RaisedSignal:N(name)`.
pub fn signal_name(sig: i32) -> String {
    let name = match sig {
        1 => "Hangup",
        2 => "Interrupt",
        6 => "Aborted",
        9 => "Killed",
        11 => "Segmentation_fault",
        13 => "Broken_pipe",
        15 => "Terminated",
        _ => return format!("Signal {}", sig),
    };
    name.to_string()
}

/// Who preempted a job, as shown in its pending reason and its accounting detail.
pub fn preemption_summary(preempted_by: Option<JobId>, preempt_qos: Option<&str>) -> String {
    match (preempted_by, preempt_qos) {
        (Some(by), Some(qos)) => format!("preempted by job {by} (QOS: {qos})"),
        (Some(by), None) => format!("preempted by job {by}"),
        (None, _) => "preempted".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::JobSpec;

    fn job() -> Job {
        Job::new(1, JobSpec::default())
    }

    fn failed(exit_code: i32, exit_signal: i32) -> Job {
        let mut job = job();
        job.exit_code = Some(exit_code);
        job.exit_signal = exit_signal;
        job
    }

    #[test]
    fn every_reason_keeps_its_stored_spelling() {
        let pinned = [
            (EndReason::None, "None"),
            (EndReason::NonZeroExitCode, "NonZeroExitCode"),
            (EndReason::RaisedSignal, "RaisedSignal"),
            (EndReason::TimeLimit, "TimeLimit"),
            (EndReason::DeadLine, "DeadLine"),
            (EndReason::OutOfMemory, "OutOfMemory"),
            (EndReason::Preempted, "Preempted"),
            (EndReason::Cancelled, "Cancelled"),
            (EndReason::Requeued, "Requeued"),
            (EndReason::NodeDown, "NodeDown"),
            (EndReason::JobLaunchFailure, "JobLaunchFailure"),
            (EndReason::CompletingTimeout, "CompletingTimeout"),
        ];
        for (reason, spelling) in pinned {
            assert_eq!(reason.as_str(), spelling);
        }
    }

    #[test]
    fn shared_causes_are_spelled_like_the_pending_reason() {
        assert_eq!(
            EndReason::TimeLimit.as_str(),
            PendingReason::TimeLimit.display()
        );
        assert_eq!(
            EndReason::DeadLine.as_str(),
            PendingReason::DeadLine.display()
        );
        assert_eq!(
            EndReason::OutOfMemory.as_str(),
            PendingReason::OutOfMemory.display()
        );
        assert_eq!(
            EndReason::JobLaunchFailure.as_str(),
            PendingReason::JobLaunchFailure.display()
        );
    }

    #[test]
    fn a_clean_run_has_no_reason() {
        assert_eq!(
            job().end_reason(JobState::Completed),
            (EndReason::None, None)
        );
    }

    #[test]
    fn a_non_zero_exit_is_non_zero_exit_code_without_detail() {
        assert_eq!(
            failed(1, 0).end_reason(JobState::Failed),
            (EndReason::NonZeroExitCode, None)
        );
    }

    #[test]
    fn exit_126_and_127_carry_a_shell_hint_but_keep_the_code() {
        let (code, detail) = failed(127, 0).end_reason(JobState::Failed);
        assert_eq!(code, EndReason::NonZeroExitCode);
        assert!(detail.unwrap().contains("command not found"));
        let (code, detail) = failed(126, 0).end_reason(JobState::Failed);
        assert_eq!(code, EndReason::NonZeroExitCode);
        assert!(detail.unwrap().contains("not executable"));
    }

    #[test]
    fn a_signalled_failure_is_raised_signal_with_the_signal_named() {
        assert_eq!(
            failed(0, 11).end_reason(JobState::Failed),
            (
                EndReason::RaisedSignal,
                Some("signal 11 (Segmentation_fault)".into())
            )
        );
    }

    #[test]
    fn signal_name_lookup() {
        assert_eq!(signal_name(9), "Killed");
        assert_eq!(signal_name(15), "Terminated");
        // Multi-word names use underscores to match Slurm (e.g. 25.11.6).
        assert_eq!(signal_name(11), "Segmentation_fault");
        assert_eq!(signal_name(13), "Broken_pipe");
        assert_eq!(signal_name(99), "Signal 99");
    }

    #[test]
    fn a_preemption_names_the_preemptor_and_its_qos() {
        let mut job = job();
        job.preempted_by = Some(42);
        job.preempt_qos = Some("high".into());
        assert_eq!(
            job.end_reason(JobState::Preempted),
            (
                EndReason::Preempted,
                Some("preempted by job 42 (QOS: high)".into())
            )
        );
        assert_eq!(preemption_summary(Some(42), None), "preempted by job 42");
        assert_eq!(preemption_summary(None, None), "preempted");
    }

    #[test]
    fn a_force_finish_names_the_nodes_that_never_reported() {
        let mut job = failed(1, 0);
        job.unreported_nodes = vec!["node2".into(), "node3".into()];
        let (code, detail) = job.end_reason(JobState::Failed);
        assert_eq!(code, EndReason::CompletingTimeout);
        assert_eq!(
            detail.as_deref(),
            Some("no completion reported by node2,node3")
        );
    }

    #[test]
    fn the_accounting_state_decides_before_the_exit_detail() {
        let killed = failed(0, 9);
        assert_eq!(killed.end_reason(JobState::Timeout).0, EndReason::TimeLimit);
        assert_eq!(
            killed.end_reason(JobState::OutOfMemory).0,
            EndReason::OutOfMemory
        );
        assert_eq!(
            killed.end_reason(JobState::Preempted).0,
            EndReason::Preempted
        );
        assert_eq!(
            killed.end_reason(JobState::Cancelled).0,
            EndReason::Cancelled
        );
    }

    #[test]
    fn deadline_and_requeue_have_their_own_codes() {
        assert_eq!(job().end_reason(JobState::Deadline).0, EndReason::DeadLine);
        assert_eq!(job().end_reason(JobState::Requeued).0, EndReason::Requeued);
    }

    #[test]
    fn a_launch_eviction_carries_the_launch_detail() {
        let mut job = job();
        job.set_pending_reason(PendingReason::JobLaunchFailure);
        job.launch_failure_detail = Some("pmix bootstrap timed out".into());
        assert_eq!(
            job.end_reason(JobState::NodeFail),
            (
                EndReason::JobLaunchFailure,
                Some("pmix bootstrap timed out".into())
            )
        );
    }

    #[test]
    fn any_other_node_fail_is_node_down() {
        let mut job = job();
        job.set_pending_reason(PendingReason::NodeDown);
        assert_eq!(
            job.end_reason(JobState::NodeFail),
            (EndReason::NodeDown, None)
        );
    }

    #[test]
    fn a_stale_scheduling_reason_does_not_leak_into_a_failure() {
        let mut job = failed(2, 0);
        job.set_pending_reason(PendingReason::Priority);
        assert_eq!(
            job.end_reason(JobState::Failed).0,
            EndReason::NonZeroExitCode
        );
    }

    #[test]
    fn a_job_that_has_not_ended_has_no_reason() {
        for state in [
            JobState::Pending,
            JobState::Running,
            JobState::Completing,
            JobState::Suspended,
        ] {
            assert_eq!(job().end_reason(state), (EndReason::None, None));
        }
    }
}
