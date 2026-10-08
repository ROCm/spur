// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{bail, Context, Result};
use clap::Parser;
use spur_proto::proto::CancelJobRequest;

/// Cancel pending or running jobs.
#[derive(Parser, Debug)]
#[command(name = "scancel", about = "Cancel jobs")]
pub struct ScancelArgs {
    /// Job IDs to cancel
    pub job_ids: Vec<u32>,

    /// Cancel all jobs for this user
    #[arg(short = 'u', long)]
    pub user: Option<String>,

    /// Cancel jobs in this partition
    #[arg(short = 'p', long)]
    pub partition: Option<String>,

    /// Cancel jobs in this state
    #[arg(short = 't', long)]
    pub state: Option<String>,

    /// Cancel jobs with this name
    #[arg(short = 'n', long)]
    pub name: Option<String>,

    /// Cancel jobs for this account
    #[arg(short = 'A', long)]
    pub account: Option<String>,

    /// Signal to send (default: SIGKILL / cancel)
    #[arg(short = 's', long)]
    pub signal: Option<String>,

    /// Batch mode: cancel the batch job step
    #[arg(short = 'b', long)]
    pub batch: bool,

    /// Quiet mode
    #[arg(short = 'Q', long)]
    pub quiet: bool,

    /// Interactive: confirm each cancellation
    #[arg(short = 'i', long)]
    pub interactive: bool,

    /// Controller address
    #[arg(
        long,
        env = "SPUR_CONTROLLER_ADDR",
        default_value = "http://localhost:6817"
    )]
    pub controller: String,
}

pub async fn main() -> Result<()> {
    main_with_args(std::env::args().collect()).await
}

