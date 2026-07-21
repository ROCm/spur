// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use clap::Parser;
use spur_proto::proto::slurm_agent_client::SlurmAgentClient;
use spur_proto::proto::{GetJobRequest, JobState, StreamJobOutputRequest};
use std::io::Write;

/// Attach to a running job step's standard I/O.
#[derive(Parser, Debug)]
#[command(name = "sattach", about = "Attach to a running job step")]
pub struct SattachArgs {
    /// Job ID (or job_id.step_id)
    pub job_step: String,

    /// Stream to attach to (stdout or stderr) — used in output-only mode
    #[arg(long, default_value = "stdout")]
    pub output: String,

    /// Output-only mode: stream job output without interactive stdin forwarding
    #[arg(long)]
    pub output_only: bool,

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

    let channel = spur_client::connect_channel(&args.controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = spur_proto::controller_client(channel);

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
    let mut agent = crate::interactive::connect_agent(&agent_addr).await?;

    if args.output_only {
        stream_output_only(&mut agent, job_id, &args.output).await
    } else {
        let exit_code = interactive_attach(&mut agent, job_id).await?;
        std::process::exit(exit_code);
    }
}

/// Stream job output without interactive input (legacy behavior).
async fn stream_output_only(
    agent: &mut SlurmAgentClient<tonic::transport::Channel>,
    job_id: u32,
    stream_name: &str,
) -> Result<()> {
    let mut stream = agent
        .stream_job_output(StreamJobOutputRequest {
            job_id,
            stream: stream_name.to_string(),
        })
        .await
        .context("failed to start output stream")?
        .into_inner();

    eprintln!(
        "sattach: streaming {} for job {} (output-only)",
        stream_name, job_id
    );

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

/// Interactive attach via InteractiveSession RPC. Returns the remote exit code.
async fn interactive_attach(
    agent: &mut SlurmAgentClient<tonic::transport::Channel>,
    job_id: u32,
) -> Result<i32> {
    let winsize = crate::interactive::get_terminal_size();
    crate::interactive::run_interactive_session(agent, job_id, 0, Vec::new(), winsize, true).await
}

fn state_name(state: i32) -> &'static str {
    spur_core::job::JobState::from_proto_i32(state)
        .map(|s| s.display())
        .unwrap_or("UNKNOWN")
}
