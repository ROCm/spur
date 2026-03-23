use anyhow::{bail, Context, Result};
use clap::Parser;
use spur_proto::proto::slurm_controller_client::SlurmControllerClient;
use spur_proto::proto::GetJobRequest;

/// Display status information for running jobs.
#[derive(Parser, Debug)]
#[command(name = "sstat", about = "Display status of running jobs")]
pub struct SstatArgs {
    /// Job ID to query
    #[arg(short = 'j', long = "jobs", required = true)]
    pub job_id: String,

    /// Output format (comma-separated field names)
    #[arg(short = 'o', long)]
    pub format: Option<String>,

    /// Don't print header
    #[arg(long)]
    pub noheader: bool,

    /// Parsable output (delimiter-separated)
    #[arg(short = 'p', long)]
    pub parsable: bool,

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
    let args = SstatArgs::try_parse_from(&args)?;

    // Parse job IDs (comma-separated)
    let job_ids: Vec<u32> = args
        .job_id
        .split(',')
        .filter_map(|j| j.trim().parse::<u32>().ok())
        .collect();

    if job_ids.is_empty() {
        bail!("sstat: no valid job IDs specified");
    }

    let mut client = SlurmControllerClient::connect(args.controller.clone())
        .await
        .context("failed to connect to spurctld")?;

    // Determine which fields to show
    let fields = if let Some(ref fmt) = args.format {
        parse_field_list(fmt)
    } else {
        default_fields()
    };

    let delimiter = if args.parsable { "|" } else { "  " };

    // Print header
    if !args.noheader {
        let headers: Vec<String> = fields.iter().map(|f| format_header(f)).collect();
        if args.parsable {
            println!("{}|", headers.join(delimiter));
        } else {
            println!("{}", headers.join(delimiter));
        }
        if !args.parsable {
            let sep: Vec<String> = fields.iter().map(|f| "-".repeat(field_width(f))).collect();
            println!("{}", sep.join(delimiter));
        }
    }

    for job_id in &job_ids {
        let response = client
            .get_job(GetJobRequest { job_id: *job_id })
            .await
            .context(format!("failed to get job {}", job_id))?;

        let job = response.into_inner();

        // Only show running jobs
        if job.state != spur_proto::proto::JobState::JobRunning as i32 {
            eprintln!(
                "sstat: job {} is not running (state: {})",
                job_id,
                state_name(job.state)
            );
            continue;
        }

        let values: Vec<String> = fields.iter().map(|f| resolve_field(&job, f)).collect();
        if args.parsable {
            println!("{}|", values.join(delimiter));
        } else {
            // Pad each value to field width
            let padded: Vec<String> = fields
                .iter()
                .zip(values.iter())
                .map(|(f, v)| format!("{:>width$}", v, width = field_width(f)))
                .collect();
            println!("{}", padded.join(delimiter));
        }
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
enum StatField {
    JobId,
    AveCpu,
    AveRss,
    AveVmSize,
    MaxRss,
    MaxVmSize,
    NTasks,
    NodeList,
    Cpus,
    MemAlloc,
    GpuAlloc,
    State,
    Elapsed,
}

fn default_fields() -> Vec<StatField> {
    vec![
        StatField::JobId,
        StatField::NTasks,
        StatField::Cpus,
        StatField::MemAlloc,
        StatField::GpuAlloc,
        StatField::Elapsed,
        StatField::NodeList,
    ]
}

fn parse_field_list(fmt: &str) -> Vec<StatField> {
    fmt.split(',')
        .filter_map(|name| match name.trim().to_lowercase().as_str() {
            "jobid" => Some(StatField::JobId),
            "avecpu" => Some(StatField::AveCpu),
            "averss" => Some(StatField::AveRss),
            "avevmsize" => Some(StatField::AveVmSize),
            "maxrss" => Some(StatField::MaxRss),
            "maxvmsize" => Some(StatField::MaxVmSize),
            "ntasks" => Some(StatField::NTasks),
            "nodelist" => Some(StatField::NodeList),
            "cpus" | "ncpus" => Some(StatField::Cpus),
            "memalloc" | "reqmem" => Some(StatField::MemAlloc),
            "gpualloc" | "gres" => Some(StatField::GpuAlloc),
            "state" => Some(StatField::State),
            "elapsed" => Some(StatField::Elapsed),
            _ => {
                eprintln!("sstat: unknown field '{}'", name.trim());
                None
            }
        })
        .collect()
}

fn format_header(field: &StatField) -> String {
    let (name, width) = header_info(field);
    format!("{:>width$}", name, width = width)
}

fn field_width(field: &StatField) -> usize {
    header_info(field).1
}

fn header_info(field: &StatField) -> (&'static str, usize) {
    match field {
        StatField::JobId => ("JobID", 10),
        StatField::AveCpu => ("AveCPU", 10),
        StatField::AveRss => ("AveRSS", 10),
        StatField::AveVmSize => ("AveVMSize", 10),
        StatField::MaxRss => ("MaxRSS", 10),
        StatField::MaxVmSize => ("MaxVMSize", 10),
        StatField::NTasks => ("NTasks", 8),
        StatField::NodeList => ("Nodelist", 20),
        StatField::Cpus => ("NCPUS", 8),
        StatField::MemAlloc => ("MemAlloc", 10),
        StatField::GpuAlloc => ("GPUAlloc", 10),
        StatField::State => ("State", 10),
        StatField::Elapsed => ("Elapsed", 12),
    }
}

fn resolve_field(job: &spur_proto::proto::JobInfo, field: &StatField) -> String {
    match field {
        StatField::JobId => job.job_id.to_string(),
        StatField::NTasks => job.num_tasks.to_string(),
        StatField::Cpus => {
            let cpus = job.num_tasks * job.cpus_per_task.max(1);
            cpus.to_string()
        }
        StatField::MemAlloc => {
            if let Some(ref res) = job.resources {
                format!("{}M", res.memory_mb)
            } else {
                "0M".into()
            }
        }
        StatField::GpuAlloc => {
            if let Some(ref res) = job.resources {
                if res.gpus.is_empty() {
                    "0".into()
                } else {
                    res.gpus.len().to_string()
                }
            } else {
                "0".into()
            }
        }
        StatField::NodeList => job.nodelist.clone(),
        StatField::State => state_name(job.state).to_string(),
        StatField::Elapsed => {
            if let Some(ref rt) = job.run_time {
                format_duration(rt.seconds)
            } else {
                "00:00:00".into()
            }
        }
        // These fields would require real-time process stats from the agent.
        // For now, show N/A since we don't poll agents for per-process metrics.
        StatField::AveCpu => "N/A".into(),
        StatField::AveRss => "N/A".into(),
        StatField::AveVmSize => "N/A".into(),
        StatField::MaxRss => "N/A".into(),
        StatField::MaxVmSize => "N/A".into(),
    }
}

fn state_name(state: i32) -> &'static str {
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