pub async fn main_with_args(args: Vec<String>) -> Result<()> {
    let args = crate::clap_exit::parse_or_exit::<ScancelArgs>(&args);

    if !has_selection(&args) {
        bail!("scancel: no job IDs or filters specified");
    }

    let signal = parse_signal(args.signal.as_deref())?;

    // Requester identity for authorization on each cancel. The server checks
    // this against the job owner (root/empty bypass), so it must be the caller.
    let requester = match &args.user {
        Some(user) => user.clone(),
        None => crate::interactive::current_user()?,
    };
    let filter_user = filter_user(&args, &requester);

    let channel = crate::authclient::connect(&args.controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = spur_proto::controller_client(channel);
    let mut failed = false;

    if !args.job_ids.is_empty() {
        // Cancel specific jobs
        for job_id in &args.job_ids {
            match client
                .cancel_job(CancelJobRequest {
                    job_id: *job_id,
                    signal,
                    user: requester.clone(),
                })
                .await
            {
                Ok(_) => {}
                Err(e) => failed |= report_refusal(*job_id, &e, signal, args.quiet),
            }
        }
    } else {
        // Filter-based cancellation: get matching jobs, then cancel each
        let states = filter_states(args.state.as_deref())?;

        let response = client
            .get_jobs(spur_proto::proto::GetJobsRequest {
                states,
                user: filter_user,
                partition: args.partition.clone().unwrap_or_default(),
                account: args.account.clone().unwrap_or_default(),
                job_ids: Vec::new(),
                name: args.name.clone().unwrap_or_default(),
                nodes: Vec::new(),
                qos: String::new(),
                reservation: String::new(),
            })
            .await
            .context("failed to get jobs")?;

        let jobs = response.into_inner().jobs;

        // Filter-based selection targets only cancellable jobs. Terminal jobs
        // matched by the filter (e.g. a user's already-finished jobs under
        // `scancel -u`) are skipped rather than sent to cancel_job, which would
        // reject each one and emit a spurious per-job error. Matches Slurm.
        for job in &jobs {
            if !is_cancellable(job.state) {
                continue;
            }
            match client
                .cancel_job(CancelJobRequest {
                    job_id: job.job_id,
                    signal,
                    user: requester.clone(),
                })
                .await
            {
                Ok(_) => {}
                Err(e) => failed |= report_refusal(job.job_id, &e, signal, args.quiet),
            }
        }
    }

    if failed {
        std::process::exit(1);
    }
    Ok(())
}

/// Print a refused cancel unless it is ignored, and return whether it fails the command.
fn report_refusal(job_id: u32, status: &tonic::Status, signal: i32, quiet: bool) -> bool {
    if should_report(status.code(), signal, quiet) {
        eprintln!(
            "scancel: error cancelling job {}: {}",
            job_id,
            status.message()
        );
    }
    counts_as_failure(status.code(), signal)
}

/// Slurm's scancel stays silent on an ignored refusal, and `-Q` also hides a
/// job that is already gone when signalling it.
fn should_report(code: tonic::Code, signal: i32, quiet: bool) -> bool {
    counts_as_failure(code, signal) && !(quiet && already_gone(code))
}

/// Whether a refused cancel makes scancel exit non-zero. Like Slurm, a job that is
/// unknown or already finished does not fail a cancel (it is gone either way), but
/// does fail a signal, and anything else, such as another user's job, fails both.
fn counts_as_failure(code: tonic::Code, signal: i32) -> bool {
    let cancels = matches!(signal, 0 | 9);
    !(cancels && already_gone(code))
}

fn already_gone(code: tonic::Code) -> bool {
    matches!(
        code,
        tonic::Code::NotFound | tonic::Code::FailedPrecondition
    )
}

/// Whether a job in the given proto state can still be cancelled. Unknown
/// state values are treated as cancellable so the server remains the
/// authority on rejection rather than the client silently dropping them.
fn is_cancellable(proto_state: i32) -> bool {
    match spur_core::job::JobState::from_proto_i32(proto_state) {
        Some(state) => !state.is_terminal(),
        None => true,
    }
}

/// Whether the caller named anything to cancel. `--state` alone does not
/// count: it narrows a selection rather than making one.
fn has_selection(args: &ScancelArgs) -> bool {
    !args.job_ids.is_empty()
        || args.user.is_some()
        || args.name.is_some()
        || args.partition.is_some()
        || args.account.is_some()
}

/// The `user` filter for get_jobs. `-u` wins; otherwise scope to the caller only
/// when no other selector was named, so `scancel -A acct` spans the whole account
/// instead of silently narrowing to self. The server still authorizes each cancel.
fn filter_user(args: &ScancelArgs, requester: &str) -> String {
    match &args.user {
        Some(user) => user.clone(),
        None if args.partition.is_some() || args.account.is_some() || args.name.is_some() => {
            String::new()
        }
        None => requester.to_string(),
    }
}

fn filter_states(state: Option<&str>) -> Result<Vec<i32>> {
    let Some(states) = state else {
        return Ok(cancellable_states());
    };

    let states = states
        .split(',')
        .map(str::trim)
        .filter(|state| !state.is_empty())
        .map(|state| {
            parse_state(state)
                .map(|state| state as i32)
                .ok_or_else(|| anyhow::anyhow!("scancel: invalid job state: {state}"))
        })
        .collect::<Result<Vec<_>>>()?;

    // Comma-only or whitespace-only input leaves no tokens after normalization.
    if states.is_empty() {
        bail!("scancel: invalid job state: (empty)");
    }

    Ok(states)
}

fn cancellable_states() -> Vec<i32> {
    spur_core::job::JobState::ALL
        .iter()
        .filter(|state| !state.is_terminal())
        .map(|state| state.to_proto_i32())
        .collect()
}

fn parse_signal(s: Option<&str>) -> Result<i32> {
    match s {
        None => Ok(0), // 0 = cancel (not a signal)
        Some("KILL") | Some("SIGKILL") | Some("9") => Ok(9),
        Some("TERM") | Some("SIGTERM") | Some("15") => Ok(15),
        Some("INT") | Some("SIGINT") | Some("2") => Ok(2),
        Some("USR1") | Some("SIGUSR1") | Some("10") => Ok(10),
        Some("USR2") | Some("SIGUSR2") | Some("12") => Ok(12),
        Some(other) => {
            if let Ok(n) = other.parse::<i32>() {
                Ok(n)
            } else {
                bail!("scancel: invalid signal: {}", other)
            }
        }
    }
}

fn parse_state(s: &str) -> Option<spur_proto::proto::JobState> {
    match s.to_uppercase().as_str() {
        "PD" | "PENDING" => Some(spur_proto::proto::JobState::JobPending),
        "R" | "RUNNING" => Some(spur_proto::proto::JobState::JobRunning),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_proto::proto::JobState;

    fn parse(args: &[&str]) -> ScancelArgs {
        let mut argv = vec!["scancel"];
        argv.extend_from_slice(args);
        ScancelArgs::try_parse_from(argv).expect("args should parse")
    }

    #[test]
    fn every_selection_flag_counts_as_a_selection() {
        for args in [
            vec!["123"],
            vec!["-u", "alice"],
            vec!["-n", "myjob"],
            vec!["-p", "default"],
            vec!["-A", "acct"],
        ] {
            assert!(
                has_selection(&parse(&args)),
                "{args:?} should select jobs to cancel"
            );
        }
    }

    #[test]
    fn no_arguments_selects_nothing() {
        assert!(!has_selection(&parse(&[])));
    }

    #[test]
    fn state_alone_selects_nothing() {
        assert!(!has_selection(&parse(&["-t", "RUNNING"])));
    }

    #[test]
    fn explicit_user_is_always_the_filter() {
        assert_eq!(filter_user(&parse(&["-u", "alice"]), "bob"), "alice");
        assert_eq!(
            filter_user(&parse(&["-u", "alice", "-A", "gpu"]), "bob"),
            "alice"
        );
    }

    #[test]
    fn account_or_partition_without_user_spans_all_users() {
        assert_eq!(filter_user(&parse(&["-A", "gpu"]), "bob"), "");
        assert_eq!(filter_user(&parse(&["-p", "batch"]), "bob"), "");
        assert_eq!(filter_user(&parse(&["-n", "train"]), "bob"), "");
    }

    #[test]
    fn no_selector_defaults_to_the_caller() {
        // Reachable via the job-id path, where the filter user is unused but the
        // fallback must stay the caller rather than an empty (owner-bypass) value.
        assert_eq!(filter_user(&parse(&["123"]), "bob"), "bob");
    }

    #[test]
    fn active_states_are_cancellable() {
        for state in [
            JobState::JobPending,
            JobState::JobRunning,
            JobState::JobCompleting,
            JobState::JobPreempted,
            JobState::JobSuspended,
        ] {
            assert!(
                is_cancellable(state as i32),
                "{state:?} should be cancellable"
            );
        }
    }

    #[test]
    fn default_filter_requests_only_cancellable_states() {
        let states = filter_states(None).unwrap();

        assert!(states.contains(&(JobState::JobPending as i32)));
        assert!(states.contains(&(JobState::JobRunning as i32)));
        assert!(states.contains(&(JobState::JobCompleting as i32)));
        assert!(states.contains(&(JobState::JobPreempted as i32)));
        assert!(states.contains(&(JobState::JobSuspended as i32)));
        assert!(!states.contains(&(JobState::JobCompleted as i32)));
        assert!(!states.contains(&(JobState::JobFailed as i32)));
        assert!(!states.contains(&(JobState::JobCancelled as i32)));
        assert!(!states.contains(&(JobState::JobTimeout as i32)));
        assert!(!states.contains(&(JobState::JobNodeFail as i32)));
        assert!(!states.contains(&(JobState::JobDeadline as i32)));
        assert!(!states.contains(&(JobState::JobOutOfMemory as i32)));
    }

    #[test]
    fn explicit_filter_uses_requested_states() {
        assert_eq!(
            filter_states(Some("PD,R")).unwrap(),
            vec![JobState::JobPending as i32, JobState::JobRunning as i32,]
        );
    }

    #[test]
    fn invalid_explicit_filter_is_rejected() {
        let error = filter_states(Some("PD,BANANA")).unwrap_err();

        assert_eq!(error.to_string(), "scancel: invalid job state: BANANA");
    }

    #[test]
    fn empty_explicit_filter_is_rejected() {
        let error = filter_states(Some(" , ")).unwrap_err();

        assert_eq!(error.to_string(), "scancel: invalid job state: (empty)");
    }

    #[test]
    fn terminal_states_are_not_cancellable() {
        for state in [
            JobState::JobCompleted,
            JobState::JobFailed,
            JobState::JobCancelled,
            JobState::JobTimeout,
            JobState::JobNodeFail,
            JobState::JobDeadline,
            JobState::JobOutOfMemory,
        ] {
            assert!(
                !is_cancellable(state as i32),
                "{state:?} should not be cancellable"
            );
        }
    }

    #[test]
    fn a_refused_cancel_of_another_users_job_fails_the_command() {
        assert!(counts_as_failure(tonic::Code::PermissionDenied, 0));
        assert!(counts_as_failure(tonic::Code::PermissionDenied, 15));
    }

    #[test]
    fn a_job_already_gone_fails_a_signal_but_not_a_cancel() {
        for code in [tonic::Code::NotFound, tonic::Code::FailedPrecondition] {
            assert!(!counts_as_failure(code, 0), "{code:?} with a cancel");
            assert!(!counts_as_failure(code, 9), "{code:?} with SIGKILL");
            assert!(counts_as_failure(code, 15), "{code:?} with SIGTERM");
        }
    }

    #[test]
    fn an_ignored_refusal_is_not_reported() {
        for code in [tonic::Code::NotFound, tonic::Code::FailedPrecondition] {
            assert!(!should_report(code, 0, false), "{code:?} with a cancel");
            assert!(!should_report(code, 9, false), "{code:?} with SIGKILL");
        }
    }

    #[test]
    fn quiet_hides_a_gone_job_on_a_signal_but_not_a_denial() {
        assert!(should_report(tonic::Code::NotFound, 15, false));
        assert!(!should_report(tonic::Code::NotFound, 15, true));
        assert!(should_report(tonic::Code::PermissionDenied, 0, true));
    }

    #[test]
    fn unknown_state_is_cancellable() {
        // Server stays the authority on rejection for values the client
        // does not recognize.
        assert!(is_cancellable(9999));
    }
}
