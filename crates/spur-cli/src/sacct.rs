// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use clap::Parser;
use spur_proto::proto::slurm_accounting_client::SlurmAccountingClient;
use spur_proto::proto::GetJobHistoryRequest;

use crate::exit_fmt::format_exit;
use crate::format_engine;

/// Display accounting data for jobs.
#[derive(Parser, Debug)]
#[command(name = "sacct", about = "Display accounting data for jobs")]
pub struct SacctArgs {
    /// Show jobs for this user
    #[arg(short = 'u', long)]
    pub user: Option<String>,

    /// Show jobs for this account
    #[arg(short = 'A', long)]
    pub account: Option<String>,

    /// Start time filter (e.g., "2024-01-01", "now-7days")
    #[arg(short = 'S', long)]
    pub starttime: Option<String>,

    /// End time filter
    #[arg(short = 'E', long)]
    pub endtime: Option<String>,

    /// Show only these states (comma-separated)
    #[arg(short = 's', long)]
    pub state: Option<String>,

    /// Show only these job IDs
    #[arg(short = 'j', long)]
    pub jobs: Option<String>,

    /// Output format
    #[arg(short = 'o', long)]
    pub format: Option<String>,

    /// Long format
    #[arg(short = 'l', long)]
    pub long: bool,

    /// Brief format
    #[arg(short = 'b', long)]
    pub brief: bool,

    /// Don't print header
    #[arg(short = 'n', long)]
    pub noheader: bool,

    /// Max records
    #[arg(long, default_value = "100")]
    pub limit: u32,

    /// Controller address (accounting is served on the same port)
    #[arg(
        long,
        env = "SPUR_CONTROLLER_ADDR",
        default_value = "http://localhost:6817"
    )]
    pub controller: String,
}

const SACCT_DEFAULT_FORMAT: &str = "%.8i %.15j %.10u %.10a %.10P %.8T %10M %.8D %6x";
const SACCT_LONG_FORMAT: &str =
    "%.8i %.15j %.10u %.10a %.10P %.8T %10M %.8D %6x %.10X %.19S %.19E %.10l";
const SACCT_BRIEF_FORMAT: &str = "%.8i %.8T %6x";

pub fn sacct_header(spec: char) -> &'static str {
    match spec {
        'i' => "JobID",
        'j' => "JobName",
        'u' => "User",
        'a' => "Account",
        'P' => "Partition",
        'T' => "State",
        't' => "State",
        'M' => "Elapsed",
        'D' => "NNodes",
        'x' => "ExitCode",
        'X' => "DerivedExitCode",
        'S' => "Start",
        'E' => "End",
        'V' => "Submit",
        'l' => "TimeLimit",
        'n' => "NodeList",
        'C' => "NCPUS",
        'R' => "ReqMem",
        'Q' => "QOS",
        _ => "?",
    }
}

/// Maps a sacct `--format` field name to its internal spec char.
fn sacct_field_spec(name: &str) -> Option<char> {
    match name.to_lowercase().as_str() {
        "jobid" => Some('i'),
        "jobname" => Some('j'),
        "user" => Some('u'),
        "account" => Some('a'),
        "partition" => Some('P'),
        "state" => Some('T'),
        "elapsed" => Some('M'),
        "nnodes" => Some('D'),
        "exitcode" => Some('x'),
        "derivedexitcode" => Some('X'),
        "start" => Some('S'),
        "end" => Some('E'),
        "submit" => Some('V'),
        "timelimit" => Some('l'),
        "nodelist" => Some('n'),
        "ncpus" => Some('C'),
        "reqmem" => Some('R'),
        "qos" => Some('Q'),
        _ => None,
    }
}

pub async fn main() -> Result<()> {
    main_with_args(std::env::args().collect()).await
}

