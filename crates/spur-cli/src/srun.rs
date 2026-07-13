// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::env_defaults::{apply_csv, apply_flag, apply_num, apply_str, apply_string};
use anyhow::{Context, Result};
use clap::{ArgMatches, CommandFactory, FromArgMatches, Parser};
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
    // Slurm semantics: comma-lists expand to multiple GRES; a repeated --gres
    // replaces rather than accumulates (last wins).
    #[arg(long, value_delimiter = ',', overrides_with = "gres")]
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

    /// Stdout file path (supports %j for job ID)
    #[arg(short = 'o', long)]
    pub output: Option<String>,

    /// Stderr file path (supports %j for job ID)
    #[arg(short = 'e', long)]
    pub error: Option<String>,

    /// Stdin file path
    #[arg(short = 'i', long)]
    pub input: Option<String>,

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
    let matches = SrunArgs::command().try_get_matches_from(&args)?;
    let mut args = SrunArgs::from_arg_matches(&matches)?;
    resolve_srun_env(&matches, &mut args)?;

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

    let io = resolve_io_paths(&args);

    let name = args.job_name.unwrap_or_else(|| args.command[0].clone());

    let script = build_command_script(&args.command)?;

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
    let channel = spur_client::connect_channel(&args.controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = SlurmControllerClient::new(channel);

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
        stdout_path: io.stdout.clone(),
        stderr_path: io.stderr.clone(),
        stdin_path: io.stdin.clone(),
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
                            &io.stdout,
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

    // Stream output to terminal only when the user didn't ask for file output.
    let output_streamed = if io.stdout.is_empty() {
        try_stream_output(&mut client, &nodelist, job_id).await
    } else {
        false
    };
    poll_for_completion(
        &mut client,
        job_id,
        &work_dir,
        &io.stdout,
        &hooks,
        output_streamed,
    )
    .await;

    Ok(())
}

