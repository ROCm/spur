use anyhow::{Context, Result};
use clap::Parser;
use spur_proto::proto::slurm_controller_client::SlurmControllerClient;
use spur_proto::proto::{GetJobsRequest, JobState};

/// Display scheduler diagnostics and statistics.
#[derive(Parser, Debug)]
#[command(name = "sdiag", about = "Display scheduling diagnostics")]
pub struct SdiagArgs {
    /// Don't print header
    #[arg(long)]
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
    let args = SdiagArgs::try_parse_from(&args)?;

    let mut client = SlurmControllerClient::connect(args.controller.clone())
        .await
        .context("failed to connect to spurctld")?;

    // Ping to get server info
    let ping_resp = client.ping(()).await.context("failed to ping controller")?;

    let ping = ping_resp.into_inner();

    // Get all jobs (no state filter) to compute stats
    let all_jobs_resp = client
        .get_jobs(GetJobsRequest {
            states: vec![
                JobState::JobPending as i32,
                JobState::JobRunning as i32,
                JobState::JobCompleting as i32,
                JobState::JobCompleted as i32,
                JobState::JobFailed as i32,
                JobState::JobCancelled as i32,
                JobState::JobTimeout as i32,
                JobState::JobNodeFail as i32,
                JobState::JobPreempted as i32,
                JobState::JobSuspended as i32,
            ],
            user: String::new(),
            partition: String::new(),
            account: String::new(),
            job_ids: Vec::new(),
        })
        .await
        .context("failed to get jobs")?;

    let jobs = all_jobs_resp.into_inner().jobs;

    // Count by state
    let mut pending = 0u32;
    let mut running = 0u32;
    let mut completed = 0u32;
    let mut failed = 0u32;
    let mut cancelled = 0u32;
    let mut timeout = 0u32;
    let total = jobs.len() as u32;

    for job in &jobs {
        match job.state {
            s if s == JobState::JobPending as i32 => pending += 1,
            s if s == JobState::JobRunning as i32 => running += 1,
            s if s == JobState::JobCompleted as i32 => completed += 1,
            s if s == JobState::JobFailed as i32 => failed += 1,
            s if s == JobState::JobCancelled as i32 => cancelled += 1,
            s if s == JobState::JobTimeout as i32 => timeout += 1,
            _ => {}
        }
    }

    let server_time = ping
        .server_time
        .as_ref()
        .map(|t| {
            chrono::DateTime::from_timestamp(t.seconds, t.nanos as u32)
                .unwrap_or_default()
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "N/A".into());

    println!("***********************************************");
    println!("sdiag output at {}", server_time);
    println!("***********************************************");
    println!();

    println!("Server Information:");
    println!("  Hostname          : {}", ping.hostname);
    println!("  Version           : {}", ping.version);
    println!("  Server Time       : {}", server_time);

    if !ping.federation_peers.is_empty() {
        println!("  Federation Peers  : {}", ping.federation_peers.join(", "));
    }

    println!();
    println!("Job Statistics:");
    println!("  Total Jobs        : {}", total);
    println!("  Pending           : {}", pending);
    println!("  Running           : {}", running);
    println!("  Completed         : {}", completed);
    println!("  Failed            : {}", failed);
    println!("  Cancelled         : {}", cancelled);
    println!("  Timeout           : {}", timeout);

    // Compute some derived stats
    let finished = completed + failed + cancelled + timeout;
    let success_rate = if finished > 0 {
        (completed as f64 / finished as f64) * 100.0
    } else {
        0.0
    };

    println!();
    println!("Derived Statistics:");
    println!("  Finished Jobs     : {}", finished);
    println!("  Success Rate      : {:.1}%", success_rate);
    println!(
        "  Active Jobs       : {} (pending + running)",
        pending + running
    );

    // Scheduler info (what we can infer)
    println!();
    println!("Scheduler:");
    println!("  Algorithm         : multifactor priority + backfill");
    println!("  Priority Weights  : age=1.0 fairshare=1.0 partition_tier=1.0");
    println!("  Age Half-Life     : 7 days (10080 minutes)");

    Ok(())
}
