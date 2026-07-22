// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::env_defaults::{apply_csv, apply_flag, apply_str, apply_string};
use anyhow::{Context, Result};
use clap::{ArgMatches, CommandFactory, FromArgMatches, Parser};
use spur_core::spur_env::SpurEnv;
use spur_proto::proto::{CancelJobRequest, GetJobRequest, JobSpec, SubmitJobRequest};
use std::collections::HashMap;

/// Allocate resources for an interactive job.
#[derive(Parser, Debug)]
#[command(name = "salloc", about = "Allocate resources for an interactive job")]
pub struct SallocArgs {
    /// Job name
    #[arg(short = 'J', long)]
    pub job_name: Option<String>,

    /// Partition
    #[arg(short = 'p', long)]
    pub partition: Option<String>,

    /// Account
    #[arg(short = 'A', long)]
    pub account: Option<String>,

    /// Number of nodes
    #[arg(short = 'N', long, default_value = "1")]
    pub nodes: u32,

    /// Number of tasks
    #[arg(short = 'n', long, default_value = "1")]
    pub ntasks: u32,

    /// CPUs per task
    #[arg(short = 'c', long, default_value = "1")]
    pub cpus_per_task: u32,

    /// Memory per node (e.g., "4G", "4096M")
    #[arg(long)]
    pub mem: Option<String>,

    /// Time limit
    #[arg(short = 't', long, default_value = "1:00:00")]
    pub time: String,

    /// GRES
    // Slurm semantics: comma-lists expand to multiple GRES; a repeated --gres
    // replaces rather than accumulates (last wins).
    #[arg(long, value_delimiter = ',', overrides_with = "gres")]
    pub gres: Vec<String>,

    /// GPUs
    #[arg(short = 'G', long)]
    pub gpus: Option<String>,

    /// Required node features (e.g., "mi300x,nvlink")
    #[arg(short = 'C', long)]
    pub constraint: Option<String>,

