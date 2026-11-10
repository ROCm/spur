// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::env_defaults::{
    apply_csv, apply_flag, apply_num_opt_nonzero, apply_str, apply_string, was_cli_set,
};
use crate::resource_opts::{option_source, requests_gpus, require_gpus, surviving, CPU_CONFLICT};
use anyhow::{Context, Result};
use clap::{ArgMatches, CommandFactory, FromArgMatches, Parser};
use spur_core::spur_env::SpurEnv;
use spur_proto::proto::slurm_controller_client::SlurmControllerClient;
use spur_proto::proto::{CancelJobRequest, GetJobRequest, JobInfo, JobSpec, SubmitJobRequest};
use std::collections::HashMap;
use std::io::IsTerminal;

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

    /// QoS
    #[arg(short = 'q', long)]
    pub qos: Option<String>,

    /// Number of nodes
    #[arg(short = 'N', long, default_value = "1")]
    pub nodes: u32,

    /// Number of tasks (default: one per node)
    #[arg(short = 'n', long)]
    pub ntasks: Option<u32>,

    /// CPUs per task
    #[arg(
        short = 'c',
        long,
        overrides_with = "cpus_per_task",
        value_parser = clap::value_parser!(u32).range(1..)
    )]
    pub cpus_per_task: Option<u32>,

    /// Mount the node's credential mint socket into the container of each
    /// `srun --container-image` step in this allocation, so a nested srun there
    /// can authenticate (anything in the container can then act on the cluster
    /// as you)
    #[arg(long)]
    pub container_auth_socket: bool,

    /// Memory per node (e.g., "4G", "4096M")
    #[arg(long, overrides_with = "mem")]
    pub mem: Option<String>,

    /// CPUs per allocated GPU
    #[arg(long, overrides_with = "cpus_per_gpu", value_parser = clap::value_parser!(u32).range(1..))]
    pub cpus_per_gpu: Option<u32>,

    /// Memory per allocated GPU (e.g., "8G")
    #[arg(long, overrides_with = "mem_per_gpu")]
    pub mem_per_gpu: Option<String>,

    /// Time limit
    #[arg(short = 't', long, default_value = "1:00:00")]
    pub time: String,

    /// GRES
    // Slurm semantics: comma-lists expand to multiple GRES; a repeated --gres
    // replaces rather than accumulates (last wins).
    #[arg(long, value_delimiter = ',', overrides_with = "gres")]
    pub gres: Vec<String>,

    /// GPUs total across the job (e.g., "4" or "mi300x:4")
    #[arg(short = 'G', long)]
    pub gpus: Option<String>,

    /// GPUs per node
    #[arg(long)]
    pub gpus_per_node: Option<String>,

    /// GPUs per task
    #[arg(long)]
    pub gpus_per_task: Option<String>,

    /// Required node features and GPU predicates: gpu.<field><op><value>, same(gpu.<field>)
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

    /// Start the shell as an interactive step on the first allocated node
    /// (overrides `[salloc] use_interactive_step`)
    #[arg(long, overrides_with = "no_interactive_step")]
    pub interactive_step: bool,

    /// Start the shell on this host (overrides `[salloc] use_interactive_step`)
    #[arg(long, overrides_with = "interactive_step")]
    pub no_interactive_step: bool,
    /// Let other users start sessions in this allocation with `srun --jobid`:
    /// comma-separated usernames, and/or `@account` for every member of the job's account
    #[arg(long, value_name = "USERS")]
    pub share_with: Option<String>,

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
    let submit_line = crate::submitline::render(&args);
    let mut args = resolve_salloc_args(&args).unwrap_or_else(|e| e.exit());
    let nodelist = crate::nodelist::resolve(args.nodelist.take(), args.nodefile.take())?;

    let controller = args.controller.clone();
    let interactive_step = use_interactive_step(
        &args,
        crate::spur_config::load_spur_config()
            .salloc
            .use_interactive_step,
        std::io::stdin().is_terminal(),
    );
    let submission = Submission {
        work_dir: std::env::current_dir()
            .map(|dir| dir.to_string_lossy().into_owned())
            .unwrap_or_default(),
        environment: crate::export_env::without_credentials(crate::export_env::submission_env(
            "salloc",
        )),
    };
    let job_spec = build_salloc_job_spec(&args, nodelist, &submit_line, submission)?;
    let submit_user = job_spec.user.clone();

    let channel = crate::authclient::connect(&controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = spur_proto::controller_client(channel);

    crate::resource_opts::require_per_gpu_options_capability(
        &mut client,
        job_spec.cpus_per_gpu,
        job_spec.memory_per_gpu_mb,
    )
    .await?;

    let response = client
        .submit_job(SubmitJobRequest {
            spec: Some(job_spec),
        })
        .await
        .context("job submission failed")?;

    let response = response.into_inner();
    for warning in &response.warnings {
        eprintln!("salloc: warning: {warning}");
    }
    let job_id = response.job_id;
    eprintln!("salloc: Pending job allocation {}...", job_id);

    crate::interactive::install_ctrl_c_cancel(
        client.clone(),
        job_id,
        submit_user.clone(),
        "salloc",
    );

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
            let cancel_user =
                crate::interactive::resolve_job_owner_for_cancel(&mut client, job_id, &submit_user)
                    .await;
            let _ = client
                .cancel_job(CancelJobRequest {
                    job_id,
                    signal: 0,
                    user: cancel_user,
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

    let owner = job_info.user.clone();
    if owner.is_empty() {
        anyhow::bail!("salloc: job {} has no owner after allocation", job_id);
    }

    // Keep the allocation attended while the shell is open: absence of these
    // pings past the controller's InactiveLimit is what lets it reap an
    // abandoned allocation. The guard stops the pings on drop.
    let _keepalive = crate::interactive::spawn_keepalive(
        client.clone(),
        controller.clone(),
        job_id,
        owner.clone(),
        "salloc",
    );

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
    // The cancel handler stays armed in both modes: a raw-mode session sends a
    // typed Ctrl-C to the remote shell as a byte, so only a SIGINT outside one
    // (session setup, reconnects, no controlling terminal) cancels the allocation.
    let exit_code = if interactive_step {
        run_shell_on_node(&mut client, job_id, &owner, &shell).await
    } else {
        run_local_shell(&shell, allocation_shell_env(job_id, &job_info)).await?
    };

    drop(_keepalive);

    // Shell exited — cancel allocation
    eprintln!("salloc: Relinquishing job allocation {}", job_id);
    let _ = client
        .cancel_job(CancelJobRequest {
            job_id,
            signal: 0,
            user: owner,
        })
        .await;

    std::process::exit(exit_code);
}

/// A `--interactive-step`/`--no-interactive-step` flag beats the site's
/// `[salloc] use_interactive_step`, which applies only to a terminal: a PTY
/// step never sees the end of piped input, so a scripted salloc stays local.
fn use_interactive_step(args: &SallocArgs, configured: bool, stdin_is_tty: bool) -> bool {
    if args.interactive_step {
        return true;
    }
    if args.no_interactive_step {
        return false;
    }
    configured && stdin_is_tty
}

/// Run the shell as an interactive step on the allocation's first node, the
/// controller's pick for an untargeted step. A session that never runs exits
/// 1, so the caller still releases the allocation.
async fn run_shell_on_node(
    client: &mut SlurmControllerClient<crate::authclient::AuthChannel>,
    job_id: u32,
    owner: &str,
    shell: &str,
) -> i32 {
    let session = crate::srun::run_interactive_pty(
        client,
        job_id,
        vec![shell.to_string()],
        String::new(),
        owner,
        None,
        crate::srun::PtySession::Extra,
    )
    .await;
    match session {
        Ok(exit_code) => exit_code,
        Err(e) => {
            eprintln!("salloc: error: interactive step failed: {e:#}");
            1
        }
    }
}

async fn run_local_shell(shell: &str, env: SpurEnv) -> Result<i32> {
    let mut cmd = tokio::process::Command::new(shell);
    for (k, v) in env.into_map() {
        cmd.env(k, v);
    }
    crate::interactive::inherit_auth_token(&mut cmd);
    let status = cmd
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .await
        .context("failed to spawn shell")?;
    Ok(status.code().unwrap_or(0))
}

/// Allocation variables for a shell on the submit host. A shell on a node
/// gets these from its agent instead.
fn allocation_shell_env(job_id: u32, job: &JobInfo) -> SpurEnv {
    let mut env = SpurEnv::new();
    env.set_with_slurm_twin("SPUR_JOB_ID", job_id);
    env.set_with_slurm_twin("SPUR_JOBID", job_id);
    env.set_with_slurm_twin("SPUR_JOB_USER", &job.user);
    env.set_with_slurm_twin("SPUR_JOB_NAME", &job.name);
    env.set_with_slurm_twin("SPUR_JOB_PARTITION", &job.partition);
    env.set_with_slurm_twin("SPUR_JOB_ACCOUNT", &job.account);
    env.set_with_slurm_twin("SPUR_JOB_QOS", &job.qos);
    env.set_with_slurm_twin("SPUR_NODELIST", &job.nodelist);
    env.set_with_slurm_twin("SPUR_JOB_NODELIST", &job.nodelist);
    env.set_with_slurm_twin("SPUR_NNODES", job.num_nodes);
    env.set_with_slurm_twin("SPUR_JOB_NUM_NODES", job.num_nodes);
    env.set_with_slurm_twin("SPUR_NTASKS", job.num_tasks);
    env.set_with_slurm_twin("SPUR_NPROCS", job.num_tasks);
    env.set_with_slurm_twin("SPUR_CPUS_PER_TASK", job.cpus_per_task);
    env.set_with_slurm_twin("SPUR_SUBMIT_DIR", &job.work_dir);
    if let Some(cpus_per_gpu) = job.cpus_per_gpu {
        env.set_with_slurm_twin("SPUR_CPUS_PER_GPU", cpus_per_gpu);
    }
    if let Some(mb) = job.mem_per_tres.strip_prefix("gpu:") {
        env.set_with_slurm_twin("SPUR_MEM_PER_GPU", mb);
    }
    env
}

/// Where and in what environment the allocation was requested. Its processes
/// on the nodes, an interactive-step shell included, start from these, as
/// under Slurm's default `--export=ALL`.
struct Submission {
    work_dir: String,
    environment: HashMap<String, String>,
}

fn build_salloc_job_spec(
    args: &SallocArgs,
    nodelist: Option<String>,
    submit_line: &str,
    submission: Submission,
) -> Result<JobSpec> {
    let gres = args.gres.clone();
    let gpus = crate::sbatch::parse_gpu_flag(args.gpus.as_deref())?;
    let gpus_per_node = crate::sbatch::parse_gpu_flag(args.gpus_per_node.as_deref())?;
    let gpus_per_task = crate::sbatch::parse_gpu_flag(args.gpus_per_task.as_deref())?;
    crate::sbatch::validate_gpu_flags(
        gpus.is_some(),
        gpus_per_node.is_some(),
        gpus_per_task.is_some(),
        &gres,
    )?;
    require_gpus(
        args.cpus_per_gpu.is_some(),
        args.mem_per_gpu.is_some(),
        requests_gpus(
            gpus.is_some(),
            gpus_per_node.is_some(),
            gpus_per_task.is_some(),
            &gres,
        ),
    )?;

    let time_limit =
        spur_core::config::parse_time_minutes(&args.time).map(|mins| prost_types::Duration {
            seconds: mins as i64 * 60,
            nanos: 0,
        });

    let memory_mb = args
        .mem
        .as_ref()
        .map(|m| crate::sbatch::parse_memory_mb(m))
        .transpose()?
        .unwrap_or(0);
    let memory_per_gpu_mb = args
        .mem_per_gpu
        .as_ref()
        .map(|m| crate::sbatch::parse_memory_mb(m))
        .transpose()?
        .unwrap_or(0);

    Ok(JobSpec {
        name: args
            .job_name
            .clone()
            .unwrap_or_else(|| "interactive".into()),
        partition: args.partition.clone().unwrap_or_default(),
        account: args.account.clone().unwrap_or_default(),
        qos: args.qos.clone().unwrap_or_default(),
        user: crate::interactive::current_user()?,
        uid: nix::unistd::getuid().as_raw(),
        gid: nix::unistd::getgid().as_raw(),
        num_nodes: args.nodes,
        num_tasks: crate::sbatch::effective_ntasks(args.ntasks, None, args.nodes),
        cpus_per_task: args.cpus_per_task.unwrap_or(0),
        memory_per_node_mb: memory_mb,
        cpus_per_gpu: args.cpus_per_gpu,
        memory_per_gpu_mb,
        gres,
        gpus,
        gpus_per_node,
        gpus_per_task,
        script: "#!/bin/bash\nsleep infinity\n".into(),
        time_limit,
        exclusive: args.exclusive,
        constraint: args.constraint.clone().unwrap_or_default(),
        submit_line: submit_line.to_string(),
        nodelist: nodelist.unwrap_or_default(),
        exclude: args.exclude.clone().unwrap_or_default(),
        reservation: args.reservation.clone().unwrap_or_default(),
        container_auth_socket: args.container_auth_socket,
        interactive: true,
        client_keepalive: true,
        work_dir: submission.work_dir,
        environment: submission.environment,
        share_with: crate::sbatch::share_with_entries(args.share_with.as_deref())?,
        ..Default::default()
    })
}

const CPUS_PER_GPU_ENV: &[&str] = &["SALLOC_CPUS_PER_GPU"];
const MEM_PER_GPU_ENV: &[&str] = &["SALLOC_MEM_PER_GPU"];
const MEM_ENV: &[&str] = &["SPUR_MEM_PER_NODE", "SALLOC_MEM_PER_NODE"];

// salloc has no `--mem-per-cpu` flag, so its own messages name only its two
// real members instead of reusing sbatch/srun's three-flag `MEM_CONFLICT`.
const MEM_CONFLICT: &str = "--mem and --mem-per-gpu are mutually exclusive";
const MEM_ENV_CONFLICT: &str = "SALLOC_MEM_PER_NODE and SALLOC_MEM_PER_GPU are mutually exclusive";

/// Resolve `SallocArgs` from argv, then apply `SALLOC_*` env defaults and the
/// per-GPU/memory option-source conflict rule. Mirrors `resolve_sbatch_args`,
/// minus directive merging (salloc has no directive source).
pub(crate) fn resolve_salloc_args(cli_args: &[String]) -> clap::error::Result<SallocArgs> {
    let matches = SallocArgs::command().try_get_matches_from(cli_args)?;
    let mut args = SallocArgs::from_arg_matches(&matches)?;
    resolve_salloc_env(&matches, &mut args)?;
    resolve_option_conflicts(&matches, &mut args)
        .map_err(|e| clap_err(clap::error::ErrorKind::ArgumentConflict, e))?;
    Ok(args)
}

/// Wrap an anyhow error from a Slurm option-source/conflict check as a clap
/// error so it exits through the same path as `resolve_salloc_args`'s other
/// failures (usage error to stderr, exit code 2).
fn clap_err(kind: clap::error::ErrorKind, e: anyhow::Error) -> clap::Error {
    clap::Error::raw(kind, format!("{e}\n"))
}

/// Applies `SALLOC_*`/`SPUR_*` env defaults to any flag not set on the
/// command line (see crate AGENTS.md "Environment variable precedence").
fn resolve_salloc_env(matches: &ArgMatches, args: &mut SallocArgs) -> clap::error::Result<()> {
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
    apply_str(matches, "qos", &["SPUR_QOS", "SALLOC_QOS"], &mut args.qos);
    apply_str(matches, "mem", MEM_ENV, &mut args.mem);
    apply_num_opt_nonzero(
        matches,
        "cpus_per_gpu",
        CPUS_PER_GPU_ENV,
        "--cpus-per-gpu",
        &mut args.cpus_per_gpu,
    )
    .map_err(|e| clap_err(clap::error::ErrorKind::ValueValidation, e))?;
    apply_str(
        matches,
        "mem_per_gpu",
        MEM_PER_GPU_ENV,
        &mut args.mem_per_gpu,
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
        "gpus_per_node",
        &["SPUR_GPUS_PER_NODE", "SALLOC_GPUS_PER_NODE"],
        &mut args.gpus_per_node,
    );
    apply_str(
        matches,
        "gpus_per_task",
        &["SPUR_GPUS_PER_TASK", "SALLOC_GPUS_PER_TASK"],
        &mut args.gpus_per_task,
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
    Ok(())
}

/// Resolve the CPU and memory per-GPU conflict groups with Slurm's
/// option-source rule. salloc has no `-c` input env (`src` passes `&[]` for
/// `cpus_per_task`) and no `--mem-per-cpu`, so the memory group has two members.
fn resolve_option_conflicts(matches: &ArgMatches, args: &mut SallocArgs) -> Result<()> {
    let src = |id: &str, env: &[&str], set: bool| option_source(was_cli_set(matches, id), env, set);

    // `cpus_per_task` has no input env var here (only `cpus_per_gpu` does), so
    // an env-vs-env conflict in this group can never fire; `CPU_CONFLICT`
    // covers both arguments.
    let keep = surviving(
        &[
            src("cpus_per_task", &[], args.cpus_per_task.is_some()),
            src(
                "cpus_per_gpu",
                CPUS_PER_GPU_ENV,
                args.cpus_per_gpu.is_some(),
            ),
        ],
        CPU_CONFLICT,
        CPU_CONFLICT,
    )?;
    if !keep[0] {
        args.cpus_per_task = None;
    }
    if !keep[1] {
        args.cpus_per_gpu = None;
    }

    let keep = surviving(
        &[
            src("mem", MEM_ENV, args.mem.is_some()),
            src("mem_per_gpu", MEM_PER_GPU_ENV, args.mem_per_gpu.is_some()),
        ],
        MEM_CONFLICT,
        MEM_ENV_CONFLICT,
    )?;
    if !keep[0] {
        args.mem = None;
    }
    if !keep[1] {
        args.mem_per_gpu = None;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_c_zero_is_rejected_by_clap() {
        let err = SallocArgs::command().try_get_matches_from(["salloc", "-c", "0"]);
        assert!(err.is_err(), "-c 0 must be a clap parse error");
    }

    fn spec_from(args: &SallocArgs, submit_line: &str) -> Result<JobSpec> {
        let submission = Submission {
            work_dir: String::new(),
            environment: HashMap::new(),
        };
        build_salloc_job_spec(args, None, submit_line, submission)
    }

    #[test]
    fn build_salloc_job_spec_carries_the_submission_dir_and_environment() {
        let args = SallocArgs::parse_from(["salloc"]);
        let submission = Submission {
            work_dir: "/home/alice/project".into(),
            environment: [("PATH".to_string(), "/opt/tools/bin".to_string())].into(),
        };
        let spec = build_salloc_job_spec(&args, None, "salloc", submission).expect("build spec");
        assert_eq!(spec.work_dir, "/home/alice/project");
        assert_eq!(spec.environment["PATH"], "/opt/tools/bin");
    }

    #[test]
    fn interactive_step_flags_override_the_configured_default() {
        let interactive = "--interactive-step";
        let local = "--no-interactive-step";
        // (cli, configured, stdin_is_tty, expected)
        let cases = [
            (vec!["salloc"], false, true, false),
            (vec!["salloc"], true, true, true),
            (vec!["salloc"], true, false, false),
            (vec!["salloc", interactive], false, true, true),
            (vec!["salloc", interactive], false, false, true),
            (vec!["salloc", local], true, true, false),
            (vec!["salloc", local, interactive], false, true, true),
            (vec!["salloc", interactive, local], true, true, false),
        ];
        for (cli, configured, stdin_is_tty, expected) in cases {
            let args = SallocArgs::try_parse_from(&cli).expect("parse failed");
            assert_eq!(
                use_interactive_step(&args, configured, stdin_is_tty),
                expected,
                "{cli:?} with configured={configured} stdin_is_tty={stdin_is_tty}"
            );
        }
    }

    #[tokio::test]
    #[serial(env_injection)]
    async fn the_interactive_step_runs_the_shell_on_the_first_node_and_returns_its_exit() {
        let _env = EnvGuard::new();
        let (agent_addr, agent_capture) = crate::mock_agent::spawn().await;
        agent_capture.script_sessions(vec![crate::mock_agent::ScriptedSession::Exit(5)]);
        let (ctrl_addr, ctrl_capture) = crate::mock_controller::spawn().await;
        ctrl_capture.set_create_step_node_addr(agent_addr.to_string());
        let mut client = crate::mock_controller::client(ctrl_addr).await;

        let exit_code = run_shell_on_node(&mut client, 9, "alice", "/bin/zsh").await;

        assert_eq!(exit_code, 5);
        assert_eq!(ctrl_capture.create_step_command(), vec!["/bin/zsh"]);
        assert!(
            ctrl_capture.create_step_node().is_empty(),
            "an untargeted step lands on the allocation's first node"
        );
        assert!(
            !ctrl_capture.create_step_job_shell(),
            "salloc, not the step, owns the allocation's lifetime"
        );
        assert_eq!(
            ctrl_capture.complete_step_calls(),
            vec![(crate::mock_controller::MOCK_STEP_ID, 5)]
        );
    }

    #[tokio::test]
    #[serial(env_injection)]
    async fn an_interactive_step_that_cannot_start_exits_nonzero() {
        let _env = EnvGuard::new();
        let (addr, capture) = crate::mock_controller::spawn().await;
        capture.set_create_step_error(tonic::Code::PermissionDenied);
        let mut client = crate::mock_controller::client(addr).await;

        assert_eq!(
            run_shell_on_node(&mut client, 9, "alice", "/bin/bash").await,
            1
        );
    }

    #[test]
    fn build_salloc_job_spec_records_the_submit_line() {
        let args = SallocArgs::parse_from(["salloc", "-N", "2"]);
        let spec = spec_from(&args, "salloc -N 2").expect("build spec");
        assert_eq!(spec.submit_line, "salloc -N 2");
    }

    #[test]
    fn build_salloc_job_spec_opts_into_inactivity_reaping() {
        let args = SallocArgs::parse_from(["salloc"]);
        let spec = spec_from(&args, "salloc").expect("build spec");
        assert!(spec.client_keepalive);
    }

    #[test]
    fn container_auth_socket_reaches_the_allocation_job_spec() {
        let args = SallocArgs::parse_from(["salloc", "--container-auth-socket"]);
        let spec = spec_from(&args, "salloc").expect("build spec");
        assert!(spec.container_auth_socket);

        let args = SallocArgs::parse_from(["salloc"]);
        let spec = spec_from(&args, "salloc").expect("build spec");
        assert!(!spec.container_auth_socket, "the socket is opt-in");
    }

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
    fn parses_qos_short_and_long() {
        let short = SallocArgs::try_parse_from(["salloc", "-q", "high"]).expect("parse short qos");
        assert_eq!(short.qos.as_deref(), Some("high"));

        let long = SallocArgs::try_parse_from(["salloc", "--qos=normal"]).expect("parse long qos");
        assert_eq!(long.qos.as_deref(), Some("normal"));
    }

    #[test]
    fn build_job_spec_sets_qos() {
        let args = SallocArgs::try_parse_from(["salloc", "--qos", "high"]).expect("parse");
        let spec = spec_from(&args, "salloc --test").expect("build spec");
        assert_eq!(spec.qos, "high");
        assert!(spec.interactive);
    }

    #[test]
    fn build_job_spec_qos_defaults_empty() {
        let args = SallocArgs::try_parse_from(["salloc"]).expect("parse");
        let spec = spec_from(&args, "salloc --test").expect("build spec");
        assert!(spec.qos.is_empty());
    }

    #[test]
    fn ntasks_defaults_to_node_count() {
        let args = SallocArgs::try_parse_from(["salloc", "-N", "4"]).expect("parse failed");
        assert_eq!(args.ntasks, None);
        assert_eq!(
            crate::sbatch::effective_ntasks(args.ntasks, None, args.nodes),
            4
        );
    }

    #[test]
    fn explicit_ntasks_overrides_node_default() {
        let args =
            SallocArgs::try_parse_from(["salloc", "-N", "4", "-n", "2"]).expect("parse failed");
        assert_eq!(
            crate::sbatch::effective_ntasks(args.ntasks, None, args.nodes),
            2
        );
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
        resolve_salloc_env(&matches, &mut args).expect("env resolution failed");
        args
    }

    fn resolve(cli: &[&str]) -> Result<SallocArgs> {
        let cli: Vec<String> = cli.iter().map(|s| s.to_string()).collect();
        resolve_salloc_args(&cli).map_err(anyhow::Error::from)
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
    fn qos_env_default_cli_override_and_spur_alias() {
        let env = EnvGuard::new();

        env.set("SALLOC_QOS", "burst");
        assert_eq!(resolve_from(&["salloc"]).qos.as_deref(), Some("burst"));

        assert_eq!(
            resolve_from(&["salloc", "--qos=normal"]).qos.as_deref(),
            Some("normal")
        );

        env.set("SPUR_QOS", "from-spur");
        assert_eq!(resolve_from(&["salloc"]).qos.as_deref(), Some("from-spur"));
    }

    #[test]
    #[serial(env_injection)]
    fn env_only_qos_reaches_job_spec() {
        let env = EnvGuard::new();
        env.set("SALLOC_QOS", "burst");
        let args = resolve_from(&["salloc"]);
        let spec = spec_from(&args, "salloc --test").expect("build spec");
        assert_eq!(spec.qos, "burst");
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

    /// Drives `main_with_args` far enough to resolve the task count, then stops
    /// at the controller dial. Guards the argument plumbing between parsing and
    /// `connect_channel`, which the parse-only tests above never execute.
    #[tokio::test]
    #[serial(env_injection)]
    async fn main_with_args_resolves_request_then_fails_to_connect() {
        let _env = EnvGuard::new();
        let addr = crate::mock_controller::unreachable_addr().await;
        let err = main_with_args(vec![
            "salloc".into(),
            "-N".into(),
            "4".into(),
            "--controller".into(),
            format!("http://{addr}"),
        ])
        .await
        .expect_err("connect to a closed port must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("failed to connect to spurctld"),
            "expected a controller connect failure, got: {msg}"
        );
    }

    /// An old controller that does not advertise `CAP_PER_GPU_OPTIONS` must
    /// refuse before `SubmitJob`, not drop `--cpus-per-gpu` silently.
    #[tokio::test]
    #[serial(env_injection)]
    async fn per_gpu_options_without_the_capability_refuse_before_submit() {
        let _env = EnvGuard::new();
        let (addr, capture) = crate::mock_controller::spawn().await;
        capture.set_ping_capabilities(vec![]);
        let err = main_with_args(vec![
            "salloc".into(),
            "--controller".into(),
            format!("http://{addr}"),
            "--gpus".into(),
            "1".into(),
            "--cpus-per-gpu".into(),
            "4".into(),
        ])
        .await
        .expect_err("an old controller must refuse --cpus-per-gpu");
        assert!(
            err.to_string().contains("does not support --cpus-per-gpu"),
            "got: {err}"
        );
        assert_eq!(capture.submit_job_calls(), 0);
    }

    // --- --cpus-per-gpu / --mem-per-gpu ---

    #[test]
    fn cpus_per_task_defaults_to_zero_when_unset() {
        let args = SallocArgs::parse_from(["salloc"]);
        let spec = spec_from(&args, "salloc").expect("build spec");
        assert_eq!(spec.cpus_per_task, 0);
    }

    #[test]
    fn cpus_per_gpu_and_mem_per_gpu_set_spec_fields() {
        let args = SallocArgs::try_parse_from([
            "salloc",
            "--gpus-per-node=2",
            "--cpus-per-gpu=8",
            "--mem-per-gpu=4G",
        ])
        .expect("parse failed");
        let spec = spec_from(&args, "salloc --test").expect("build spec");
        assert_eq!(spec.cpus_per_gpu, Some(8));
        assert_eq!(spec.memory_per_gpu_mb, 4096);
        assert_eq!(spec.cpus_per_task, 0);
    }

    #[test]
    fn cpus_per_gpu_and_cpus_per_task_both_cli_conflict() {
        let err = resolve(&["salloc", "--cpus-per-gpu=8", "-c", "2"]).unwrap_err();
        assert!(err.to_string().contains(CPU_CONFLICT), "got: {err}");
    }

    #[test]
    #[serial(env_injection)]
    fn env_cpus_per_gpu_dropped_when_cli_sets_cpus_per_task() {
        let env = EnvGuard::new();
        env.set("SALLOC_CPUS_PER_GPU", "8");
        let args = resolve(&["salloc", "-c", "2", "--gres=gpu:1"]).expect("resolve failed");
        assert_eq!(args.cpus_per_gpu, None);
        assert_eq!(args.cpus_per_task, Some(2));
    }

    #[test]
    fn mem_and_mem_per_gpu_both_cli_conflict() {
        let err = resolve(&["salloc", "--mem=1G", "--mem-per-gpu=1G"]).unwrap_err();
        assert!(err.to_string().contains(MEM_CONFLICT), "got: {err}");
    }

    #[test]
    #[serial(env_injection)]
    fn env_mem_per_node_and_mem_per_gpu_conflict() {
        let env = EnvGuard::new();
        env.set("SALLOC_MEM_PER_NODE", "4G");
        env.set("SALLOC_MEM_PER_GPU", "1G");
        let err = resolve(&["salloc"]).unwrap_err();
        assert!(err.to_string().contains(MEM_ENV_CONFLICT), "got: {err}");
    }

    #[test]
    fn cpus_per_gpu_without_gpus_rejected() {
        let args = SallocArgs::try_parse_from(["salloc", "--cpus-per-gpu=8"]).expect("parse");
        let err = spec_from(&args, "salloc --test").unwrap_err();
        assert_eq!(err.to_string(), "--cpus-per-gpu requires a GPU request");
    }

    #[test]
    fn mem_per_gpu_without_gpus_rejected() {
        let args = SallocArgs::try_parse_from(["salloc", "--mem-per-gpu=4G"]).expect("parse");
        let err = spec_from(&args, "salloc --test").unwrap_err();
        assert_eq!(err.to_string(), "--mem-per-gpu requires a GPU request");
    }

    #[test]
    fn cpus_per_gpu_zero_is_rejected_on_cli() {
        let err = resolve(&["salloc", "--cpus-per-gpu=0", "--gres=gpu:1"]).unwrap_err();
        assert!(err.to_string().contains("--cpus-per-gpu"), "got: {err}");
    }

    #[test]
    #[serial(env_injection)]
    fn env_cpus_per_gpu_zero_is_rejected() {
        let env = EnvGuard::new();
        env.set("SALLOC_CPUS_PER_GPU", "0");
        let err = resolve(&["salloc", "--gres=gpu:1"]).unwrap_err();
        assert!(
            err.to_string().contains("SALLOC_CPUS_PER_GPU"),
            "got: {err}"
        );
    }

    #[test]
    #[serial(env_injection)]
    fn salloc_cpus_per_gpu_and_mem_per_gpu_names_work() {
        let env = EnvGuard::new();
        env.set("SALLOC_CPUS_PER_GPU", "4");
        env.set("SALLOC_MEM_PER_GPU", "2G");
        let args = resolve(&["salloc", "--gres=gpu:1"]).expect("resolve failed");
        assert_eq!(args.cpus_per_gpu, Some(4));
        assert_eq!(args.mem_per_gpu.as_deref(), Some("2G"));
    }

    #[test]
    #[serial(env_injection)]
    fn spur_cpus_per_gpu_and_mem_per_gpu_twins_not_read_as_input() {
        let env = EnvGuard::new();
        env.set("SPUR_CPUS_PER_GPU", "8");
        env.set("SPUR_MEM_PER_GPU", "1024");
        let args = resolve(&["salloc"]).expect("resolve failed");
        let spec = spec_from(&args, "salloc --test").expect("build spec");
        assert_eq!(spec.cpus_per_gpu, None);
        assert_eq!(spec.memory_per_gpu_mb, 0);
    }

    #[test]
    fn allocation_env_sets_cpus_per_gpu_and_mem_per_gpu_when_present() {
        let job_info = JobInfo {
            job_id: 42,
            cpus_per_gpu: Some(8),
            mem_per_tres: "gpu:4096".into(),
            ..Default::default()
        };
        let map = allocation_shell_env(job_info.job_id, &job_info).into_map();
        assert_eq!(map.get("SPUR_CPUS_PER_GPU").map(String::as_str), Some("8"));
        assert_eq!(map.get("SLURM_CPUS_PER_GPU").map(String::as_str), Some("8"));
        assert_eq!(
            map.get("SPUR_MEM_PER_GPU").map(String::as_str),
            Some("4096")
        );
        assert_eq!(
            map.get("SLURM_MEM_PER_GPU").map(String::as_str),
            Some("4096")
        );
    }

    #[test]
    fn allocation_env_omits_cpus_and_mem_per_gpu_when_absent() {
        let job_info = JobInfo::default();
        let map = allocation_shell_env(job_info.job_id, &job_info).into_map();
        assert!(!map.contains_key("SPUR_CPUS_PER_GPU"));
        assert!(!map.contains_key("SLURM_CPUS_PER_GPU"));
        assert!(!map.contains_key("SPUR_MEM_PER_GPU"));
        assert!(!map.contains_key("SLURM_MEM_PER_GPU"));
    }
}
