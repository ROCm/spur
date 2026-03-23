use anyhow::{Context, Result};
use clap::Parser;
use spur_proto::proto::slurm_controller_client::SlurmControllerClient;
use spur_proto::proto::{GetJobsRequest, GetPartitionsRequest, JobState};

/// View job priority breakdown for pending jobs.
#[derive(Parser, Debug)]
#[command(name = "sprio", about = "View job priority factors")]
pub struct SprioArgs {
    /// Show only these job IDs (comma-separated)
    #[arg(short = 'j', long)]
    pub jobs: Option<String>,

    /// Show only jobs for this user
    #[arg(short = 'u', long)]
    pub user: Option<String>,

    /// Long format (more detail)
    #[arg(short = 'l', long)]
    pub long: bool,

    /// Don't print header
    #[arg(short = 'h', long)]
    pub noheader: bool,

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
    let args = SprioArgs::try_parse_from(&args)?;

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

    let mut client = SlurmControllerClient::connect(args.controller.clone())
        .await
        .context("failed to connect to spurctld")?;

    // Get pending jobs only (priority is relevant for pending jobs)
    let response = client
        .get_jobs(GetJobsRequest {
            states: vec![JobState::JobPending as i32],
            user: args.user.unwrap_or_default(),
            partition: String::new(),
            account: String::new(),
            job_ids,
        })
        .await
        .context("failed to get jobs")?;

    let jobs = response.into_inner().jobs;

    // Get partitions for tier lookup
    let partitions_resp = client
        .get_partitions(GetPartitionsRequest {
            name: String::new(),
        })
        .await
        .context("failed to get partitions")?;

    let partitions = partitions_resp.into_inner().partitions;

    let now = chrono::Utc::now();

    if args.long {
        if !args.noheader {
            println!(
                "{:>10} {:>10} {:>10} {:>10} {:>10} {:>12} {:>10} {:>10}",
                "JOBID", "USER", "PRIORITY", "AGE", "FAIRSHARE", "PARTITION", "QOS", "EFFECTIVE"
            );
        }

        for job in &jobs {
            let submit_secs = job
                .submit_time
                .as_ref()
                .map(|t| t.seconds)
                .unwrap_or(now.timestamp());
            let age_minutes = (now.timestamp() - submit_secs) / 60;
            let age_factor = 1.0 + (age_minutes as f64 / 10080.0).min(1.0);

            let partition_tier = partitions
                .iter()
                .find(|p| p.name == job.partition)
                .map(|p| p.priority_tier)
                .unwrap_or(1);

            let base = job.priority;
            // Approximate fair_share as 1.0 since we don't have usage data here
            let fair_share = 1.0_f64;
            let effective =
                (base as f64 * fair_share.min(10.0) * age_factor * partition_tier.max(1) as f64)
                    as u32;

            println!(
                "{:>10} {:>10} {:>10} {:>10.4} {:>10.4} {:>12} {:>10} {:>10}",
                job.job_id,
                job.user,
                base,
                age_factor,
                fair_share,
                format!("{}(T{})", job.partition, partition_tier),
                if job.qos.is_empty() {
                    "normal"
                } else {
                    &job.qos
                },
                effective,
            );
        }
    } else {
        if !args.noheader {
            println!(
                "{:>10} {:>10} {:>10} {:>10} {:>10} {:>10}",
                "JOBID", "USER", "PRIORITY", "AGE", "FAIRSHARE", "PARTITION"
            );
        }

        for job in &jobs {
            let submit_secs = job
                .submit_time
                .as_ref()
                .map(|t| t.seconds)
                .unwrap_or(now.timestamp());
            let age_minutes = (now.timestamp() - submit_secs) / 60;
            let age_factor = 1.0 + (age_minutes as f64 / 10080.0).min(1.0);

            let partition_tier = partitions
                .iter()
                .find(|p| p.name == job.partition)
                .map(|p| p.priority_tier)
                .unwrap_or(1);

            println!(
                "{:>10} {:>10} {:>10} {:>10.4} {:>10.4} {:>10}",
                job.job_id, job.user, job.priority, age_factor, 1.0, partition_tier,
            );
        }
    }

    Ok(())
}
