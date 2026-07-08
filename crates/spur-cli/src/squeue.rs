// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use clap::Parser;
use spur_proto::proto::slurm_controller_client::SlurmControllerClient;
use spur_proto::proto::GetJobsRequest;

use crate::format_engine;

/// View information about jobs in the scheduling queue.
#[derive(Parser, Debug)]
#[command(name = "squeue", about = "View the job queue")]
pub struct SqueueArgs {
    /// Show only jobs for this user
    #[arg(short = 'u', long)]
    pub user: Option<String>,

    /// Show only jobs in this partition
    #[arg(short = 'p', long)]
    pub partition: Option<String>,

    /// Show only jobs in these states (comma-separated)
    #[arg(short = 't', long)]
    pub states: Option<String>,

    /// Show only these job IDs (comma-separated)
    #[arg(short = 'j', long)]
    pub jobs: Option<String>,

    /// Show only this account
    #[arg(short = 'A', long)]
    pub account: Option<String>,

    /// Output format string
    #[arg(short = 'o', long)]
    pub format: Option<String>,

    /// Long format (more columns)
    #[arg(short = 'l', long)]
    pub long: bool,

    /// Don't print header
    #[arg(short = 'h', long)]
    pub noheader: bool,

    /// Sort by field
    #[arg(short = 'S', long)]
    pub sort: Option<String>,

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
    let args = SqueueArgs::try_parse_from(&args)?;

    // Determine format
    let fmt = if let Some(ref f) = args.format {
        f.clone()
    } else if args.long {
        "%.18i %.9P %.8j %.8u %.8T %.10M %.9l %.6D %R".to_string()
    } else {
        format_engine::SQUEUE_DEFAULT_FORMAT.to_string()
    };

    let fields = format_engine::parse_format(&fmt, &format_engine::squeue_header);

    // Parse state filter — default to Pending+Running+Completing when no filter specified (Slurm default)
    let states = match args.states.as_deref() {
        Some(s) => parse_states_arg(s)?,
        None => default_squeue_states(),
    };

