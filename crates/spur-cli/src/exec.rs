//! `spur exec` — execute a command inside a running container job.

use anyhow::{Context, Result};
use clap::Parser;
use spur_proto::proto::slurm_agent_client::SlurmAgentClient;
use spur_proto::proto::ExecInJobRequest;

/// Execute a command inside a running containerized job.
#[derive(Parser, Debug)]
#[command(
    name = "exec",
    about = "Execute a command inside a running job's container"
)]
pub struct ExecArgs {
    /// Job ID
    pub job_id: u32,

    /// Agent address (host:port of the node running the job)
    #[arg(long, env = "SPUR_AGENT_ADDR", default_value = "http://localhost:6818")]
    pub agent: String,

    /// Command to execute
    #[arg(trailing_var_arg = true, required = true)]
    pub command: Vec<String>,
}

pub async fn main() -> Result<()> {
    main_with_args(std::env::args().collect()).await
}

pub async fn main_with_args(args: Vec<String>) -> Result<()> {
    let args = ExecArgs::try_parse_from(&args)?;

    let mut client = SlurmAgentClient::connect(args.agent.clone())
        .await
        .context("failed to connect to agent")?;

    let resp = client
        .exec_in_job(ExecInJobRequest {
            job_id: args.job_id,
            command: args.command.clone(),
        })
        .await
        .context("exec failed")?;

    let inner = resp.into_inner();

    if !inner.stdout.is_empty() {
        print!("{}", inner.stdout);
    }
    if !inner.stderr.is_empty() {
        eprint!("{}", inner.stderr);
    }

    std::process::exit(inner.exit_code);
}
