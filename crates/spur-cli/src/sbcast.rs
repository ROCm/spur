// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `sbcast` — broadcast a local file to node-local storage on every node of a
//! running job's allocation (Slurm-compatible).

use anyhow::{bail, Context, Result};
use clap::Parser;
use spur_proto::proto::SbcastRequest;

/// The file rides in one gRPC message, so it has to fit the controller's
/// inbound cap with room left for the rest of the request.
const MAX_SBCAST_BYTES: usize = spur_proto::MAX_GRPC_REQUEST_SIZE - 64 * 1024;

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
    let args = crate::clap_exit::parse_or_exit::<SbcastArgs>(&args);

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
    check_transferable(&args.source, data.len())?;

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

/// Reject a source the transport cannot carry, so the user gets the size and
/// the limit instead of a bare gRPC "message too large".
fn check_transferable(source: &str, len: usize) -> Result<()> {
    if len > MAX_SBCAST_BYTES {
        bail!("source '{source}' is {len} bytes, over the {MAX_SBCAST_BYTES} byte sbcast limit");
    }
    Ok(())
}

/// Resolve the job id from the allocation environment, mirroring Slurm's
/// SLURM_JOB_ID lookup (spur sets SPUR_JOB_ID; SLURM_JOB_ID is honored for
/// drop-in compatibility).
fn job_id_from_env() -> Result<u32> {
    job_id_from(|var| std::env::var(var).ok())
}

/// The lookup is injected because mutating the real environment races every
/// other test in the same binary.
fn job_id_from(lookup: impl Fn(&str) -> Option<String>) -> Result<u32> {
    for var in ["SPUR_JOB_ID", "SLURM_JOB_ID", "SLURM_JOBID"] {
        if let Some(v) = lookup(var) {
            if let Ok(id) = v.trim().parse::<u32>() {
                return Ok(id);
            }
        }
    }
    bail!("job id not present in environment")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_positionals_and_flags() {
        let a = SbcastArgs::try_parse_from(["sbcast", "-f", "-j", "42", "src.bin", "/tmp/dst.bin"])
            .unwrap();
        assert_eq!(a.source, "src.bin");
        assert_eq!(a.dest, "/tmp/dst.bin");
        assert!(a.force);
        assert_eq!(a.jobid, Some(42));
    }

    #[test]
    fn slurm_only_flags_parse_but_stay_inert() {
        let a = SbcastArgs::try_parse_from(["sbcast", "-C", "-p", "s", "d"]).unwrap();
        assert!(a.compress);
        assert!(a.preserve);
        assert!(!a.force);
    }

    #[test]
    fn both_positionals_are_required() {
        assert!(SbcastArgs::try_parse_from(["sbcast"]).is_err());
        assert!(SbcastArgs::try_parse_from(["sbcast", "only-source"]).is_err());
    }

    #[test]
    fn spur_job_id_wins_over_the_slurm_names() {
        let env = |v: &str| match v {
            "SPUR_JOB_ID" => Some("7".to_string()),
            "SLURM_JOB_ID" => Some("9".to_string()),
            _ => None,
        };
        assert_eq!(job_id_from(env).unwrap(), 7);
    }

    #[test]
    fn unset_and_unparsable_vars_fall_through() {
        let env = |v: &str| match v {
            "SPUR_JOB_ID" => Some("   ".to_string()),
            "SLURM_JOBID" => Some(" 11 ".to_string()),
            _ => None,
        };
        assert_eq!(job_id_from(env).unwrap(), 11);
        assert!(job_id_from(|_| None).is_err());
    }

    #[test]
    fn a_source_over_the_message_cap_is_refused_before_transfer() {
        assert!(check_transferable("big.bin", MAX_SBCAST_BYTES).is_ok());
        let err = check_transferable("big.bin", MAX_SBCAST_BYTES + 1)
            .unwrap_err()
            .to_string();
        assert!(err.contains("sbcast limit"), "{err}");
    }
}