/// Apply environment-variable defaults to any flag not set on the command
/// line. srun mirrors real Slurm: most flags read `SLURM_*` (inherited from an
/// allocation's output vars), a few read genuine `SRUN_*`, and each also
/// accepts its `SPUR_*` native twin. CLI always overrides env.
fn resolve_srun_env(matches: &ArgMatches, args: &mut SrunArgs) -> Result<()> {
    apply_str(
        matches,
        "job_name",
        &["SPUR_JOB_NAME", "SLURM_JOB_NAME"],
        &mut args.job_name,
    );
    apply_str(
        matches,
        "partition",
        &[
            "SPUR_PARTITION",
            "SPUR_JOB_PARTITION",
            "SLURM_PARTITION",
            "SLURM_JOB_PARTITION",
        ],
        &mut args.partition,
    );
    apply_str(
        matches,
        "account",
        &[
            "SPUR_ACCOUNT",
            "SPUR_JOB_ACCOUNT",
            "SLURM_ACCOUNT",
            "SLURM_JOB_ACCOUNT",
        ],
        &mut args.account,
    );
    apply_num(
        matches,
        "nodes",
        &[
            "SPUR_NNODES",
            "SPUR_JOB_NUM_NODES",
            "SLURM_NNODES",
            "SLURM_JOB_NUM_NODES",
        ],
        &mut args.nodes,
    )?;
    apply_num(
        matches,
        "ntasks",
        &["SPUR_NTASKS", "SPUR_NPROCS", "SLURM_NTASKS", "SLURM_NPROCS"],
        &mut args.ntasks,
    )?;
    apply_num(
        matches,
        "cpus_per_task",
        &["SPUR_CPUS_PER_TASK", "SLURM_CPUS_PER_TASK"],
        &mut args.cpus_per_task,
    )?;
    apply_str(
        matches,
        "mem",
        &["SPUR_MEM_PER_NODE", "SLURM_MEM_PER_NODE"],
        &mut args.mem,
    );
    apply_str(
        matches,
        "time",
        &["SPUR_TIMELIMIT", "SLURM_TIMELIMIT"],
        &mut args.time,
    );
    apply_csv(
        matches,
        "gres",
        &["SPUR_GRES", "SLURM_GRES"],
        &mut args.gres,
    );
    apply_str(
        matches,
        "gpus",
        &["SPUR_GPUS", "SLURM_GPUS"],
        &mut args.gpus,
    );
    apply_str(
        matches,
        "chdir",
        &[
            "SPUR_REMOTE_CWD",
            "SPUR_WORKING_DIR",
            "SLURM_REMOTE_CWD",
            "SLURM_WORKING_DIR",
        ],
        &mut args.chdir,
    );
    apply_str(
        matches,
        "cpu_bind",
        &["SPUR_CPU_BIND", "SLURM_CPU_BIND"],
        &mut args.cpu_bind,
    );
    apply_str(
        matches,
        "gpu_bind",
        &["SPUR_GPU_BIND", "SLURM_GPU_BIND"],
        &mut args.gpu_bind,
    );
    apply_str(
        matches,
        "constraint",
        &["SPUR_CONSTRAINT", "SLURM_CONSTRAINT"],
        &mut args.constraint,
    );
    apply_str(
        matches,
        "reservation",
        &["SPUR_RESERVATION", "SLURM_RESERVATION"],
        &mut args.reservation,
    );
    apply_string(
        matches,
        "mpi",
        &["SPUR_MPI_TYPE", "SLURM_MPI_TYPE"],
        &mut args.mpi,
    );
    apply_flag(
        matches,
        "label",
        &["SPUR_LABELIO", "SLURM_LABELIO"],
        &mut args.label,
    );
    apply_str(
        matches,
        "output",
        &["SPUR_OUTPUT", "SRUN_OUTPUT"],
        &mut args.output,
    );
    apply_str(
        matches,
        "error",
        &["SPUR_ERROR", "SRUN_ERROR"],
        &mut args.error,
    );
    apply_str(
        matches,
        "input",
        &["SPUR_INPUT", "SRUN_INPUT"],
        &mut args.input,
    );
    apply_str(
        matches,
        "container_image",
        &["SPUR_CONTAINER", "SRUN_CONTAINER"],
        &mut args.container_image,
    );
    apply_str(
        matches,
        "prolog",
        &["SPUR_PROLOG", "SLURM_PROLOG"],
        &mut args.prolog,
    );
    apply_str(
        matches,
        "epilog",
        &["SPUR_EPILOG", "SLURM_EPILOG"],
        &mut args.epilog,
    );

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
    stdout_path: &str,
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
                            stdout_path,
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
    stdout_path: &str,
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

    let should_print = !output_streamed && stdout_path.is_empty();

    match state {
        JobState::JobCompleted => {
            if should_print {
                print_job_output(work_dir, stdout_path, job_id).await;
            }
            std::process::exit(exit_code);
        }
        JobState::JobFailed => {
            if should_print {
                print_job_output(work_dir, stdout_path, job_id).await;
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
async fn print_job_output(work_dir: &str, stdout_path: &str, job_id: u32) {
    let path = resolve_output_path_client(stdout_path, job_id, work_dir);
    if let Ok(content) = tokio::fs::read_to_string(&path).await {
        print!("{}", content);
    }
}

/// Client-side output path resolution, mirroring the agent's logic.
fn resolve_output_path_client(pattern: &str, job_id: u32, work_dir: &str) -> String {
    let resolved = if pattern.is_empty() {
        format!("spur-{}.out", job_id)
    } else {
        pattern
            .replace("%j", &job_id.to_string())
            .replace("%J", &job_id.to_string())
    };

    if std::path::Path::new(&resolved).is_absolute() {
        resolved
    } else {
        std::path::PathBuf::from(work_dir)
            .join(resolved)
            .to_string_lossy()
            .into()
    }
}

struct ResolvedIoPaths {
    stdout: String,
    stderr: String,
    stdin: String,
}

/// Wrap the command argv in a bash script for batch submission.
///
/// Each argv element is shell-escaped so metacharacters (spaces, quotes, `;`,
/// `>`, `$`, globs) stay literal arguments instead of being reinterpreted by
/// the wrapper shell. This matches Slurm's semantics, where `srun` execs the
/// argv directly with no shell: `srun touch "my file"` creates one file, and
/// `srun bash -c 'a; b'` passes the whole script as a single `-c` argument.
fn build_command_script(command: &[String]) -> Result<String> {
    let cmd_line = shlex::try_join(command.iter().map(String::as_str))
        .context("command contains a NUL byte and cannot be run")?;
    Ok(format!("#!/bin/bash\n{}\n", cmd_line))
}

/// Resolve I/O paths from CLI args. When `-o` is set but `-e` is not,
/// stderr follows stdout (Slurm default behavior).
fn resolve_io_paths(args: &SrunArgs) -> ResolvedIoPaths {
    let stdout = args.output.clone().unwrap_or_default();
    let stderr = args.error.clone().unwrap_or_else(|| {
        if stdout.is_empty() {
            String::new()
        } else {
            stdout.clone()
        }
    });
    let stdin = args.input.clone().unwrap_or_default();
    ResolvedIoPaths {
        stdout,
        stderr,
        stdin,
    }
}

/// Write step output to a file, used by `run_as_step` for both stdout and stderr.
async fn write_step_file(content: &str, pattern: &str, job_id: u32, work_dir: &str, append: bool) {
    let resolved = resolve_output_path_client(pattern, job_id, work_dir);
    if let Some(parent) = std::path::Path::new(&resolved).parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    let result = if append {
        use tokio::io::AsyncWriteExt;
        match tokio::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&resolved)
            .await
        {
            Ok(mut f) => f.write_all(content.as_bytes()).await,
            Err(e) => Err(e),
        }
    } else {
        tokio::fs::write(&resolved, content).await
    };
    if let Err(e) = result {
        eprintln!("srun: warning: failed to write to {}: {}", resolved, e);
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

    let channel = spur_client::connect_channel(&args.controller)
        .await
        .context("failed to connect to spurctld")?;
    let mut client = SlurmControllerClient::new(channel);

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

    if args.input.is_some() {
        eprintln!("srun: warning: --input is not supported in step mode, ignoring");
    }

    let io = resolve_io_paths(args);

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

    let stderr_appends = !io.stderr.is_empty() && io.stderr == io.stdout;

    if !resp.stdout.is_empty() {
        if io.stdout.is_empty() {
            print!("{}", resp.stdout);
        } else {
            write_step_file(&resp.stdout, &io.stdout, job_id, work_dir, false).await;
        }
    }
    if !resp.stderr.is_empty() {
        if io.stderr.is_empty() {
            eprint!("{}", resp.stderr);
        } else {
            write_step_file(&resp.stderr, &io.stderr, job_id, work_dir, stderr_appends).await;
        }
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

    #[test]
    fn parses_output_error_input_short() {
        let args = SrunArgs::try_parse_from([
            "srun",
            "-o",
            "/tmp/out.txt",
            "-e",
            "/tmp/err.txt",
            "-i",
            "/tmp/in.txt",
            "hostname",
        ])
        .expect("parse failed");
        assert_eq!(args.output.as_deref(), Some("/tmp/out.txt"));
        assert_eq!(args.error.as_deref(), Some("/tmp/err.txt"));
        assert_eq!(args.input.as_deref(), Some("/tmp/in.txt"));
    }

    #[test]
    fn parses_output_error_input_long() {
        let args = SrunArgs::try_parse_from([
            "srun",
            "--output",
            "job-%j.out",
            "--error",
            "job-%j.err",
            "--input",
            "/dev/null",
            "hostname",
        ])
        .expect("parse failed");
        assert_eq!(args.output.as_deref(), Some("job-%j.out"));
        assert_eq!(args.error.as_deref(), Some("job-%j.err"));
        assert_eq!(args.input.as_deref(), Some("/dev/null"));
    }

    #[test]
    fn output_error_input_default_to_none() {
        let args = SrunArgs::try_parse_from(["srun", "hostname"]).expect("parse failed");
        assert!(args.output.is_none());
        assert!(args.error.is_none());
        assert!(args.input.is_none());
    }

    #[test]
    fn resolve_output_path_client_defaults() {
        assert_eq!(
            resolve_output_path_client("", 42, "/work"),
            "/work/spur-42.out"
        );
    }

    #[test]
    fn resolve_output_path_client_pattern() {
        assert_eq!(
            resolve_output_path_client("results-%j.log", 99, "/home/user"),
            "/home/user/results-99.log"
        );
    }

    #[test]
    fn resolve_output_path_client_absolute() {
        assert_eq!(
            resolve_output_path_client("/shared/output-%j.txt", 7, "/work"),
            "/shared/output-7.txt"
        );
    }

    #[test]
    fn resolve_io_paths_stderr_follows_stdout() {
        let args =
            SrunArgs::try_parse_from(["srun", "-o", "/tmp/out.txt", "hostname"]).expect("parse");
        let io = resolve_io_paths(&args);
        assert_eq!(io.stdout, "/tmp/out.txt");
        assert_eq!(io.stderr, "/tmp/out.txt");
        assert!(io.stdin.is_empty());
    }

    #[test]
    fn resolve_io_paths_explicit_stderr_overrides() {
        let args = SrunArgs::try_parse_from([
            "srun",
            "-o",
            "/tmp/out.txt",
            "-e",
            "/tmp/err.txt",
            "hostname",
        ])
        .expect("parse");
        let io = resolve_io_paths(&args);
        assert_eq!(io.stdout, "/tmp/out.txt");
        assert_eq!(io.stderr, "/tmp/err.txt");
    }

    #[test]
    fn resolve_io_paths_no_flags() {
        let args = SrunArgs::try_parse_from(["srun", "hostname"]).expect("parse");
        let io = resolve_io_paths(&args);
        assert!(io.stdout.is_empty());
        assert!(io.stderr.is_empty());
        assert!(io.stdin.is_empty());
    }

    /// The generated script must reconstruct the exact argv after one pass of
    /// shell parsing — i.e. metacharacters stay data, matching Slurm's direct
    /// exec of the argv (no top-level shell interpretation).
    fn assert_script_round_trips(argv: &[&str]) {
        let command: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let script = build_command_script(&command).expect("build script");
        let body = script
            .strip_prefix("#!/bin/bash\n")
            .expect("shebang")
            .trim_end();
        let reparsed = shlex::split(body).expect("generated script must be shell-parseable");
        assert_eq!(reparsed, command, "argv did not round-trip: {body:?}");
    }

    #[test]
    fn build_command_script_simple_argv() {
        assert_eq!(
            build_command_script(&["hostname".to_string()]).unwrap(),
            "#!/bin/bash\nhostname\n"
        );
    }

    #[test]
    fn build_command_script_preserves_spaces_in_arg() {
        // `srun touch "my file.txt"` must stay a single argument, not two.
        assert_script_round_trips(&["touch", "my file.txt"]);
    }

    #[test]
    fn build_command_script_keeps_shell_metacharacters_literal() {
        // Redirections, pipes, globs, and var refs are literal args, not shell
        // syntax — Slurm would pass them verbatim to the exec'd program.
        assert_script_round_trips(&["mytool", "a>b", "x|y", "*", "$HOME"]);
    }

    #[test]
    fn build_command_script_nested_bash_c_stays_one_arg() {
        // `srun bash -c 'echo A; echo B >&2'`: the `;` and `>&2` must remain
        // inside the single -c argument, not leak to the wrapper shell.
        assert_script_round_trips(&["bash", "-c", "echo A; echo B >&2"]);
    }

    #[test]
    fn build_command_script_preserves_quotes_in_arg() {
        assert_script_round_trips(&["echo", "it's a \"test\""]);
    }

    // These tests mutate process-global env vars, so they run serially and use
    // the shared EnvGuard, which clears every SPUR_/SLURM_/SALLOC_/SRUN_-prefixed
    // var on construction and drop, keeping them independent of the runner's
    // environment (which may inject SLURM_* vars) and of each other.
    use crate::env_defaults::EnvGuard;
    use serial_test::serial;

    /// Run the real matches-parse + env-resolve pipeline used by `main_with_args`.
    fn resolve_from(cli: &[&str]) -> SrunArgs {
        let matches = SrunArgs::command()
            .try_get_matches_from(cli)
            .expect("parse failed");
        let mut args = SrunArgs::from_arg_matches(&matches).expect("from_arg_matches failed");
        resolve_srun_env(&matches, &mut args).expect("resolve failed");
        args
    }

    #[test]
    #[serial(env_injection)]
    fn env_provides_default() {
        let env = EnvGuard::new();
        env.set("SLURM_PARTITION", "gpu");
        let args = resolve_from(&["srun", "hostname"]);
        assert_eq!(args.partition.as_deref(), Some("gpu"));
    }

    #[test]
    #[serial(env_injection)]
    fn cli_overrides_env() {
        let env = EnvGuard::new();
        env.set("SLURM_PARTITION", "gpu");
        let args = resolve_from(&["srun", "--partition=cpu", "hostname"]);
        assert_eq!(args.partition.as_deref(), Some("cpu"));
    }

    #[test]
    #[serial(env_injection)]
    fn spur_native_alias_works() {
        let env = EnvGuard::new();
        env.set("SPUR_PARTITION", "spur-part");
        let args = resolve_from(&["srun", "hostname"]);
        assert_eq!(args.partition.as_deref(), Some("spur-part"));
    }

    #[test]
    #[serial(env_injection)]
    fn spur_alias_precedes_slurm_twin() {
        let env = EnvGuard::new();
        env.set("SPUR_PARTITION", "from-spur");
        env.set("SLURM_PARTITION", "from-slurm");
        let args = resolve_from(&["srun", "hostname"]);
        assert_eq!(args.partition.as_deref(), Some("from-spur"));
    }

    #[test]
    #[serial(env_injection)]
    fn nodes_alias_fallback_and_precedence() {
        let env = EnvGuard::new();
        env.set("SLURM_JOB_NUM_NODES", "3");
        assert_eq!(resolve_from(&["srun", "hostname"]).nodes, 3);

        // SLURM_NNODES is listed before SLURM_JOB_NUM_NODES, so it wins.
        env.set("SLURM_NNODES", "5");
        assert_eq!(resolve_from(&["srun", "hostname"]).nodes, 5);
    }

    #[test]
    #[serial(env_injection)]
    fn ntasks_nprocs_fallback() {
        let env = EnvGuard::new();
        env.set("SLURM_NPROCS", "7");
        assert_eq!(resolve_from(&["srun", "hostname"]).ntasks, 7);
    }

    #[test]
    #[serial(env_injection)]
    fn numeric_env_parsed() {
        let env = EnvGuard::new();
        env.set("SLURM_NNODES", "4");
        assert_eq!(resolve_from(&["srun", "hostname"]).nodes, 4);
    }

    #[test]
    #[serial(env_injection)]
    fn numeric_env_trims_whitespace() {
        let env = EnvGuard::new();
        env.set("SLURM_NTASKS", " 4 ");
        assert_eq!(resolve_from(&["srun", "hostname"]).ntasks, 4);
    }

    #[test]
    #[serial(env_injection)]
    fn invalid_numeric_env_errors() {
        let env = EnvGuard::new();
        env.set("SLURM_NNODES", "abc");
        let matches = SrunArgs::command()
            .try_get_matches_from(["srun", "hostname"])
            .expect("parse failed");
        let mut args = SrunArgs::from_arg_matches(&matches).expect("from_arg_matches failed");
        assert!(resolve_srun_env(&matches, &mut args).is_err());
    }

    #[test]
    #[serial(env_injection)]
    fn genuine_srun_vars() {
        let env = EnvGuard::new();
        env.set("SRUN_OUTPUT", "out-%j.log");
        env.set("SRUN_ERROR", "err-%j.log");
        env.set("SRUN_INPUT", "/dev/null");
        env.set("SRUN_CONTAINER", "img.sqsh");
        let args = resolve_from(&["srun", "hostname"]);
        assert_eq!(args.output.as_deref(), Some("out-%j.log"));
        assert_eq!(args.error.as_deref(), Some("err-%j.log"));
        assert_eq!(args.input.as_deref(), Some("/dev/null"));
        assert_eq!(args.container_image.as_deref(), Some("img.sqsh"));
    }

    #[test]
    #[serial(env_injection)]
    fn label_flag_semantics() {
        let env = EnvGuard::new();

        env.set("SLURM_LABELIO", "yes");
        assert!(resolve_from(&["srun", "hostname"]).label);

        env.set("SLURM_LABELIO", "1");
        assert!(resolve_from(&["srun", "hostname"]).label);

        env.set("SLURM_LABELIO", "0");
        assert!(!resolve_from(&["srun", "hostname"]).label);

        env.set("SLURM_LABELIO", "maybe");
        assert!(!resolve_from(&["srun", "hostname"]).label);
    }

    #[test]
    #[serial(env_injection)]
    fn gres_comma_split() {
        let env = EnvGuard::new();
        env.set("SLURM_GRES", "gpu:1,fpga:2");
        let args = resolve_from(&["srun", "hostname"]);
        assert_eq!(args.gres, vec!["gpu:1", "fpga:2"]);
    }

    #[test]
    #[serial(env_injection)]
    fn gres_env_trims_whitespace_and_drops_empties() {
        let env = EnvGuard::new();
        env.set("SLURM_GRES", "gpu:1, fpga:2 ,");
        let args = resolve_from(&["srun", "hostname"]);
        assert_eq!(args.gres, vec!["gpu:1", "fpga:2"]);
    }

    #[test]
    fn cli_gres_comma_list_splits_into_multiple() {
        let args = SrunArgs::try_parse_from(["srun", "--gres=gpu:1,fpga:2", "hostname"])
            .expect("parse failed");
        assert_eq!(args.gres, vec!["gpu:1", "fpga:2"]);
    }

    #[test]
    fn cli_repeated_gres_last_wins() {
        let args = SrunArgs::try_parse_from(["srun", "--gres=gpu:1", "--gres=fpga:2", "hostname"])
            .expect("parse failed");
        assert_eq!(args.gres, vec!["fpga:2"]);
    }

    #[test]
    #[serial(env_injection)]
    fn defaults_unchanged_without_env() {
        let _env = EnvGuard::new();
        let args = resolve_from(&["srun", "hostname"]);
        assert_eq!(args.nodes, 1);
        assert_eq!(args.ntasks, 1);
        assert_eq!(args.cpus_per_task, 1);
        assert!(args.partition.is_none());
        assert_eq!(args.mpi, "none");
        assert!(!args.label);
    }

    /// srun launched inside an allocation inherits the allocation's task and
    /// CPU counts: salloc/sbatch export SLURM_NTASKS and SLURM_CPUS_PER_TASK,
    /// and srun reads them as defaults (matching Slurm). This is what feeds
    /// `create_job_step` in step mode, so the step sizes to the allocation
    /// rather than the bare `-n 1 -c 1` defaults.
    #[test]
    #[serial(env_injection)]
    fn inherits_allocation_task_and_cpu_counts() {
        let env = EnvGuard::new();
        env.set("SLURM_NTASKS", "4");
        env.set("SLURM_CPUS_PER_TASK", "2");
        env.set("SLURM_JOB_NUM_NODES", "3");
        let args = resolve_from(&["srun", "hostname"]);
        assert_eq!(args.ntasks, 4);
        assert_eq!(args.cpus_per_task, 2);
        assert_eq!(args.nodes, 3);

        // An explicit CLI value still wins over the inherited allocation env.
        let overridden = resolve_from(&["srun", "-n", "1", "hostname"]);
        assert_eq!(overridden.ntasks, 1);
    }

    /// Allocation context is exported under the `*_JOB_*` names
    /// (SLURM_JOB_PARTITION / SLURM_JOB_ACCOUNT), so srun must read those to
    /// inherit partition/account when run inside a Spur allocation.
    #[test]
    #[serial(env_injection)]
    fn inherits_allocation_partition_and_account() {
        let env = EnvGuard::new();
        env.set("SLURM_JOB_PARTITION", "alloc-part");
        env.set("SLURM_JOB_ACCOUNT", "alloc-acct");
        let args = resolve_from(&["srun", "hostname"]);
        assert_eq!(args.partition.as_deref(), Some("alloc-part"));
        assert_eq!(args.account.as_deref(), Some("alloc-acct"));

        // The direct input var takes precedence over the allocation twin.
        env.set("SLURM_PARTITION", "input-part");
        let direct = resolve_from(&["srun", "hostname"]);
        assert_eq!(direct.partition.as_deref(), Some("input-part"));

        // An explicit CLI value still wins over both.
        let overridden = resolve_from(&["srun", "-p", "cli-part", "hostname"]);
        assert_eq!(overridden.partition.as_deref(), Some("cli-part"));
    }
}