pub async fn main_with_args(args: Vec<String>) -> Result<()> {
    let args = SacctArgs::try_parse_from(&args)?;

    // -o/--format uses Slurm's comma-separated field-name syntax, not %-specifiers.
    let fields = if let Some(ref f) = args.format {
        let fields = format_engine::parse_named_format(f, &sacct_field_spec, &sacct_header);
        if fields.is_empty() {
            anyhow::bail!("sacct: no recognized fields in --format='{f}'");
        }
        fields
    } else if args.long {
        format_engine::parse_format(SACCT_LONG_FORMAT, &sacct_header)
    } else if args.brief {
        format_engine::parse_format(SACCT_BRIEF_FORMAT, &sacct_header)
    } else {
        format_engine::parse_format(SACCT_DEFAULT_FORMAT, &sacct_header)
    };

    // Parse state filter
    let states: Vec<i32> = args
        .state
        .as_ref()
        .map(|s| {
            s.split(',')
                .filter_map(|st| parse_acct_state(st.trim()))
                .collect()
        })
        .unwrap_or_default();

    let mut client = SlurmAccountingClient::connect(args.controller)
        .await
        .context("failed to connect to controller")?;

    let start_after = args
        .starttime
        .as_deref()
        .and_then(parse_time_arg)
        .map(datetime_to_proto);
    let start_before = args
        .endtime
        .as_deref()
        .and_then(parse_time_arg)
        .map(datetime_to_proto);

    let response = client
        .get_job_history(GetJobHistoryRequest {
            user: args.user.unwrap_or_default(),
            account: args.account.unwrap_or_default(),
            start_after,
            start_before,
            states,
            limit: args.limit,
        })
        .await
        .context("failed to get job history")?;

    let jobs = response.into_inner().jobs;

    if !args.noheader {
        println!("{}", format_engine::format_header(&fields));
        // Print separator line
        let sep = format_engine::format_header(&fields)
            .chars()
            .map(|c| if c == ' ' { ' ' } else { '-' })
            .collect::<String>();
        println!("{}", sep);
    }

    for job in &jobs {
        let row = format_engine::format_row(&fields, &|spec| resolve_sacct_field(job, spec));
        println!("{}", row);
    }

    Ok(())
}

fn resolve_sacct_field(job: &spur_proto::proto::JobInfo, spec: char) -> String {
    match spec {
        'i' => job.job_id.to_string(),
        'j' => job.name.clone(),
        'u' => job.user.clone(),
        'a' => job.account.clone(),
        'P' => job.partition.clone(),
        'T' | 't' => state_name(job.state),
        'M' => format_elapsed(job),
        'D' => job.num_nodes.to_string(),
        'x' => format_exit(job.exit_code, job.exit_signal),
        'X' => format_exit(job.derived_exit_code, 0),
        'l' => match job.time_limit.as_ref() {
            Some(d) if d.seconds > 0 => format_duration(d.seconds),
            _ => "UNLIMITED".into(),
        },
        'S' => format_timestamp(job.start_time.as_ref()),
        'E' => format_timestamp(job.end_time.as_ref()),
        'V' => format_timestamp(job.submit_time.as_ref()),
        'n' => job.nodelist.clone(),
        'C' => (job.num_tasks * job.cpus_per_task.max(1)).to_string(),
        'Q' => job.qos.clone(),
        _ => "?".into(),
    }
}

fn state_name(state: i32) -> String {
    spur_core::job::JobState::from_proto_i32(state)
        .map(|s| s.display().to_string())
        .unwrap_or_else(|| "UNKNOWN".into())
}

fn parse_acct_state(s: &str) -> Option<i32> {
    match s.to_uppercase().as_str() {
        "CD" | "COMPLETED" => Some(3),
        "F" | "FAILED" => Some(4),
        "CA" | "CANCELLED" => Some(5),
        "TO" | "TIMEOUT" => Some(6),
        "NF" | "NODE_FAIL" => Some(7),
        "DL" | "DEADLINE" => Some(10),
        "R" | "RUNNING" => Some(1),
        "PD" | "PENDING" => Some(0),
        _ => None,
    }
}

fn format_elapsed(job: &spur_proto::proto::JobInfo) -> String {
    if let Some(ref rt) = job.run_time {
        format_duration(rt.seconds)
    } else {
        "00:00:00".into()
    }
}

fn format_duration(total_seconds: i64) -> String {
    let total_seconds = total_seconds.unsigned_abs();
    let days = total_seconds / 86400;
    let hours = (total_seconds % 86400) / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;

    if days > 0 {
        format!("{}-{:02}:{:02}:{:02}", days, hours, minutes, seconds)
    } else {
        format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
    }
}

fn format_timestamp(ts: Option<&prost_types::Timestamp>) -> String {
    match ts {
        Some(t) if t.seconds > 0 => {
            let dt =
                chrono::DateTime::from_timestamp(t.seconds, t.nanos as u32).unwrap_or_default();
            dt.format("%Y-%m-%dT%H:%M:%S").to_string()
        }
        _ => "Unknown".into(),
    }
}

