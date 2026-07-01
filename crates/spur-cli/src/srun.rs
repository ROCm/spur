// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use clap::Parser;
use spur_core::config::HooksConfig;
use spur_proto::proto::slurm_agent_client::SlurmAgentClient;
use spur_proto::proto::slurm_controller_client::SlurmControllerClient;
use spur_proto::proto::{
    CancelJobRequest, CreateJobStepRequest, GetJobRequest, GetNodeRequest, JobSpec, JobState,
    StreamJobOutputRequest, SubmitJobRequest,
};
use std::collections::HashMap;
use std::io::Write;

/// Run a parallel job (interactive or allocation-based).
#[derive(Parser, Debug)]
#[command(name = "srun", about = "Run a parallel job")]
pub struct SrunArgs {
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
    #[arg(short = 't', long)]
    pub time: Option<String>,

    /// GRES
    #[arg(long)]
    pub gres: Vec<String>,

    /// Licenses (e.g., "fluent:5", "matlab:1")
    #[arg(short = 'L', long)]
    pub licenses: Vec<String>,

    /// GPUs
    #[arg(short = 'G', long)]
    pub gpus: Option<String>,

    /// Working directory
    #[arg(short = 'D', long)]
    pub chdir: Option<String>,

    /// CPU binding (none, cores, threads, sockets, ldoms, rank, map_cpu, mask_cpu)
    #[arg(long)]
    pub cpu_bind: Option<String>,

    /// GPU binding (closest, map_gpu, mask_gpu, none)
    #[arg(long)]
    pub gpu_bind: Option<String>,

    /// Required node features (e.g., "mi300x,nvlink")
    #[arg(short = 'C', long)]
    pub constraint: Option<String>,

    /// Node list
    #[arg(short = 'w', long)]
    pub nodelist: Option<String>,

    /// Exclude nodes
    #[arg(short = 'x', long)]
    pub exclude: Option<String>,

    /// Target a named reservation
    #[arg(long)]
    pub reservation: Option<String>,

    /// MPI type (none, pmix, pmi2)
    #[arg(long, default_value = "none")]
    pub mpi: String,

    /// Job step label output
    #[arg(short = 'l', long)]
    pub label: bool,

    // Container
    /// Container image (OCI ref or squashfs path)
    #[arg(long)]
    pub container_image: Option<String>,

    /// Container bind mounts ("/src:/dst:ro")
    #[arg(long)]
    pub container_mounts: Vec<String>,

    /// Working directory inside the container
    #[arg(long)]
    pub container_workdir: Option<String>,

    /// Mount user home directory in container
    #[arg(long)]
    pub container_mount_home: bool,

    /// Set environment variable inside container (KEY=VAL)
    #[arg(long)]
    pub container_env: Vec<String>,

    /// Remap user to root inside container
    #[arg(long)]
    pub container_remap_root: bool,

    /// Prolog script to run locally before step dispatch
    #[arg(long)]
    pub prolog: Option<String>,

    /// Epilog script to run locally after step completion
    #[arg(long)]
    pub epilog: Option<String>,

    /// Controller address
    #[arg(
        long,
        env = "SPUR_CONTROLLER_ADDR",
        default_value = "http://localhost:6817"
    )]
    pub controller: String,

    /// Command and arguments
    #[arg(trailing_var_arg = true)]
    pub command: Vec<String>,
}

pub async fn main() -> Result<()> {
    main_with_args(std::env::args().collect()).await
}