    /// Node list
    #[arg(
        short = 'w',
        long,
        overrides_with_all = ["nodelist", "nodefile"]
    )]
    pub nodelist: Option<String>,

    /// Read the node list from a file
    #[arg(
        short = 'F',
        long,
        overrides_with_all = ["nodelist", "nodefile"]
    )]
    pub nodefile: Option<String>,

    /// Exclude nodes
    #[arg(short = 'x', long)]
    pub exclude: Option<String>,

    /// Target a named reservation
    #[arg(long)]
    pub reservation: Option<String>,

    /// Exclusive node allocation
    #[arg(long)]
    pub exclusive: bool,

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
    let matches = SallocArgs::command().try_get_matches_from(&args)?;
    let mut args = SallocArgs::from_arg_matches(&matches)?;
    resolve_salloc_env(&matches, &mut args);
    let nodelist = crate::nodelist::resolve(args.nodelist.take(), args.nodefile.take())?;

    let name = args.job_name.unwrap_or_else(|| "interactive".into());
    let mut gres = args.gres;
    if let Some(gpus) = &args.gpus {
        gres.push(format!("gpu:{}", gpus));
    }

    let time_limit =
        spur_core::config::parse_time_minutes(&args.time).map(|mins| prost_types::Duration {
            seconds: mins as i64 * 60,
            nanos: 0,
        });

    let memory_mb = args
        .mem
        .as_ref()
        .map(|m| parse_memory_mb(m))
        .transpose()?
        .unwrap_or(0);

    let nodes = args.nodes;
    let controller = args.controller.clone();
    let exclusive = args.exclusive;
    let constraint = args.constraint;
    let exclude = args.exclude;
    let reservation = args.reservation;
    let partition = args.partition;
    let account = args.account;
    let ntasks = args.ntasks;
    let cpus_per_task = args.cpus_per_task;

    let channel = spur_client::connect_channel(&controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = spur_proto::controller_client(channel);

    // Submit interactive allocation (sleep infinity holds the allocation)
    let job_spec = JobSpec {
        name,
        partition: partition.unwrap_or_default(),
        account: account.unwrap_or_default(),
        user: whoami::username().unwrap_or_else(|_| "unknown".into()),
        uid: nix::unistd::getuid().as_raw(),
        gid: nix::unistd::getgid().as_raw(),
        num_nodes: nodes,
        num_tasks: ntasks,
        cpus_per_task,
        memory_per_node_mb: memory_mb,
        gres,
        script: "#!/bin/bash\nsleep infinity\n".into(),
        time_limit,
        exclusive,
        constraint: constraint.unwrap_or_default(),
        nodelist: nodelist.unwrap_or_default(),
        exclude: exclude.unwrap_or_default(),
        reservation: reservation.unwrap_or_default(),
        interactive: true,
        environment: HashMap::new(),
        ..Default::default()
    };

    let response = client
        .submit_job(SubmitJobRequest {
            spec: Some(job_spec),
        })
        .await
        .context("job submission failed")?;

    let job_id = response.into_inner().job_id;
    let user = whoami::username().unwrap_or_else(|_| "unknown".into());
    eprintln!("salloc: Pending job allocation {}...", job_id);

    // Set up Ctrl+C handler to cancel the job on interrupt
    let cancel_client = client.clone();
    let cancel_user = user.clone();
    tokio::spawn(async move {
        let mut cancel_client = cancel_client;
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\nsalloc: cancelling job {}...", job_id);
            let _ = cancel_client
                .cancel_job(CancelJobRequest {
                    job_id,
                    signal: 2, // SIGINT
                    user: cancel_user,
                })
                .await;
            std::process::exit(130); // Standard SIGINT exit code
        }
    });

    // Wait for the job to start running (with timeout and progress)
    let job_info;
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(300);
    let mut last_reason = String::new();
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        if start.elapsed() > timeout {
            eprintln!(
                "salloc: timed out waiting for job {} to start (last reason: {})",
                job_id, last_reason
            );
            let _ = client
                .cancel_job(CancelJobRequest {
                    job_id,
                    signal: 0,
                    user: whoami::username().unwrap_or_default(),
                })
                .await;
            std::process::exit(1);
        }

        match client.get_job(GetJobRequest { job_id }).await {
            Ok(resp) => {
                let job = resp.into_inner();
                match job.state {
                    1 => {
                        // RUNNING
                        job_info = job;
                        break;
                    }
                    3..=7 => {
                        // Terminal
                        eprintln!("salloc: job {} ended before allocation was granted", job_id);
                        std::process::exit(1);
                    }
                    _ => {
                        let reason = job.state_reason.clone();
                        if reason != last_reason && !reason.is_empty() && reason != "None" {
                            eprintln!("salloc: job {} pending ({})", job_id, reason);
                            last_reason = reason;
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("salloc: warning: {}", e.message());
            }
        }
    }

    let nodelist = &job_info.nodelist;
    eprintln!("salloc: Nodes {} are ready for job {}", nodelist, job_id);
    eprintln!("salloc: Granted job allocation {}", job_id);

    let mut env = SpurEnv::new();
    env.set_with_slurm_twin("SPUR_JOB_ID", job_id);
    env.set_with_slurm_twin("SPUR_JOBID", job_id);
    env.set_with_slurm_twin("SPUR_JOB_NAME", &job_info.name);
    env.set_with_slurm_twin("SPUR_JOB_PARTITION", &job_info.partition);
    env.set_with_slurm_twin("SPUR_JOB_ACCOUNT", &job_info.account);
    env.set_with_slurm_twin("SPUR_JOB_QOS", &job_info.qos);
    env.set_with_slurm_twin("SPUR_NODELIST", nodelist);
    env.set_with_slurm_twin("SPUR_JOB_NODELIST", nodelist);
    env.set_with_slurm_twin("SPUR_NNODES", job_info.num_nodes);
    env.set_with_slurm_twin("SPUR_JOB_NUM_NODES", job_info.num_nodes);
    env.set_with_slurm_twin("SPUR_NTASKS", job_info.num_tasks);
    env.set_with_slurm_twin("SPUR_NPROCS", job_info.num_tasks);
    env.set_with_slurm_twin("SPUR_CPUS_PER_TASK", job_info.cpus_per_task);
    env.set_with_slurm_twin("SPUR_SUBMIT_DIR", &job_info.work_dir);

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
    let mut cmd = tokio::process::Command::new(&shell);
    for (k, v) in env.into_map() {
        cmd.env(k, v);
    }
    let status = cmd
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .await
        .context("failed to spawn shell")?;

    // Shell exited — cancel allocation
    eprintln!("salloc: Relinquishing job allocation {}", job_id);
    let _ = client
        .cancel_job(CancelJobRequest {
            job_id,
            signal: 0,
            user,
        })
        .await;

    std::process::exit(status.code().unwrap_or(0));
}

/// Apply `SALLOC_*` environment-variable defaults (plus `SPUR_*` native twins)
/// to any flag not set on the command line. Mirrors real Slurm: salloc reads
/// command-prefixed `SALLOC_*` vars, and notably provides no env var for
/// `--nodes`, `--ntasks`, `--cpus-per-task`, or `--job-name`. CLI overrides env.
fn resolve_salloc_env(matches: &ArgMatches, args: &mut SallocArgs) {
    apply_str(
        matches,
        "partition",
        &["SPUR_PARTITION", "SALLOC_PARTITION"],
        &mut args.partition,
    );
    apply_str(
        matches,
        "account",
        &["SPUR_ACCOUNT", "SALLOC_ACCOUNT"],
        &mut args.account,
    );
    apply_str(
        matches,
        "mem",
        &["SPUR_MEM_PER_NODE", "SALLOC_MEM_PER_NODE"],
        &mut args.mem,
    );
    apply_string(
        matches,
        "time",
        &["SPUR_TIMELIMIT", "SALLOC_TIMELIMIT"],
        &mut args.time,
    );
    apply_csv(
        matches,
        "gres",
        &["SPUR_GRES", "SALLOC_GRES"],
        &mut args.gres,
    );
    apply_str(
        matches,
        "gpus",
        &["SPUR_GPUS", "SALLOC_GPUS"],
        &mut args.gpus,
    );
    apply_str(
        matches,
        "constraint",
        &["SPUR_CONSTRAINT", "SALLOC_CONSTRAINT"],
        &mut args.constraint,
    );
    apply_str(
        matches,
        "reservation",
        &["SPUR_RESERVATION", "SALLOC_RESERVATION"],
        &mut args.reservation,
    );
    apply_flag(
        matches,
        "exclusive",
        &["SPUR_EXCLUSIVE", "SALLOC_EXCLUSIVE"],
        &mut args.exclusive,
    );
}

fn parse_memory_mb(s: &str) -> Result<u64> {
    let s = s.trim();
    if let Some(gb) = s.strip_suffix('G').or_else(|| s.strip_suffix('g')) {
        let val: f64 = gb.parse().context("invalid memory value")?;
        Ok((val * 1024.0) as u64)
    } else if let Some(mb) = s.strip_suffix('M').or_else(|| s.strip_suffix('m')) {
        Ok(mb.parse().context("invalid memory value")?)
    } else {
        Ok(s.parse().context("invalid memory value")?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nodelist_and_exclude_short() {
        let args = SallocArgs::try_parse_from(["salloc", "-w", "node001,node002", "-x", "node003"])
            .expect("parse failed");
        assert_eq!(args.nodelist.as_deref(), Some("node001,node002"));
        assert_eq!(args.exclude.as_deref(), Some("node003"));
    }

    #[test]
    fn parses_nodelist_and_exclude_long() {
        let args =
            SallocArgs::try_parse_from(["salloc", "--nodelist", "node001", "--exclude", "node002"])
                .expect("parse failed");
        assert_eq!(args.nodelist.as_deref(), Some("node001"));
        assert_eq!(args.exclude.as_deref(), Some("node002"));
    }

    #[test]
    fn parses_nodefile_short_and_long() {
        let short = SallocArgs::try_parse_from(["salloc", "-F", "nodes.txt"])
            .expect("parse short nodefile");
        assert_eq!(short.nodefile.as_deref(), Some("nodes.txt"));
        assert!(short.nodelist.is_none());

        let long = SallocArgs::try_parse_from(["salloc", "--nodefile=other.txt"])
            .expect("parse long nodefile");
        assert_eq!(long.nodefile.as_deref(), Some("other.txt"));
        assert!(long.nodelist.is_none());
    }

    // These mutate process-global env vars, so they run serially and use the
    // shared EnvGuard, which clears every SPUR_/SLURM_/SALLOC_/SRUN_-prefixed
    // var on construction and drop to stay independent of the runner's env.
    use crate::env_defaults::EnvGuard;
    use serial_test::serial;

    fn resolve_from(cli: &[&str]) -> SallocArgs {
        let matches = SallocArgs::command()
            .try_get_matches_from(cli)
            .expect("parse failed");
        let mut args = SallocArgs::from_arg_matches(&matches).expect("from_arg_matches failed");
        resolve_salloc_env(&matches, &mut args);
        args
    }

    #[test]
    #[serial(env_injection)]
    fn env_provides_defaults() {
        let env = EnvGuard::new();
        env.set("SALLOC_PARTITION", "gpu");
        env.set("SALLOC_ACCOUNT", "proj");
        let args = resolve_from(&["salloc"]);
        assert_eq!(args.partition.as_deref(), Some("gpu"));
        assert_eq!(args.account.as_deref(), Some("proj"));
    }

    #[test]
    #[serial(env_injection)]
    fn cli_overrides_env() {
        let env = EnvGuard::new();
        env.set("SALLOC_PARTITION", "gpu");
        let args = resolve_from(&["salloc", "--partition=cpu"]);
        assert_eq!(args.partition.as_deref(), Some("cpu"));
    }

    #[test]
    #[serial(env_injection)]
    fn spur_native_alias_works() {
        let env = EnvGuard::new();
        env.set("SPUR_PARTITION", "spur-part");
        let args = resolve_from(&["salloc"]);
        assert_eq!(args.partition.as_deref(), Some("spur-part"));
    }

    #[test]
    #[serial(env_injection)]
    fn timelimit_env_overrides_default_but_not_cli() {
        let env = EnvGuard::new();
        env.set("SALLOC_TIMELIMIT", "2:00:00");
        assert_eq!(resolve_from(&["salloc"]).time, "2:00:00");

        let args = resolve_from(&["salloc", "--time=8:00:00"]);
        assert_eq!(args.time, "8:00:00");
    }

    #[test]
    #[serial(env_injection)]
    fn exclusive_flag_semantics() {
        let env = EnvGuard::new();

        env.set("SALLOC_EXCLUSIVE", "yes");
        assert!(resolve_from(&["salloc"]).exclusive);

        env.set("SALLOC_EXCLUSIVE", "0");
        assert!(!resolve_from(&["salloc"]).exclusive);
    }

    #[test]
    #[serial(env_injection)]
    fn gres_comma_split_and_more() {
        let env = EnvGuard::new();
        env.set("SALLOC_GRES", "gpu:2,fpga:1");
        env.set("SALLOC_MEM_PER_NODE", "8G");
        env.set("SALLOC_GPUS", "4");
        env.set("SALLOC_CONSTRAINT", "mi300x");
        let args = resolve_from(&["salloc"]);
        assert_eq!(args.gres, vec!["gpu:2", "fpga:1"]);
        assert_eq!(args.mem.as_deref(), Some("8G"));
        assert_eq!(args.gpus.as_deref(), Some("4"));
        assert_eq!(args.constraint.as_deref(), Some("mi300x"));
    }

    #[test]
    fn cli_gres_comma_list_splits_into_multiple() {
        let args =
            SallocArgs::try_parse_from(["salloc", "--gres=gpu:1,fpga:2"]).expect("parse failed");
        assert_eq!(args.gres, vec!["gpu:1", "fpga:2"]);
    }

    #[test]
    fn cli_repeated_gres_last_wins() {
        let args = SallocArgs::try_parse_from(["salloc", "--gres=gpu:1", "--gres=fpga:2"])
            .expect("parse failed");
        assert_eq!(args.gres, vec!["fpga:2"]);
    }

    #[test]
    #[serial(env_injection)]
    fn slurm_faithful_no_env_for_nodes_or_job_name() {
        let env = EnvGuard::new();
        // These vars have no effect on salloc in real Slurm; Spur ignores them.
        env.set("SALLOC_NNODES", "9");
        env.set("SLURM_NNODES", "9");
        env.set("SALLOC_JOB_NAME", "envname");
        env.set("SLURM_JOB_NAME", "envname");
        let args = resolve_from(&["salloc"]);
        assert_eq!(args.nodes, 1, "nodes must stay at the CLI default");
        assert!(args.job_name.is_none(), "job_name must stay unset");
    }
}