/// Parse a time argument string into a DateTime.
/// Supports: "2024-01-01", "2024-01-01T00:00:00", "now-7days", "now-24hours".
fn parse_time_arg(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    use chrono::{NaiveDate, NaiveDateTime, Utc};
    let s = s.trim();

    // Relative: "now-Ndays", "now-Nhours"
    if let Some(rest) = s.strip_prefix("now-") {
        if let Some(days) = rest
            .strip_suffix("days")
            .or_else(|| rest.strip_suffix("day"))
        {
            let n: i64 = days.trim().parse().ok()?;
            return Some(Utc::now() - chrono::Duration::days(n));
        }
        if let Some(hours) = rest
            .strip_suffix("hours")
            .or_else(|| rest.strip_suffix("hour"))
        {
            let n: i64 = hours.trim().parse().ok()?;
            return Some(Utc::now() - chrono::Duration::hours(n));
        }
    }

    // ISO datetime: "2024-01-01T00:00:00"
    if let Ok(ndt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Some(ndt.and_utc());
    }

    // Date only: "2024-01-01"
    if let Ok(nd) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return nd.and_hms_opt(0, 0, 0).map(|ndt| ndt.and_utc());
    }

    None
}

fn datetime_to_proto(dt: chrono::DateTime<chrono::Utc>) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: dt.timestamp(),
        nanos: dt.timestamp_subsec_nanos() as i32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_proto::proto::{JobInfo, JobState};

    fn job(exit_code: i32, exit_signal: i32, derived_exit_code: i32) -> JobInfo {
        JobInfo {
            exit_code,
            exit_signal,
            derived_exit_code,
            ..Default::default()
        }
    }

    #[test]
    fn exit_and_derived_fields_render() {
        // %x is Slurm's code:signal (signal half must survive from accounting);
        // %X is DerivedExitCode (running max over steps), signal half always 0.
        let j = job(3, 9, 7);
        assert_eq!(resolve_sacct_field(&j, 'x'), "3:9");
        assert_eq!(resolve_sacct_field(&j, 'X'), "7:0");
        assert_eq!(resolve_sacct_field(&job(0, 0, 0), 'x'), "0:0");
        assert_eq!(sacct_header('X'), "DerivedExitCode");
    }

    #[test]
    fn time_limit_field_renders() {
        // %l resolves to a formatted duration, or UNLIMITED when unset/zero.
        let mut j = job(0, 0, 0);
        assert_eq!(resolve_sacct_field(&j, 'l'), "UNLIMITED");
        j.time_limit = Some(prost_types::Duration {
            seconds: 3600,
            nanos: 0,
        });
        assert_eq!(resolve_sacct_field(&j, 'l'), format_duration(3600));
    }

    #[test]
    fn parse_acct_state_maps_deadline() {
        // Both the long form and the squeue short code resolve to the proto
        // JobDeadline discriminant, so `sacct --state=DEADLINE` filters work.
        assert_eq!(
            parse_acct_state("DEADLINE"),
            Some(JobState::JobDeadline as i32)
        );
        assert_eq!(parse_acct_state("DL"), Some(JobState::JobDeadline as i32));
        assert_eq!(
            parse_acct_state("deadline"),
            Some(JobState::JobDeadline as i32)
        );
    }

    #[test]
    fn parse_acct_state_round_trips_known_states() {
        let cases = [
            ("COMPLETED", JobState::JobCompleted),
            ("FAILED", JobState::JobFailed),
            ("CANCELLED", JobState::JobCancelled),
            ("TIMEOUT", JobState::JobTimeout),
            ("NODE_FAIL", JobState::JobNodeFail),
            ("DEADLINE", JobState::JobDeadline),
        ];
        for (s, expected) in cases {
            assert_eq!(parse_acct_state(s), Some(expected as i32), "state {s}");
        }
    }

    #[test]
    fn sacct_field_spec_is_case_insensitive_and_round_trips_header() {
        for name in ["JobID", "jobid", "JOBID"] {
            assert_eq!(sacct_field_spec(name), Some('i'));
        }
        assert_eq!(sacct_field_spec("NotAField"), None);
    }

    #[test]
    fn custom_format_comma_separated_names_populate_requested_columns() {
        // Real Slurm's sacct --format syntax; used to produce blank columns.
        let fields = format_engine::parse_named_format(
            "JobID,JobName,Partition,State",
            &sacct_field_spec,
            &sacct_header,
        );
        assert_eq!(fields.len(), 4);
        let mut j = job(0, 0, 0);
        j.name = "train".into();
        j.partition = "gpu".into();
        j.job_id = 42;
        let row = format_engine::format_row(&fields, &|spec| resolve_sacct_field(&j, spec));
        assert!(row.contains("42"));
        assert!(row.contains("train"));
        assert!(row.contains("gpu"));
    }

    #[tokio::test]
    async fn format_with_no_recognized_fields_fails_fast() {
        // Errors before ever connecting, so no server/network is needed here.
        let err = main_with_args(vec![
            "sacct".into(),
            "--format=NotAField,AlsoNotAField".into(),
        ])
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no recognized fields"));
    }
}
