use anyhow::{Context, Result};
use clap::Parser;
use spur_proto::proto::slurm_accounting_client::SlurmAccountingClient;
use spur_proto::proto::GetJobHistoryRequest;

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
    #[arg(long)]
    pub noheader: bool,

    /// Max records
    #[arg(long, default_value = "100")]
    pub limit: u32,

    /// Accounting daemon address
    #[arg(
        long,
        env = "SPUR_ACCOUNTING_ADDR",
        default_value = "http://localhost:6819"
    )]
    pub accounting: String,
}

const SACCT_DEFAULT_FORMAT: &str = "%.8i %.15j %.10u %.10a %.10P %.8T %10M %.8D %6x";
const SACCT_LONG_FORMAT: &str = "%.8i %.15j %.10u %.10a %.10P %.8T %10M %.8D %6x %.19S %.19E %.10l";
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

pub async fn main() -> Result<()> {
    main_with_args(std::env::args().collect()).await
}

pub async fn main_with_args(args: Vec<String>) -> Result<()> {
    let args = SacctArgs::try_parse_from(&args)?;

    let fmt = if let Some(ref f) = args.format {
        f.clone()
    } else if args.long {
        SACCT_LONG_FORMAT.to_string()
    } else if args.brief {
        SACCT_BRIEF_FORMAT.to_string()
    } else {
        SACCT_DEFAULT_FORMAT.to_string()
    };

    let fields = format_engine::parse_format(&fmt, &sacct_header);

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

    let mut client = SlurmAccountingClient::connect(args.accounting)
        .await
        .context("failed to connect to spurdbd")?;

    let response = client
        .get_job_history(GetJobHistoryRequest {
            user: args.user.unwrap_or_default(),
            account: args.account.unwrap_or_default(),
            start_after: None, // TODO: parse starttime
            start_before: None,
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
        'x' => format_exit_code(job.exit_code),
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
    match state {
        0 => "PENDING",
        1 => "RUNNING",
        2 => "COMPLETING",
        3 => "COMPLETED",
        4 => "FAILED",
        5 => "CANCELLED",
        6 => "TIMEOUT",
        7 => "NODE_FAIL",
        8 => "PREEMPTED",
        9 => "SUSPENDED",
        _ => "UNKNOWN",
    }
    .into()
}

fn parse_acct_state(s: &str) -> Option<i32> {
    match s.to_uppercase().as_str() {
        "CD" | "COMPLETED" => Some(3),
        "F" | "FAILED" => Some(4),
        "CA" | "CANCELLED" => Some(5),
        "TO" | "TIMEOUT" => Some(6),
        "NF" | "NODE_FAIL" => Some(7),
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

fn format_exit_code(code: i32) -> String {
    // Slurm format: exit_code:signal
    if code >= 0 {
        format!("{}:0", code)
    } else {
        format!("0:{}", -code)
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
