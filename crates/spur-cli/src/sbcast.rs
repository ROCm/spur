// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `sbcast` — broadcast a local file to node-local storage on every node of a
//! running job's allocation (Slurm-compatible).

use anyhow::{bail, Context, Result};
use clap::Parser;
use spur_proto::proto::SbcastRequest;

/// Transmit a file to the nodes allocated to a running job.
#[derive(Parser, Debug)]
#[command(
    name = "sbcast",
    about = "Broadcast a file to node-local storage across a job's allocated nodes"
)]
pub struct SbcastArgs {
    /// Source file on the local (submit) host
    pub source: String,

    /// Destination path on each allocated node (relative resolves against the job work dir)
    pub dest: String,

    /// Overwrite an existing destination file
    #[arg(short = 'f', long)]
    pub force: bool,

    /// Job ID (defaults to $SPUR_JOB_ID / $SLURM_JOB_ID inside an allocation)
    #[arg(short = 'j', long = "jobid")]
    pub jobid: Option<u32>,

    /// Accepted for Slurm compatibility (compression is not yet implemented)
    #[arg(short = 'C', long, hide = true)]
    pub compress: bool,

    /// Accepted for Slurm compatibility (mode is always taken from the source file)
    #[arg(short = 'p', long, hide = true)]
    pub preserve: bool,

    /// Controller address (the controller fans the file out to the compute nodes)
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
    let args = SbcastArgs::try_parse_from(&args)?;

    let job_id = match args.jobid {
        Some(j) => j,
        None => job_id_from_env().context(
            "no job id: pass --jobid or run inside an allocation (SPUR_JOB_ID / SLURM_JOB_ID)",
        )?,
    };

    let meta = std::fs::metadata(&args.source)
        .with_context(|| format!("cannot stat source file '{}'", args.source))?;
    if !meta.is_file() {
        bail!("source '{}' is not a regular file", args.source);
    }
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o7777
    };
    let data = std::fs::read(&args.source)
        .with_context(|| format!("cannot read source file '{}'", args.source))?;

    let channel = crate::authclient::connect(&args.controller)
        .await
        .context("failed to connect to controller")?;
    let mut client = spur_proto::controller_client(channel);

    let resp = client
        .sbcast(SbcastRequest {
            job_id,
            dest: args.dest.clone(),
            data,
            mode,
            force: args.force,
            user: crate::interactive::current_user()?,
        })
        .await
        .context("sbcast failed")?
        .into_inner();

    if resp.success {
        println!(
            "sbcast: {} -> {} on {} node(s)",
            args.source,
            args.dest,
            resp.nodes.len()
        );
        Ok(())
    } else {
        bail!("sbcast failed: {}", resp.message);
    }
}

/// Resolve the job id from the allocation environment, mirroring Slurm's
/// SLURM_JOB_ID lookup (spur sets SPUR_JOB_ID; SLURM_JOB_ID is honored for
/// drop-in compatibility).
fn job_id_from_env() -> Result<u32> {
    for var in ["SPUR_JOB_ID", "SLURM_JOB_ID", "SLURM_JOBID"] {
        if let Ok(v) = std::env::var(var) {
            if let Ok(id) = v.trim().parse::<u32>() {
                return Ok(id);
            }
        }
    }
    bail!("job id not present in environment")
}
