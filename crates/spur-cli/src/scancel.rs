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
    let args = ScancelArgs::try_parse_from(&args)?;

    if args.job_ids.is_empty() && args.user.is_none() && args.name.is_none() {
        bail!("scancel: no job IDs or filters specified");
    }

    let signal = parse_signal(args.signal.as_deref())?;

    let user = args
        .user
        .unwrap_or_else(|| whoami::username().unwrap_or_else(|_| "unknown".into()));

    let channel = spur_client::connect_channel(&args.controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = spur_proto::controller_client(channel);

    if !args.job_ids.is_empty() {
        // Cancel specific jobs
        for job_id in &args.job_ids {
            match client
                .cancel_job(CancelJobRequest {
                    job_id: *job_id,
                    signal,
                    user: user.clone(),
                })
                .await
            {
                Ok(_) => {
                    if !args.quiet {
                        // scancel is silent on success by default (like Slurm)
                    }
                }
                Err(e) => {
                    eprintln!("scancel: error cancelling job {}: {}", job_id, e.message());
                }
            }
        }
    } else {
        // Filter-based cancellation: get matching jobs, then cancel each
        let states = args
            .state
            .as_ref()
            .map(|s| {
                s.split(',')
                    .filter_map(|st| parse_state(st.trim()))
                    .map(|s| s as i32)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let response = client
            .get_jobs(spur_proto::proto::GetJobsRequest {
                states,
                user: user.clone(),
                partition: args.partition.unwrap_or_default(),
                account: args.account.unwrap_or_default(),
                job_ids: Vec::new(),
                name: args.name.unwrap_or_default(),
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
                    user: user.clone(),
                })
                .await
            {
                Ok(_) => {}
                Err(e) => {
                    eprintln!(
                        "scancel: error cancelling job {}: {}",
                        job.job_id,
                        e.message()
                    );
                }
            }
        }
    }

    Ok(())
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

    #[test]
    fn active_states_are_cancellable() {
        for state in [
            JobState::JobPending,
            JobState::JobRunning,
            JobState::JobCompleting,
            JobState::JobSuspended,
        ] {
            assert!(
                is_cancellable(state as i32),
                "{state:?} should be cancellable"
            );
        }
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
    fn unknown_state_is_cancellable() {
        // Server stays the authority on rejection for values the client
        // does not recognize.
        assert!(is_cancellable(9999));
    }
}