pub async fn main_with_args(args: Vec<String>) -> Result<()> {
    let args = SrunArgs::try_parse_from(&args)?;

    if args.command.is_empty() {
        eprintln!("srun: no command specified");
        std::process::exit(1);
    }

    // Resolve hooks: CLI flags override config file
    let mut hooks = load_hooks_config();
    if let Some(ref prolog) = args.prolog {
        hooks.srun_prolog = Some(prolog.clone());
    }
    if let Some(ref epilog) = args.epilog {
        hooks.srun_epilog = Some(epilog.clone());
    }

    let work_dir = args
        .chdir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap().to_string_lossy().into());

    // SrunProlog: run locally before dispatching
    if let Some(ref srun_prolog) = hooks.srun_prolog {
        let ctx = srun_hook_context("prolog_srun", &work_dir);
        spur_core::hooks::run_hook(srun_prolog, &ctx)
            .await
            .context("SrunProlog failed — step not dispatched")?;
    }

    // Step mode: if running inside an allocation, create a step instead of a new job
    if let Ok(parent_job_id) = std::env::var("SPUR_JOB_ID") {
        if let Ok(job_id) = parent_job_id.parse::<u32>() {
            return run_as_step(&args, job_id, &hooks, &work_dir).await;
        }
    }

    let name = args.job_name.unwrap_or_else(|| args.command[0].clone());

    // Build a wrapper script from the command
    let cmd_line = args.command.join(" ");
    let script = format!("#!/bin/bash\n{}\n", cmd_line);

    // Build GRES list
    let mut gres = args.gres;
    if let Some(gpus) = &args.gpus {
        gres.push(format!("gpu:{}", gpus));
    }
    // Don't push licenses into gres here — proto_to_job_spec already folds them in.

    let time_limit = args
        .time
        .as_ref()
        .and_then(|t| spur_core::config::parse_time_minutes(t))
        .map(|mins| prost_types::Duration {
            seconds: mins as i64 * 60,
            nanos: 0,
        });

    // Build environment — pass CPU/GPU binding via env vars
    let mut environment: HashMap<String, String> = std::env::vars().collect();
    if let Some(ref cpu_bind) = args.cpu_bind {
        environment.insert("SPUR_CPU_BIND".into(), cpu_bind.clone());
    }
    if let Some(ref gpu_bind) = args.gpu_bind {
        environment.insert("SPUR_GPU_BIND".into(), gpu_bind.clone());
    }
    if args.label {
        environment.insert("SPUR_LABEL".into(), "1".into());
    }

    let memory_mb = args
        .mem
        .as_ref()
        .map(|m| parse_memory_mb(m))
        .transpose()?
        .unwrap_or(0);

    // Submit as a batch job
    let mut client = SlurmControllerClient::connect(args.controller.clone())
        .await
        .context("failed to connect to spurctld")?;

    let job_spec = JobSpec {
        name,
        partition: args.partition.unwrap_or_default(),
        account: args.account.unwrap_or_default(),
        user: whoami::username().unwrap_or_else(|_| "unknown".into()),
        uid: nix::unistd::getuid().as_raw(),
        gid: nix::unistd::getgid().as_raw(),
        num_nodes: args.nodes,
        num_tasks: args.ntasks,
        cpus_per_task: args.cpus_per_task,
        memory_per_node_mb: memory_mb,
        gres,
        script,
        work_dir: work_dir.clone(),
        environment,
        time_limit,
        constraint: args.constraint.unwrap_or_default(),
        nodelist: args.nodelist.unwrap_or_default(),
        exclude: args.exclude.unwrap_or_default(),
        reservation: args.reservation.unwrap_or_default(),
        mpi: args.mpi,
        container_image: args.container_image.unwrap_or_default(),
        container_mounts: args.container_mounts,
        container_workdir: args.container_workdir.unwrap_or_default(),
        container_mount_home: args.container_mount_home,
        container_env: args
            .container_env
            .iter()
            .filter_map(|s| {
                s.split_once('=')
                    .map(|(k, v)| (k.to_string(), v.to_string()))
            })
            .collect(),
        container_remap_root: args.container_remap_root,
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
    eprintln!("srun: job {} submitted, waiting for completion...", job_id);

    // Set up Ctrl+C handler to cancel the job on interrupt
    let cancel_client = client.clone();
    let cancel_user = user.clone();
    tokio::spawn(async move {
        let mut cancel_client = cancel_client;
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\nsrun: cancelling job {}...", job_id);
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

    // Wait for the job to start running
    let mut poll_interval = tokio::time::interval(tokio::time::Duration::from_secs(1));
    #[allow(unused_assignments)]
    let mut nodelist = String::new();
    let mut warned_unknown_state = false;

    loop {
        poll_interval.tick().await;

        match client.get_job(GetJobRequest { job_id }).await {
            Ok(resp) => {
                let job = resp.into_inner();
                match JobState::try_from(job.state) {
                    Ok(JobState::JobRunning) => {
                        nodelist = job.nodelist.clone();
                        if !nodelist.is_empty() {
                            eprintln!("srun: job {} running on {}", job_id, nodelist);
                        }
                        break;
                    }
                    Ok(
                        state @ (JobState::JobCompleted
                        | JobState::JobFailed
                        | JobState::JobCancelled
                        | JobState::JobTimeout
                        | JobState::JobNodeFail
                        | JobState::JobDeadline),
                    ) => {
                        handle_terminal_state(
                            state,
                            job_id,
                            job.exit_code,
                            &work_dir,
                            &hooks,
                            false,
                        )
                        .await;
                    }
                    Ok(_) => {}
                    Err(_) if !warned_unknown_state => {
                        warned_unknown_state = true;
                        eprintln!(
                            "srun: warning: job {} has unrecognized state {} \
                             (controller may be newer than client)",
                            job_id, job.state
                        );
                    }
                    Err(_) => {}
                }
            }
            Err(e) => {
                eprintln!("srun: warning: failed to get job status: {}", e.message());
            }
        }
    }

    // Stream output if possible (best-effort), then poll for terminal state.
    let output_streamed = try_stream_output(&mut client, &nodelist, job_id).await;
    poll_for_completion(&mut client, job_id, &work_dir, &hooks, output_streamed).await;

    Ok(())
}

/// Try to stream live output from the agent.
/// Returns true if streaming connected and delivered output.
async fn try_stream_output(
    controller: &mut SlurmControllerClient<tonic::transport::Channel>,
    nodelist: &str,
    job_id: u32,
) -> bool {
    let first_node = nodelist.split(',').next().unwrap_or(nodelist).trim();
    if first_node.is_empty() {
        return false;
    }

    if controller
        .get_node(GetNodeRequest {
            name: first_node.to_string(),
        })
        .await
        .is_err()
    {
        return false;
    }

    let agent_addr = format!("http://{}:6818", first_node);

    let mut agent = match SlurmAgentClient::connect(agent_addr).await {
        Ok(c) => c,
        Err(_) => return false,
    };

    let mut stream = match agent
        .stream_job_output(StreamJobOutputRequest {
            job_id,
            stream: "stdout".into(),
        })
        .await
    {
        Ok(resp) => resp.into_inner(),
        Err(_) => return false,
    };

    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    loop {
        match stream.message().await {
            Ok(Some(chunk)) => {
                if chunk.eof {
                    return true;
                }
                let _ = handle.write_all(&chunk.data);
                let _ = handle.flush();
            }
            Ok(None) => return true,
            Err(_) => return false,
        }
    }
}

/// Poll-based fallback for agents that don't support streaming.
async fn poll_for_completion(
    client: &mut SlurmControllerClient<tonic::transport::Channel>,
    job_id: u32,
    work_dir: &str,
    hooks: &HooksConfig,
    output_streamed: bool,
) {
    let mut poll_interval = tokio::time::interval(tokio::time::Duration::from_secs(1));
    let mut warned_unknown_state = false;
    loop {
        poll_interval.tick().await;
        match client.get_job(GetJobRequest { job_id }).await {
            Ok(resp) => {
                let job = resp.into_inner();
                match JobState::try_from(job.state) {
                    Ok(
                        state @ (JobState::JobCompleted
                        | JobState::JobFailed
                        | JobState::JobCancelled
                        | JobState::JobTimeout
                        | JobState::JobNodeFail
                        | JobState::JobDeadline),
                    ) => {
                        handle_terminal_state(
                            state,
                            job_id,
                            job.exit_code,
                            work_dir,
                            hooks,
                            output_streamed,
                        )
                        .await;
                    }
                    Ok(_) => {}
                    Err(_) if !warned_unknown_state => {
                        warned_unknown_state = true;
                        eprintln!(
                            "srun: warning: job {} has unrecognized state {} \
                             (controller may be newer than client)",
                            job_id, job.state
                        );
                    }
                    Err(_) => {}
                }
            }
            Err(e) => {
                eprintln!("srun: warning: failed to get job status: {}", e.message());
            }
        }
    }
}

async fn handle_terminal_state(
    state: JobState,
    job_id: u32,
    exit_code: i32,
    work_dir: &str,
    hooks: &HooksConfig,
    output_streamed: bool,
) -> ! {
    // SrunEpilog: run locally after job reaches terminal state
    if let Some(ref srun_epilog) = hooks.srun_epilog {
        let ctx = srun_hook_context("epilog_srun", work_dir);
        if let Err(e) = spur_core::hooks::run_hook(srun_epilog, &ctx).await {
            eprintln!("srun: warning: SrunEpilog failed: {}", e);
        }
    }

    match state {
        JobState::JobCompleted => {
            if !output_streamed {
                print_job_output(work_dir, job_id).await;
            }
            std::process::exit(exit_code);
        }
        JobState::JobFailed => {
            if !output_streamed {
                print_job_output(work_dir, job_id).await;
            }
            eprintln!("srun: job {} failed with exit code {}", job_id, exit_code);
            std::process::exit(exit_code.max(1));
        }
        JobState::JobCancelled => {
            eprintln!("srun: job {} cancelled", job_id);
            std::process::exit(1);
        }
        JobState::JobTimeout => {
            eprintln!("srun: job {} timed out", job_id);
            std::process::exit(1);
        }
        JobState::JobNodeFail => {
            eprintln!("srun: job {} failed (node failure)", job_id);
            std::process::exit(1);
        }
        JobState::JobDeadline => {
            eprintln!("srun: job {} hit its --deadline", job_id);
            std::process::exit(1);
        }
        _ => {
            eprintln!("srun: job {} ended with state {:?}", job_id, state);
            std::process::exit(1);
        }
    }
}

/// Print job output file to stdout (best-effort).
async fn print_job_output(work_dir: &str, job_id: u32) {
    let path = format!("{}/spur-{}.out", work_dir, job_id);
    if let Ok(content) = tokio::fs::read_to_string(&path).await {
        print!("{}", content);
    }
}

/// When srun runs inside an allocation (SPUR_JOB_ID is set, e.g. inside an
/// `salloc` interactive shell or sbatch script on the submit host), it
/// dispatches the command to one of the allocation's nodes via the
/// controller's RunStep RPC. The controller picks an allocated node and
/// forwards to that agent's RunCommand RPC.
///
/// Closes #146 — previously this ran the command locally, so `srun hostname`
/// inside `salloc` printed the controller's hostname instead of the
/// allocated compute node's.
async fn run_as_step(
    args: &SrunArgs,
    job_id: u32,
    hooks: &HooksConfig,
    work_dir: &str,
) -> Result<()> {
    use spur_proto::proto::RunStepRequest;

    let mut client = SlurmControllerClient::connect(args.controller.clone())
        .await
        .context("failed to connect to spurctld")?;

    // Create a step on the controller for tracking; capture the assigned
    // step_id so the completion (and thus DerivedExitCode) records against it.
    let step_id = client
        .create_job_step(CreateJobStepRequest {
            job_id,
            command: args.command.clone(),
            num_tasks: args.ntasks,
            cpus_per_task: args.cpus_per_task,
        })
        .await
        .context("failed to create job step")?
        .into_inner()
        .step_id;

    let env: std::collections::HashMap<String, String> = std::env::vars().collect();

    let resp = client
        .run_step(RunStepRequest {
            job_id,
            command: args.command.clone(),
            uid: nix::unistd::geteuid().as_raw(),
            gid: nix::unistd::getegid().as_raw(),
            work_dir: work_dir.to_string(),
            environment: env,
            step_id,
        })
        .await
        .context("RunStep dispatch failed")?
        .into_inner();

    if !resp.node.is_empty() {
        eprintln!("srun: dispatched to node {}", resp.node);
    }
    if !resp.stdout.is_empty() {
        print!("{}", resp.stdout);
    }
    if !resp.stderr.is_empty() {
        eprint!("{}", resp.stderr);
    }

    // SrunEpilog: run locally after step completes (failure logged only)
    if let Some(ref srun_epilog) = hooks.srun_epilog {
        let ctx = srun_hook_context("epilog_srun", work_dir);
        if let Err(e) = spur_core::hooks::run_hook(srun_epilog, &ctx).await {
            eprintln!("srun: warning: SrunEpilog failed: {}", e);
        }
    }

    std::process::exit(resp.exit_code);
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

fn load_hooks_config() -> HooksConfig {
    let path_str = std::env::var("SPUR_CONF").unwrap_or_else(|_| "/etc/spur/spur.conf".to_string());
    let path = std::path::Path::new(&path_str);
    match spur_core::config::SlurmConfig::load_from_file(path) {
        Ok(config) => config.hooks,
        Err(_) => HooksConfig::default(),
    }
}

fn srun_hook_context(script_context: &str, work_dir: &str) -> spur_core::hooks::HookContext {
    spur_core::hooks::HookContext {
        job_id: std::env::var("SPUR_JOB_ID")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        work_dir: work_dir.to_string(),
        uid: nix::unistd::getuid().as_raw(),
        gid: nix::unistd::getgid().as_raw(),
        partition: String::new(),
        nodelist: String::new(),
        script_context: script_context.into(),
        gpu_devices: Vec::new(),
        cpus: 1,
        memory_mb: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nodelist_and_exclude_short() {
        let args = SrunArgs::try_parse_from([
            "srun",
            "-w",
            "node001,node002",
            "-x",
            "node003",
            "hostname",
        ])
        .expect("parse failed");
        assert_eq!(args.nodelist.as_deref(), Some("node001,node002"));
        assert_eq!(args.exclude.as_deref(), Some("node003"));
    }

    #[test]
    fn parses_nodelist_and_exclude_long() {
        let args = SrunArgs::try_parse_from([
            "srun",
            "--nodelist",
            "node001",
            "--exclude",
            "node002",
            "hostname",
        ])
        .expect("parse failed");
        assert_eq!(args.nodelist.as_deref(), Some("node001"));
        assert_eq!(args.exclude.as_deref(), Some("node002"));
    }
}