    // Parse job ID filter
    let job_ids = args
        .jobs
        .as_ref()
        .map(|s| {
            s.split(',')
                .filter_map(|j| j.trim().parse::<u32>().ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    // Connect and fetch
    let channel = spur_client::connect_channel(&args.controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = SlurmControllerClient::new(channel);

    let response = client
        .get_jobs(GetJobsRequest {
            states: states.iter().map(|s| *s as i32).collect(),
            user: args.user.unwrap_or_default(),
            partition: args.partition.unwrap_or_default(),
            account: args.account.unwrap_or_default(),
            job_ids,
        })
        .await
        .context("failed to get jobs")?;

    let jobs = response.into_inner().jobs;

    // Print header
    if !args.noheader {
        println!("{}", format_engine::format_header(&fields));
    }

    // Print rows
    for job in &jobs {
        let row = format_engine::format_row(&fields, &|spec| resolve_job_field(job, spec));
        println!("{}", row);
    }

    Ok(())
}

fn resolve_job_field(job: &spur_proto::proto::JobInfo, spec: char) -> String {
    match spec {
        'i' => job.job_id.to_string(),
        'j' | 'n' => job.name.clone(),
        'u' => job.user.clone(),
        'P' => job.partition.clone(),
        't' => state_code(job.state),
        'T' => state_name(job.state),
        'M' => format_runtime(job),
        'l' => format_time_limit(job),
        'D' => job.num_nodes.to_string(),
        'R' => {
            if job.state == spur_proto::proto::JobState::JobPending as i32 {
                format!("({})", job.state_reason)
            } else {
                job.nodelist.clone()
            }
        }
        'C' => job.cpus_per_task.to_string(),
        'N' => job.nodelist.clone(),
        'a' => job.account.clone(),
        'p' => job.priority.to_string(),
        'q' => job.qos.clone(),
        'r' => crate::exit_fmt::render_reason(&job.state_reason, job.exit_signal),
        'Z' => job.work_dir.clone(),
        'o' => job.command.clone(),
        'S' => format_timestamp(job.start_time.as_ref()),
        'V' => format_timestamp(job.submit_time.as_ref()),
        'v' => job.reservation.clone(),
        'e' => format_timestamp(job.end_time.as_ref()),
        _ => "?".into(),
    }
}

fn state_code(state: i32) -> String {
    spur_core::job::JobState::from_proto_i32(state)
        .map(|s| s.code().to_string())
        .unwrap_or_else(|| "?".into())
}

fn state_name(state: i32) -> String {
    spur_core::job::JobState::from_proto_i32(state)
        .map(|s| s.display().to_string())
        .unwrap_or_else(|| "UNKNOWN".into())
}

/// Default `squeue` states when `-t` is omitted: PD, R, S, CG (Slurm parity —
/// suspended jobs remain visible in the default view).
fn default_squeue_states() -> Vec<spur_proto::proto::JobState> {
    vec![
        spur_proto::proto::JobState::JobPending,
        spur_proto::proto::JobState::JobRunning,
        spur_proto::proto::JobState::JobSuspended,
        spur_proto::proto::JobState::JobCompleting,
    ]
}

/// Parse `-t` / `--states` (comma-separated). Whole-string `all` means no state filter.
/// Unknown tokens are rejected (Slurm exits with an error rather than showing all jobs).
fn parse_states_arg(s: &str) -> Result<Vec<spur_proto::proto::JobState>> {
    use spur_core::job::JobState;

    let trimmed = s.trim();
    if trimmed.eq_ignore_ascii_case("all") {
        return Ok(Vec::new());
    }

    let tokens: Vec<&str> = trimmed
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();

    if tokens.is_empty() {
        anyhow::bail!("Invalid job state specified: (empty)");
    }

    let mut states = Vec::with_capacity(tokens.len());
    for token in tokens {
        let core = JobState::from_code_or_name(token)
            .ok_or_else(|| anyhow::anyhow!("Invalid job state specified: {token}"))?;
        states.push(core.to_proto());
    }
    Ok(states)
}

fn format_runtime(job: &spur_proto::proto::JobInfo) -> String {
    if let Some(ref rt) = job.run_time {
        format_duration_hms(rt.seconds)
    } else {
        "0:00".into()
    }
}

fn format_time_limit(job: &spur_proto::proto::JobInfo) -> String {
    if let Some(ref tl) = job.time_limit {
        format_duration_hms(tl.seconds)
    } else {
        "UNLIMITED".into()
    }
}

fn format_duration_hms(total_seconds: i64) -> String {
    let total_seconds = total_seconds.unsigned_abs();
    let days = total_seconds / 86400;
    let hours = (total_seconds % 86400) / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;

    if days > 0 {
        format!("{}-{:02}:{:02}:{:02}", days, hours, minutes, seconds)
    } else if hours > 0 {
        format!("{}:{:02}:{:02}", hours, minutes, seconds)
    } else {
        format!("{}:{:02}", minutes, seconds)
    }
}

fn format_timestamp(ts: Option<&prost_types::Timestamp>) -> String {
    match ts {
        Some(t) if t.seconds > 0 => {
            let dt =
                chrono::DateTime::from_timestamp(t.seconds, t.nanos as u32).unwrap_or_default();
            dt.format("%Y-%m-%dT%H:%M:%S").to_string()
        }
        _ => "N/A".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_proto::proto::JobState as P;

    #[test]
    fn default_squeue_states_includes_completing() {
        let states = default_squeue_states();
        assert_eq!(states.len(), 4);
        assert!(states.contains(&P::JobPending));
        assert!(states.contains(&P::JobRunning));
        assert!(states.contains(&P::JobSuspended));
        assert!(states.contains(&P::JobCompleting));
    }

    #[test]
    fn parse_states_arg_accepts_codes_and_names() {
        let states = parse_states_arg("R,PD").unwrap();
        assert_eq!(states.len(), 2);
        assert_eq!(states[0], P::JobRunning);
        assert_eq!(states[1], P::JobPending);
    }

    #[test]
    fn parse_states_arg_all_means_no_filter() {
        assert!(parse_states_arg("all").unwrap().is_empty());
        assert!(parse_states_arg("ALL").unwrap().is_empty());
    }

    #[test]
    fn parse_states_arg_rejects_unknown() {
        let err = parse_states_arg("BOGUS").unwrap_err();
        assert!(err.to_string().contains("BOGUS"));

        let err = parse_states_arg("R,BOGUS").unwrap_err();
        assert!(err.to_string().contains("BOGUS"));
    }

    #[test]
    fn parse_states_arg_rejects_empty_list() {
        assert!(parse_states_arg("").is_err());
        assert!(parse_states_arg("  ,  ").is_err());
    }
}
