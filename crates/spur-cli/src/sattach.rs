use anyhow::{Context, Result};
use clap::Parser;
use spur_proto::proto::slurm_agent_client::SlurmAgentClient;
use spur_proto::proto::slurm_controller_client::SlurmControllerClient;
use spur_proto::proto::{GetJobRequest, JobState, StreamJobOutputRequest};
use std::io::Write;

/// Attach to a running job step's standard I/O.
#[derive(Parser, Debug)]
#[command(name = "sattach", about = "Attach to a running job step")]
pub struct SattachArgs {
    /// Job ID (or job_id.step_id)
    pub job_step: String,

    /// Stream to attach to
    #[arg(long, default_value = "stdout")]
    pub output: String,

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
    let args = SattachArgs::try_parse_from(&args)?;

    // Parse job_id from "job_id" or "job_id.step_id"
    let job_id: u32 = args
        .job_step
        .split('.')
        .next()
        .and_then(|s| s.parse().ok())
        .context("sattach: invalid job ID format (expected JOB_ID or JOB_ID.STEP_ID)")?;

    let mut client = SlurmControllerClient::connect(args.controller)
        .await
        .context("failed to connect to spurctld")?;

    // Look up the job to find which node it is running on
    let job = client
        .get_job(GetJobRequest { job_id })
        .await
        .context("failed to get job info")?
        .into_inner();

    if job.state != JobState::JobRunning as i32 {
        eprintln!(
            "sattach: job {} is not running (state={})",
            job_id,
            state_name(job.state)
        );
        std::process::exit(1);
    }

    let nodelist = &job.nodelist;
    if nodelist.is_empty() {
        anyhow::bail!("sattach: job {} has no allocated nodes", job_id);
    }

    // Connect to the first node's agent
    let first_node = nodelist.split(',').next().unwrap_or(nodelist).trim();

    let agent_addr = format!("http://{}:6818", first_node);
    let mut agent = SlurmAgentClient::connect(agent_addr.clone())
        .await
        .context(format!("failed to connect to agent at {}", agent_addr))?;

    let mut stream = agent
        .stream_job_output(StreamJobOutputRequest {
            job_id,
            stream: args.output.clone(),
        })
        .await
        .context("failed to start output stream")?
        .into_inner();

    eprintln!("sattach: attached to job {} ({})", job_id, args.output);

    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    loop {
        match stream.message().await {
            Ok(Some(chunk)) => {
                if chunk.eof {
                    break;
                }
                let _ = handle.write_all(&chunk.data);
                let _ = handle.flush();
            }
            Ok(None) => break,
            Err(e) => {
                eprintln!("sattach: stream error: {}", e);
                break;
            }
        }
    }

    Ok(())
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
