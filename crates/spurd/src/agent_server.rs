// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! gRPC server implementing the SlurmAgent service.
//! Receives job launch/cancel requests from spurctld.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tonic::{Request, Response, Status};
use tracing::{error, info, warn};

use tokio_stream::wrappers::ReceiverStream;

use spur_proto::proto::slurm_agent_server::SlurmAgent;
use spur_proto::proto::*;

use spur_sched::cons_tres::{AllocError, AllocationResult, NodeAllocation};

use spur_spank::{SpankContext, SpankHandle, SpankHook, SpankHost};

use spur_core::config::{CgroupConfig, HooksConfig, MpiConfig};
use spur_core::mpi::{resolve_step_mpi, PmixLaunchPlan, MPI_NONE, MPI_PMIX};
use spur_core::spur_env::SpurEnv;
use spur_core::task_launch::{
    batch_companion_hold_script, batch_script_uses_step_launch, build_multi_task_pmix_wrapper,
    build_multi_task_wrapper, use_multi_task_launch,
};
use spur_devices::DeviceRegistry;

use crate::executor;
use crate::mpi_plugin::{self, MpiPluginHost, PmixLaunchGuard};
use crate::reporter::NodeReporter;

/// Apply GPU-deny sentinels to a job env when no GPUs were allocated.
///
/// Keeps the GPU-job path untouched; only zero-GPU jobs are forced to "no
/// devices" so they cannot inherit the runtime's all-visible default.
fn maybe_deny_gpu_env(env: &mut HashMap<String, String>, allocated_device_ids: &[u32]) {
    if allocated_device_ids.is_empty() {
        spur_core::task_launch::gpu_deny_visibility(env);
    }
}

#[cfg(test)]
mod gpu_deny_tests {
    use std::collections::HashMap;

    #[test]
    fn empty_allocation_denies_gpu_env() {
        let mut env = HashMap::new();
        super::maybe_deny_gpu_env(&mut env, &[]);
        assert_eq!(
            env.get("ROCR_VISIBLE_DEVICES").map(String::as_str),
            Some("-1")
        );
    }

    #[test]
    fn nonempty_allocation_leaves_gpu_env_untouched() {
        let mut env = HashMap::new();
        super::maybe_deny_gpu_env(&mut env, &[0u32, 1]);
        assert!(!env.contains_key("ROCR_VISIBLE_DEVICES"));
    }
}

pub(crate) struct TrackedJob {
    job: executor::RunningJob,
    rootfs_mode: crate::container::RootfsMode,
    stdout_path: String,
    stderr_path: String,
    has_pid_namespace: bool,
    has_user_namespace: bool,
    has_mount_namespace: bool,
    _pty_master: Option<std::os::fd::OwnedFd>,
    work_dir: String,
    uid: u32,
    gid: u32,
    /// Owning username, used to gate exec/attach requests that arrive straight
    /// at the agent without passing through the controller.
    user: String,
    partition: String,
    gpu_devices: Vec<u32>,
    cpus: u32,
    memory_mb: u64,
    nodelist: String,
    mpi: String,
    /// Run epoch; echoed on completion and guards the grace-period SIGKILL.
    run_attempt: u32,
}

struct CompletedJob {
    job_id: u32,
    exit_code: i32,
    signal: i32,
    run_attempt: u32,
    rootfs_mode: crate::container::RootfsMode,
    cgroup: Option<std::path::PathBuf>,
    work_dir: String,
    uid: u32,
    gid: u32,
    partition: String,
    gpu_devices: Vec<u32>,
    cpus: u32,
    memory_mb: u64,
    nodelist: String,
    mpi: String,
}

async fn cleanup_completed_job_mpi(job_id: u32, mpi: &str, mpi_host: &MpiPluginHost) {
    if mpi == MPI_PMIX {
        if let Err(e) = mpi_host.release_pmix_server(job_id) {
            warn!(job_id, error = %e, "PMIx batch ref release failed");
        }
    }
}

/// Enforced per-node budget: the controller's allocation wins, the spec is the
/// fallback (every task on this node, and `--mem-per-cpu` when `--mem` is unset).
fn resolve_cgroup_budget(
    alloc: Option<&ResourceAllocations>,
    spec: &JobSpec,
    tasks_per_node: u32,
) -> (u32, u64) {
    let (alloc_cpus, alloc_mem_mb) = alloc.map(|a| (a.cpus, a.memory_mb)).unwrap_or((0, 0));
    // Saturating: the spec is caller-supplied, and a wrapped product would set
    // limits from a budget bearing no relation to what the job asked for.
    let cpus = if alloc_cpus > 0 {
        alloc_cpus
    } else {
        tasks_per_node
            .max(1)
            .saturating_mul(spec.cpus_per_task.max(1))
    };
    let memory_mb = if alloc_mem_mb > 0 {
        alloc_mem_mb
    } else if spec.memory_per_node_mb > 0 {
        spec.memory_per_node_mb
    } else {
        spec.memory_per_cpu_mb.saturating_mul(cpus as u64)
    };
    (cpus, memory_mb)
}

/// Job ids this node holds, shared with the reporter so heartbeats carry them.
pub(crate) type RunningJobs = Arc<Mutex<HashMap<u32, TrackedJob>>>;

/// Build an empty running-jobs map to share between the reporter and the agent.
pub(crate) fn new_running_jobs() -> RunningJobs {
    Arc::new(Mutex::new(HashMap::new()))
}

type PmixLaunchSetup = (
    PmixLaunchGuard,
    PmixLaunchPlan,
    Option<Vec<HashMap<String, String>>>,
);

fn start_pmix_launch(
    mpi_host: Arc<MpiPluginHost>,
    proto_plan: &spur_proto::proto::PmixLaunchPlan,
    pmix_prepared: bool,
    task_offset: u32,
    tasks_on_node: u32,
) -> Result<PmixLaunchSetup, Status> {
    let plan = mpi_plugin::plan_from_proto(proto_plan).map_err(Status::failed_precondition)?;
    let guard = if pmix_prepared {
        match PmixLaunchGuard::join_prepared(mpi_host.clone(), &plan) {
            Ok(guard) => guard,
            Err(err) if err.contains("PMIx was not prepared") => {
                // Step inside a running `--mpi=pmix` batch job: the batch
                // launch already started/joined this namespace via start().
                PmixLaunchGuard::start(mpi_host.clone(), &plan)
                    .map_err(Status::failed_precondition)?
            }
            Err(err) => return Err(Status::failed_precondition(err)),
        }
    } else {
        PmixLaunchGuard::start(mpi_host.clone(), &plan).map_err(Status::failed_precondition)?
    };
    // Per-rank direct fork under Spur's embedded PMIx server.
    let per_local_rank_env = if tasks_on_node > 1 {
        Some(
            mpi_plugin::pmix_setup_fork_env_for_node_tasks(
                &mpi_host,
                &plan,
                task_offset,
                tasks_on_node,
            )
            .map_err(Status::failed_precondition)?,
        )
    } else {
        None
    };
    Ok((guard, plan, per_local_rank_env))
}

/// Monotonic, never-reused id stamped on each `ActiveStep`. The delayed SIGKILL
/// escalation compares epochs (not pids) so it can never signal a recycled pid
/// belonging to a different step that reused the same (job_id, step_id) key.
static STEP_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_step_epoch() -> u64 {
    STEP_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

#[derive(Debug, Default)]
struct ActiveStep {
    cancel_requested: bool,
    pid: Option<u32>,
    epoch: u64,
    /// Spool files the step's stdout/stderr are redirected to, so
    /// `stream_job_output` can tail them live keyed on (job_id, step_id).
    stdout_path: String,
    stderr_path: String,
}

struct ActiveStepGuard {
    steps: Arc<Mutex<HashMap<(u32, u32), ActiveStep>>>,
    key: (u32, u32),
}

impl Drop for ActiveStepGuard {
    fn drop(&mut self) {
        if let Ok(mut steps) = self.steps.try_lock() {
            steps.remove(&self.key);
        }
    }
}

fn cancelled_step_response() -> RunCommandResponse {
    RunCommandResponse {
        exit_code: 128 + nix::sys::signal::Signal::SIGTERM as i32,
        stdout: String::new(),
        stderr: "step cancelled".into(),
    }
}

/// Signal a step's whole workload. The tracked pid may be an intermediate
/// parent (a containerized step runs the workload as a grandchild that is PID 1
/// of its own PID namespace), so a single `kill` on it never reaches the real
/// process. It walks the `/proc` descendant tree first (before the group signal
/// can reparent/reap children and empty `/proc/<pid>/children`), then signals
/// the process group (host steps are group leaders, so this also covers a
/// double-forked descendant that reparented out of the `/proc` tree).
/// Best-effort; warns only when the tracked pid cannot be signalled at all.
fn signal_step_tree(pid: u32, signal: i32) {
    let sig =
        nix::sys::signal::Signal::try_from(signal).unwrap_or(nix::sys::signal::Signal::SIGTERM);
    let target = nix::unistd::Pid::from_raw(pid as i32);
    crate::executor::kill_process_tree(pid as i32, sig);
    if let Err(pg_err) = nix::sys::signal::killpg(target, sig) {
        // Not a group leader (a container fork), or already gone. Fall back to
        // the tracked pid so an unsignalable step still surfaces a warning.
        if let Err(e) = nix::sys::signal::kill(target, Some(sig)) {
            if e != nix::errno::Errno::ESRCH {
                warn!(
                    pid,
                    signal,
                    killpg = %pg_err,
                    kill = %e,
                    "failed to signal step (it may already have exited)"
                );
            }
        }
    }
}

#[cfg(test)]
mod budget_tests {
    use super::resolve_cgroup_budget;
    // Explicit path: `spur_core::resource::ResourceAllocations` is a different
    // type with the same name, and the module glob-imports the proto one.
    use spur_proto::proto::{JobSpec, ResourceAllocations};

    fn spec(cpus_per_task: u32, mem_node: u64, mem_per_cpu: u64) -> JobSpec {
        JobSpec {
            cpus_per_task,
            memory_per_node_mb: mem_node,
            memory_per_cpu_mb: mem_per_cpu,
            ..Default::default()
        }
    }

    #[test]
    fn allocation_wins_when_present() {
        // Spec asks 2 cpus / 1024 MB per task; the node allocation grants 8/32G.
        let alloc = ResourceAllocations {
            cpus: 8,
            memory_mb: 32_768,
            ..Default::default()
        };
        assert_eq!(
            resolve_cgroup_budget(Some(&alloc), &spec(2, 1024, 0), 4),
            (8, 32_768)
        );
    }

    #[test]
    fn spec_fallback_counts_every_task_on_the_node() {
        // The G1 undercount: 4 tasks x 2 cpus is 8 cores, not 2.
        assert_eq!(resolve_cgroup_budget(None, &spec(2, 1024, 0), 4), (8, 1024));
    }

    #[test]
    fn spec_fallback_honors_mem_per_cpu() {
        // --mem-per-cpu=512 with 8 cores on this node.
        assert_eq!(resolve_cgroup_budget(None, &spec(2, 0, 512), 4), (8, 4096));
    }

    #[test]
    fn an_absurd_spec_saturates_instead_of_wrapping() {
        // A spec reaches the agent unvalidated, and a wrapped product would
        // hand limits_for a budget unrelated to the request.
        let (cpus, memory_mb) = resolve_cgroup_budget(None, &spec(100_000, 0, 0), 100_000);
        assert_eq!(cpus, u32::MAX);
        assert_eq!(memory_mb, 0, "no memory request stays unbounded");

        let (cpus, memory_mb) = resolve_cgroup_budget(None, &spec(u32::MAX, 0, u64::MAX), 2);
        assert_eq!(cpus, u32::MAX);
        assert_eq!(memory_mb, u64::MAX);
    }

    #[test]
    fn floors_cpus_at_one() {
        assert_eq!(resolve_cgroup_budget(None, &spec(0, 0, 0), 0), (1, 0));
    }
}

async fn step_cancel_requested(
    steps: &Arc<Mutex<HashMap<(u32, u32), ActiveStep>>>,
    key: (u32, u32),
) -> bool {
    steps
        .lock()
        .await
        .get(&key)
        .is_some_and(|step| step.cancel_requested)
}

/// Spawn a command, register its PID for cancellation, wait for output.
/// Shared by the nsenter (Case 1) and plain-host (Case 3) step dispatch paths.
/// Spawn a tokio step command with stdout/stderr redirected to the per-step
/// spool files (so `stream_job_output` can tail them live), register its pid for
/// cancellation, and wait. Returns `Ok(None)` if the step was cancelled before
/// it could run. Shared by the nsenter (Case 1) and plain-host (Case 3) paths.
async fn run_tokio_step_to_spool(
    mut cmd: tokio::process::Command,
    step_files: crate::executor::StepOutputFiles,
    active_steps: &Arc<Mutex<HashMap<(u32, u32), ActiveStep>>>,
    step_key: (u32, u32),
) -> Result<Option<std::process::ExitStatus>, Status> {
    use std::process::Stdio;
    let mut child = cmd
        .stdout(Stdio::from(step_files.stdout))
        .stderr(Stdio::from(step_files.stderr))
        .spawn()
        .map_err(|e| Status::internal(format!("step command failed to spawn: {e}")))?;

    if let Some(pid) = child.id() {
        let cancel_now = {
            let mut steps = active_steps.lock().await;
            if let Some(step) = steps.get_mut(&step_key) {
                step.pid = Some(pid);
                step.cancel_requested
            } else {
                false
            }
        };
        if cancel_now {
            signal_step_tree(pid, nix::sys::signal::Signal::SIGTERM as i32);
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Ok(None);
        }
    }

    match child.wait().await {
        Ok(status) => Ok(Some(status)),
        Err(e) => Err(Status::internal(format!("step command failed: {e}"))),
    }
}

/// Launch a step command inside a fresh container rootfs, wiring the step's
/// stdout/stderr to the per-step spool files (so `stream_job_output` can tail
/// them live), matching the non-container step path. Used for standalone
/// `srun --container-image` steps where the parent job is not itself
/// containerized. Returns `Ok(None)` if the step was cancelled before it ran.
// A fork/exec helper — each input is a distinct piece of the child's context.
#[allow(clippy::too_many_arguments)]
async fn run_containerized_step(
    container_cfg: crate::container::ContainerConfig,
    rootfs: std::path::PathBuf,
    script_path: String,
    env: HashMap<String, String>,
    step_files: crate::executor::StepOutputFiles,
    active_steps: &Arc<Mutex<HashMap<(u32, u32), ActiveStep>>>,
    step_key: (u32, u32),
    memlock: spur_core::config::MemlockLimit,
) -> Result<(Option<std::process::ExitStatus>, i32), Status> {
    // Returns (exit_status, child_pid). The pid is 0 when the step was
    // cancelled before the fork completed. The caller uses it to set the
    // StepRootfsGuard::pid so the guard kills the child before removing the
    // rootfs (preventing a live-process vs rm-rf race on future drop).
    use std::os::fd::AsRawFd;

    // Sync pipe: child signals readiness (or error) after container_init.
    let (ready_r, ready_w) =
        nix::unistd::pipe().map_err(|e| Status::internal(format!("pipe failed: {e}")))?;
    nix::fcntl::fcntl(
        &ready_r,
        nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
    )
    .ok();

    let ready_r_fd = ready_r.as_raw_fd();
    let ready_w_fd = ready_w.as_raw_fd();
    // Raw fds of the spool files the step's stdout/stderr are redirected to.
    let stdout_fd = step_files.stdout.as_raw_fd();
    let stderr_fd = step_files.stderr.as_raw_fd();

    let rootfs_clone = rootfs.clone();
    // Start from the image's own config.Env (as `docker run` does and as the
    // batch container path does), with the job environment layered on top; read
    // from the rootfs before the fork. Keeps a containerized step consistent with
    // a containerized batch job (e.g. tools on the image's PATH resolve).
    let env_clone = spur_net::oci::container_base_env(&rootfs, env);
    let container_env = container_cfg.container_env.clone();
    let entrypoint = container_cfg.entrypoint.clone();
    let script_path_clone = script_path.clone();

    // Reject NUL bytes before forking: a NUL would otherwise degrade the child's
    // exec to `bash -c ""`, which exits 0 and reports a step that never ran as a
    // success.
    for s in [Some(script_path_clone.as_str()), entrypoint.as_deref()]
        .into_iter()
        .flatten()
    {
        if s.as_bytes().contains(&0) {
            return Err(Status::invalid_argument(
                "container entrypoint or step command contains a NUL byte",
            ));
        }
    }

    match unsafe { nix::unistd::fork().map_err(|e| Status::internal(format!("fork failed: {e}")))? }
    {
        nix::unistd::ForkResult::Child => {
            // === CHILD PROCESS — synchronous only ===
            drop(ready_r);

            unsafe {
                libc::signal(libc::SIGCHLD, libc::SIG_DFL);
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                // Wire stdout/stderr to the per-step spool files.
                libc::dup2(stdout_fd, libc::STDOUT_FILENO);
                libc::dup2(stderr_fd, libc::STDERR_FILENO);
            }

            crate::container::close_inherited_fds(ready_w_fd);

            executor::apply_memlock(memlock);

            let hook_env = match crate::container::container_init(&container_cfg, &rootfs_clone) {
                Ok(env) => env,
                Err(e) => {
                    let msg = format!("E:{e:#}");
                    unsafe {
                        libc::write(ready_w_fd, msg.as_ptr() as *const _, msg.len());
                    }
                    std::process::exit(1);
                }
            };

            unsafe { libc::write(ready_w_fd, b"OK".as_ptr() as *const _, 2) };
            drop(ready_w);

            let mut final_env = env_clone;
            for (k, v) in &container_env {
                final_env.insert(k.clone(), v.clone());
            }
            for (k, v) in hook_env {
                final_env.insert(k, v);
            }

            // Probe for a shell in the image (busybox/alpine has no /bin/bash),
            // matching the batch path in executor.rs.
            let shell = if std::path::Path::new("/bin/bash").exists() {
                "/bin/bash"
            } else {
                "/bin/sh"
            };
            let c_shell = std::ffi::CString::new(shell)
                .unwrap_or_else(|_| std::ffi::CString::new("/bin/sh").unwrap());
            // Honor --container-entrypoint, matching the batch path: run the
            // entrypoint, then the step script, with the same shell. NUL bytes in
            // these strings were rejected before fork, so CString::new cannot fail
            // here in practice.
            let argv: Vec<std::ffi::CString> = if let Some(ref ep) = entrypoint {
                let cmd = format!("{ep} && {shell} {script_path_clone}");
                vec![
                    c_shell.clone(),
                    std::ffi::CString::new("-c").unwrap_or_default(),
                    std::ffi::CString::new(cmd).unwrap_or_default(),
                ]
            } else {
                vec![
                    c_shell.clone(),
                    std::ffi::CString::new(script_path_clone.as_bytes()).unwrap_or_default(),
                ]
            };
            let c_env: Vec<std::ffi::CString> = final_env
                .iter()
                .filter_map(|(k, v)| std::ffi::CString::new(format!("{k}={v}")).ok())
                .collect();
            let argv_refs: Vec<&std::ffi::CStr> = argv.iter().map(|s| s.as_c_str()).collect();
            let env_refs: Vec<&std::ffi::CStr> = c_env.iter().map(|s| s.as_c_str()).collect();
            let _ = nix::unistd::execve(&c_shell, &argv_refs, &env_refs);
            // stderr is the step's spool file here; surface the failure like the
            // batch path rather than exiting silently.
            eprintln!(
                "spur: execve {shell} failed: {}",
                std::io::Error::last_os_error()
            );
            std::process::exit(127);
        }
        nix::unistd::ForkResult::Parent { child: child_pid } => {
            // === PARENT ===
            drop(ready_w);
            // The spool-file fds belong to the child now; close our copies.
            drop(step_files);

            // Read the ready signal from the child.
            let mut buf = [0u8; 256];
            let n = unsafe { libc::read(ready_r_fd, buf.as_mut_ptr() as *mut _, buf.len()) };
            drop(ready_r);
            if n < 2 || &buf[..2] != b"OK" {
                let msg = if n > 0 {
                    String::from_utf8_lossy(&buf[..n.max(0) as usize]).to_string()
                } else {
                    "container init failed (no status)".to_string()
                };
                let _ = unsafe { libc::kill(child_pid.as_raw(), libc::SIGKILL) };
                let _ = nix::sys::wait::waitpid(child_pid, None);
                return Err(Status::internal(format!(
                    "step container init failed: {msg}"
                )));
            }

            // Register PID for cancellation.
            let raw_pid = child_pid.as_raw() as u32;
            let cancel_now = {
                let mut steps = active_steps.lock().await;
                if let Some(step) = steps.get_mut(&step_key) {
                    step.pid = Some(raw_pid);
                    step.cancel_requested
                } else {
                    false
                }
            };
            if cancel_now {
                // SIGKILL the whole subtree: the workload is a grandchild that
                // is PID 1 of its namespace and would ignore SIGTERM from here.
                crate::executor::kill_process_tree(
                    child_pid.as_raw(),
                    nix::sys::signal::Signal::SIGKILL,
                );
                let _ = nix::sys::wait::waitpid(child_pid, None);
                return Ok((None, child_pid.as_raw()));
            }

            let raw_pid = child_pid.as_raw();
            let wait_result =
                tokio::task::spawn_blocking(move || nix::sys::wait::waitpid(child_pid, None)).await;

            use std::os::unix::process::ExitStatusExt;
            let exit_status =
                match wait_result.unwrap_or(Ok(nix::sys::wait::WaitStatus::Exited(child_pid, 1))) {
                    Ok(nix::sys::wait::WaitStatus::Exited(_, code)) => {
                        std::process::ExitStatus::from_raw(code << 8)
                    }
                    Ok(nix::sys::wait::WaitStatus::Signaled(_, sig, _)) => {
                        std::process::ExitStatus::from_raw(sig as i32)
                    }
                    _ => std::process::ExitStatus::from_raw(1 << 8),
                };

            Ok((Some(exit_status), raw_pid))
        }
    }
}

pub struct AgentService {
    pub reporter: Arc<NodeReporter>,
    /// In-memory only: starts empty on every spurd start/restart, regardless
    /// of whether the controller still reports a job Running from before.
    running: RunningJobs,
    allocation: Arc<Mutex<NodeAllocation>>,
    spank: Arc<Option<SpankHost>>,
    mpi_host: Arc<MpiPluginHost>,
    hooks: Arc<HooksConfig>,
    limits: spur_core::config::JobLimits,
    cgroup: CgroupConfig,
    #[allow(dead_code)]
    device_registry: Arc<Mutex<DeviceRegistry>>,
    /// RPC-driven owner of this node's k0s systemd unit.
    k0s: Arc<crate::cluster::K0sAgent>,
    /// In-flight srun steps keyed by `(job_id, step_id)`.
    active_steps: Arc<Mutex<HashMap<(u32, u32), ActiveStep>>>,
    /// `[auth] allow_root_jobs` — when false (default) this agent refuses to execute as uid 0.
    allow_root_jobs: bool,
    /// Whether spurd runs as root. Stored (not queried per call) so tests can drive the refusal
    /// path through a real RPC on an unprivileged runner.
    spurd_is_root: bool,
}

impl AgentService {
    /// Construct with default k0s settings (pinned version, `/usr/local/bin/k0s`). Test-only; the
    /// binary uses `with_cluster_config` to honor the operator's `[cluster]` settings.
    #[cfg(test)]
    pub fn new(
        reporter: Arc<NodeReporter>,
        hooks: HooksConfig,
        device_registry: Arc<Mutex<DeviceRegistry>>,
        memlock: spur_core::config::MemlockLimit,
    ) -> Self {
        Self::with_cluster_config(
            reporter,
            hooks,
            device_registry,
            &spur_core::config::ClusterConfig::default(),
            spur_core::config::JobLimits { memlock },
            CgroupConfig::default(),
            MpiConfig::default(),
            new_running_jobs(),
            spur_core::config::AuthConfig::default().allow_root_jobs,
        )
        // Deterministic regardless of whether the test runner is root: a root runner would
        // otherwise make every launch test (which uses the default uid 0) hit the refusal.
        .with_root_override(false)
    }

    /// Construct with the `[cluster]` config so this node's K0sAgent honors the operator's k0s
    /// version + install path.
    #[allow(clippy::too_many_arguments)]
    pub fn with_cluster_config(
        reporter: Arc<NodeReporter>,
        hooks: HooksConfig,
        device_registry: Arc<Mutex<DeviceRegistry>>,
        cluster: &spur_core::config::ClusterConfig,
        limits: spur_core::config::JobLimits,
        cgroup: CgroupConfig,
        mpi: MpiConfig,
        running: RunningJobs,
        allow_root_jobs: bool,
    ) -> Self {
        let allocation = NodeAllocation::new(
            hostname::get()
                .map(|h| h.to_string_lossy().to_string())
                .unwrap_or_else(|_| "unknown".into()),
            &reporter.resources,
        );

        // Load SPANK plugins from plugstack.conf if available
        let plugstack_path = std::env::var("SPUR_PLUGSTACK")
            .unwrap_or_else(|_| "/etc/spur/plugstack.conf".to_string());
        let spank = if std::path::Path::new(&plugstack_path).exists() {
            match spur_spank::parse_plugstack(std::path::Path::new(&plugstack_path)) {
                Ok(entries) => {
                    let mut host = SpankHost::new();
                    for entry in &entries {
                        if let Err(e) = host.load_plugin(&entry.path, &entry.args) {
                            if entry.required {
                                warn!(
                                    plugin = %entry.path.display(),
                                    error = %e,
                                    "required SPANK plugin failed to load"
                                );
                            } else {
                                info!(
                                    plugin = %entry.path.display(),
                                    error = %e,
                                    "optional SPANK plugin failed to load, skipping"
                                );
                            }
                        }
                    }
                    if host.plugin_count() > 0 {
                        info!(count = host.plugin_count(), "SPANK plugins loaded");
                        Some(host)
                    } else {
                        None
                    }
                }
                Err(e) => {
                    warn!(
                        path = %plugstack_path,
                        error = %e,
                        "failed to parse plugstack.conf"
                    );
                    None
                }
            }
        } else {
            None
        };

        Self {
            reporter,
            running,
            allocation: Arc::new(Mutex::new(allocation)),
            spank: Arc::new(spank),
            mpi_host: Arc::new(MpiPluginHost::new(mpi)),
            hooks: Arc::new(hooks),
            limits,
            cgroup,
            device_registry,
            k0s: Arc::new(crate::cluster::K0sAgent::from_config(cluster)),
            active_steps: Arc::new(Mutex::new(HashMap::new())),
            allow_root_jobs,
            spurd_is_root: crate::privdrop::spurd_runs_as_root(),
        }
    }

    /// Pretend spurd is (or is not) root, so a test can drive the uid-0 refusal through a real RPC
    /// on an unprivileged runner. Production always uses the value read at construction.
    #[cfg(test)]
    fn with_root_override(mut self, is_root: bool) -> Self {
        self.spurd_is_root = is_root;
        self
    }

    /// Handle to the RPC-driven k0s component owner. spurd `main()` spawns its supervise loop.
    pub fn k0s(&self) -> Arc<crate::cluster::K0sAgent> {
        self.k0s.clone()
    }

    /// Spawn a background task to monitor running jobs and report completions.
    pub fn start_monitor(&self, controller_addr: String) {
        let running = self.running.clone();
        let allocation = self.allocation.clone();
        let spank = self.spank.clone();
        let mpi_host = self.mpi_host.clone();
        let hooks = self.hooks.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(2));
            loop {
                interval.tick().await;
                let mut jobs = running.lock().await;
                let mut completed: Vec<CompletedJob> = Vec::new();

                for (job_id, tracked) in jobs.iter_mut() {
                    match tracked.job.try_wait() {
                        Ok(Some((exit_code, mut signal))) => {
                            // Disambiguate an OOM kill (cgroup memory.events) from
                            // a plain SIGKILL by OR'ing a sentinel into the reported
                            // signal; read before cleanup_cgroup removes the dir.
                            let cgroup = tracked.job.take_cgroup();
                            if let Some(ref cg) = cgroup {
                                if crate::executor::cgroup_oom_killed(cg) {
                                    warn!(job_id, "job OOM-killed (cgroup oom_kill > 0)");
                                    signal |= spur_core::job::OOM_SIGNAL_FLAG;
                                }
                            }
                            info!(job_id, exit_code, signal, "job finished");
                            completed.push(CompletedJob {
                                job_id: *job_id,
                                exit_code,
                                signal,
                                run_attempt: tracked.run_attempt,
                                rootfs_mode: tracked.rootfs_mode.clone(),
                                cgroup,
                                work_dir: tracked.work_dir.clone(),
                                uid: tracked.uid,
                                gid: tracked.gid,
                                partition: tracked.partition.clone(),
                                gpu_devices: tracked.gpu_devices.clone(),
                                cpus: tracked.cpus,
                                memory_mb: tracked.memory_mb,
                                nodelist: tracked.nodelist.clone(),
                                mpi: tracked.mpi.clone(),
                            });
                        }
                        Ok(None) => {}
                        Err(e) => {
                            warn!(job_id, error = %e, "failed to check job status");
                        }
                    }
                }

                for c in &completed {
                    jobs.remove(&c.job_id);
                    crate::container::cleanup_rootfs(
                        &crate::container::job_rootfs_base(c.job_id),
                        &c.rootfs_mode,
                    );
                    crate::executor::cleanup_job_spool(c.job_id);
                    if let Some(ref cgroup) = c.cgroup {
                        crate::executor::cleanup_cgroup(cgroup);
                    }
                    allocation.lock().await.release_job(c.job_id);
                    cleanup_completed_job_mpi(c.job_id, &c.mpi, &mpi_host).await;
                }

                // Self-heal backstop: reclaim allocations with no tracked,
                // non-launching job. `jobs` is held so the live set is a
                // consistent snapshot that can't race a committing launch
                // (commit_job takes the running lock first).
                reconcile_orphaned_allocations(&jobs, &mut *allocation.lock().await);

                // Release lock BEFORE network I/O — holding the lock during
                // report_completion blocks new job launches and can lose
                // completions if the RPC times out.
                drop(jobs);

                let local_hostname = hostname::get()
                    .map(|h| h.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "localhost".into());

                let mut drain_jobs: std::collections::HashSet<u32> =
                    std::collections::HashSet::new();

                // Run epilog hook for completed jobs
                if let Some(ref epilog_script) = hooks.epilog {
                    for c in &completed {
                        let ctx = spur_core::hooks::HookContext {
                            job_id: c.job_id,
                            work_dir: c.work_dir.clone(),
                            uid: c.uid,
                            gid: c.gid,
                            partition: c.partition.clone(),
                            nodelist: c.nodelist.clone(),
                            script_context: "epilog_slurmd".into(),
                            gpu_devices: c.gpu_devices.clone(),
                            cpus: c.cpus,
                            memory_mb: c.memory_mb,
                        };
                        if let Err(e) = spur_core::hooks::run_hook(epilog_script, &ctx).await {
                            error!(
                                job_id = c.job_id,
                                error = %e,
                                "epilog hook failed — requesting node drain"
                            );
                            drain_jobs.insert(c.job_id);
                        }
                    }
                }

                // Invoke SPANK TaskExit and JobEpilog hooks for completed jobs
                if let Some(ref spank_host) = *spank {
                    for c in &completed {
                        let context = SpankContext {
                            job_id: c.job_id,
                            uid: c.uid,
                            gid: c.gid,
                            ..Default::default()
                        };
                        let mut handle = SpankHandle::new(context, HashMap::new());
                        if let Err(e) = spank_host.invoke_hook(SpankHook::TaskExit, &mut handle) {
                            warn!(c.job_id, error = %e, "SPANK TaskExit hook failed");
                        }
                        if let Err(e) = spank_host.invoke_hook(SpankHook::JobEpilog, &mut handle) {
                            warn!(c.job_id, error = %e, "SPANK JobEpilog hook failed");
                        }
                    }
                }

                for c in &completed {
                    let drain = if drain_jobs.contains(&c.job_id) {
                        Some(DrainRequest {
                            reason: "epilog script failed".into(),
                        })
                    } else {
                        None
                    };
                    report_completion(
                        &controller_addr,
                        c.job_id,
                        c.exit_code,
                        c.signal,
                        c.run_attempt,
                        &local_hostname,
                        drain.as_ref(),
                    )
                    .await;
                }
            }
        });
    }
}

struct DrainRequest {
    reason: String,
}

/// Reclaim a launch reservation that never commits within this bound. Sized
/// above a typical image pull + fork so a normal launch is spared; one stalled
/// past this bound is reclaimed.
const LAUNCHING_TTL: std::time::Duration = std::time::Duration::from_secs(600);

/// Reclaim allocations whose job is no longer tracked and is not mid-launch,
/// using the running set as ground truth. Callers hold the `running` lock
/// across building `running` and this call so the live set is a consistent
/// snapshot (see the monitor loop). Returns nothing; logs what it reclaimed.
fn reconcile_orphaned_allocations(
    running: &HashMap<u32, TrackedJob>,
    allocation: &mut NodeAllocation,
) {
    let live: std::collections::HashSet<u32> = running.keys().copied().collect();
    let reclaimed = allocation.reconcile(&live, std::time::Instant::now(), LAUNCHING_TTL);
    if !reclaimed.is_empty() {
        warn!(
            ?reclaimed,
            "reconciled orphaned resource allocations with no tracked job"
        );
    }
}

/// Releases a launch reservation if the handler exits between reserve and
/// commit, including on future cancellation which no error path can catch.
/// Disarmed once the job is committed to the running set.
struct LaunchReservationGuard {
    allocation: Arc<Mutex<NodeAllocation>>,
    job_id: u32,
    armed: bool,
}

impl LaunchReservationGuard {
    fn new(allocation: Arc<Mutex<NodeAllocation>>, job_id: u32) -> Self {
        Self {
            allocation,
            job_id,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for LaunchReservationGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let job_id = self.job_id;
        if let Ok(mut alloc) = self.allocation.try_lock() {
            alloc.release_job(job_id);
        } else if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let allocation = self.allocation.clone();
            handle.spawn(async move {
                allocation.lock().await.release_job(job_id);
            });
        }
    }
}

use spur_proto::controller_rpc_retryable;

const CONTROLLER_RPC_ATTEMPTS: u32 = 3;
const CONTROLLER_RPC_RETRY_GAP: std::time::Duration = std::time::Duration::from_secs(1);

/// A single failed attempt at a controller RPC.
enum ControllerRpcError {
    Connect(tonic::transport::Error),
    Rpc(tonic::Status),
}

impl ControllerRpcError {
    /// A transport failure is always worth another attempt: it says nothing
    /// about the request, only that no controller answered. A server response
    /// is worth one only when its code says so.
    fn retryable(&self) -> bool {
        match self {
            Self::Connect(_) => true,
            Self::Rpc(status) => controller_rpc_retryable(status),
        }
    }
}

impl std::fmt::Display for ControllerRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connect(e) => write!(f, "connect: {e}"),
            Self::Rpc(status) => write!(f, "{status}"),
        }
    }
}

/// Run `attempt` until it succeeds, fails in a way no retry can fix, or spends
/// the attempt budget, returning the last failure. A single transient failure
/// must not lose a job completion or leave a broken node accepting work.
async fn retry_controller_rpc<T, F, Fut>(mut attempt: F) -> Result<T, ControllerRpcError>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Result<T, ControllerRpcError>>,
{
    let mut n = 1;
    loop {
        match attempt(n).await {
            Ok(value) => return Ok(value),
            Err(e) => {
                if !e.retryable() || n == CONTROLLER_RPC_ATTEMPTS {
                    return Err(e);
                }
                n += 1;
                tokio::time::sleep(CONTROLLER_RPC_RETRY_GAP).await;
            }
        }
    }
}

#[cfg(test)]
mod controller_rpc_tests {
    use super::{
        controller_rpc_retryable, retry_controller_rpc, ControllerRpcError,
        CONTROLLER_RPC_ATTEMPTS, CONTROLLER_RPC_RETRY_GAP,
    };
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;
    use tonic::Status;

    #[test]
    fn permanent_errors_are_not_retryable() {
        assert!(!controller_rpc_retryable(&Status::invalid_argument("x")));
        assert!(!controller_rpc_retryable(&Status::not_found("x")));
    }

    #[test]
    fn transient_errors_are_retryable() {
        assert!(controller_rpc_retryable(&Status::unavailable("x")));
        assert!(controller_rpc_retryable(&Status::internal("x")));
    }

    /// Drive the retry loop with one scripted outcome per attempt, reporting how
    /// many attempts it actually made.
    async fn run_script(script: Vec<Result<(), Status>>) -> (Result<(), ControllerRpcError>, u32) {
        let calls = AtomicU32::new(0);
        let result = retry_controller_rpc(|_| {
            let outcome = script[calls.fetch_add(1, Ordering::SeqCst) as usize].clone();
            async move { outcome.map_err(ControllerRpcError::Rpc) }
        })
        .await;
        (result, calls.load(Ordering::SeqCst))
    }

    #[tokio::test(start_paused = true)]
    async fn success_returns_on_the_first_attempt() {
        let start = tokio::time::Instant::now();
        let (result, calls) = run_script(vec![
            Ok(()),
            Err(Status::unavailable("must not be reached")),
            Err(Status::unavailable("must not be reached")),
        ])
        .await;

        assert!(result.is_ok());
        assert_eq!(calls, 1);
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    /// A rejection no retry can fix must not spend the budget: the caller runs in
    /// a spawned task, and sleeping through attempts delays the drain that stops
    /// the node taking more work. The trailing successes make a regression here
    /// surface as a wrong result rather than a short script.
    #[tokio::test(start_paused = true)]
    async fn non_retryable_error_gives_up_without_retrying() {
        let start = tokio::time::Instant::now();
        let (result, calls) = run_script(vec![
            Err(Status::invalid_argument("unknown node")),
            Ok(()),
            Ok(()),
        ])
        .await;

        assert!(matches!(result, Err(ControllerRpcError::Rpc(_))));
        assert_eq!(calls, 1);
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn retryable_errors_are_retried_until_one_succeeds() {
        let start = tokio::time::Instant::now();
        let (result, calls) = run_script(vec![
            Err(Status::unavailable("controller restarting")),
            Err(Status::internal("leader election in progress")),
            Ok(()),
        ])
        .await;

        assert!(result.is_ok());
        assert_eq!(calls, CONTROLLER_RPC_ATTEMPTS);
        assert_eq!(start.elapsed(), 2 * CONTROLLER_RPC_RETRY_GAP);
    }

    #[tokio::test(start_paused = true)]
    async fn retryable_errors_give_up_once_the_budget_is_spent() {
        let start = tokio::time::Instant::now();
        let script = vec![Err(Status::unavailable("no route")); CONTROLLER_RPC_ATTEMPTS as usize];
        let (result, calls) = run_script(script).await;

        assert!(matches!(result, Err(ControllerRpcError::Rpc(_))));
        assert_eq!(calls, CONTROLLER_RPC_ATTEMPTS);
        assert_eq!(
            start.elapsed(),
            (CONTROLLER_RPC_ATTEMPTS - 1) * CONTROLLER_RPC_RETRY_GAP
        );
    }
}

/// Reap an already-killed displaced run. Polls `try_wait` so both executor
/// variants are collected: a `Managed` child via tokio, a `Forked` container's
/// raw pid via `waitpid`. Once a displaced run leaves the `running` map the
/// monitor loop no longer polls it, so without this a killed `Forked` run would
/// linger as a zombie until spurd exits.
async fn reap_killed_job(mut job: executor::RunningJob) {
    loop {
        match job.try_wait() {
            Ok(Some(_)) | Err(_) => break,
            Ok(None) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }
}

/// Build a bash script that execs a command vector without shell interpretation.
fn build_one_shot_command_script(command: &[String]) -> Result<String, Status> {
    let joined = shlex::try_join(command.iter().map(String::as_str))
        .map_err(|e| Status::invalid_argument(format!("command is not shell-safe: {e}")))?;
    Ok(format!("#!/bin/bash\nexec {joined}\n"))
}

/// Build the argument vector passed to `nsenter` (everything after the
/// `nsenter` program name) for entering a running job and executing `command`.
///
/// The namespace is entered as root; privilege is dropped *inside* the target
/// via `setpriv --init-groups` when `priv_drop` is set, which is the only way
/// to initialise supplementary groups after nsenter (nsenter's own
/// --setuid/--setgid skip setgroups). Root jobs pass `priv_drop = None` and run
/// the command directly.
fn build_nsenter_argv(
    entry: &crate::job_entry::JobEntry,
    priv_drop: Option<&crate::privdrop::PrivDrop>,
    command: &[String],
) -> Vec<String> {
    let mut args = entry.nsenter_args();

    // Rootless container (the job has its own user namespace). nsenter's default
    // behaviour on entering a user namespace is to reset credentials — it calls
    // setgroups() to drop supplementary groups — but the kernel forbids
    // setgroups() in an unprivileged user namespace, so that fails with EPERM.
    // A `setpriv --init-groups` drop would hit the same wall. Neither is needed:
    // the job user is already mapped inside the namespace (host uid -> the
    // namespace's root), so entering with --preserve-credentials and no setpriv
    // runs the command as the correct user without ever touching groups.
    //
    // This branch only applies to rootless containers. When spurd is root the
    // job has no user namespace (root containers use pid/mount only), so the
    // path below — setpriv --init-groups, which preserves GPU groups — is
    // unchanged.
    if entry.has_user_namespace {
        args.push("--preserve-credentials".into());
        args.push("--".into());
        args.extend(command.iter().cloned());
        return args;
    }

    args.push("--".into());
    if let Some(pd) = priv_drop {
        args.extend(pd.setpriv_prefix());
    }
    args.extend(command.iter().cloned());
    args
}

/// How to launch `command` for a job: the program to spawn, its arguments, and
/// whether the privilege drop must still be applied in the spawned child.
struct LaunchPlan {
    program: String,
    args: Vec<String>,
    /// True only for the direct-spawn path, where the caller must run
    /// `PrivDrop::apply()` in a `pre_exec` hook. On the nsenter path the drop
    /// happens inside the entered namespace via `setpriv`, so the child hook is
    /// skipped.
    apply_priv_in_child: bool,
}

/// Decide how to enter a job and run `command`, shared by `exec_in_job` and
/// `spawn_pty_in_job`.
///
/// When the job has live namespaces, enter them with `nsenter` and drop
/// privilege inside via `setpriv` (see [`build_nsenter_argv`]); the child hook
/// is not used. Otherwise spawn the command directly and let the caller drop
/// privilege in a `pre_exec` hook.
fn build_launch_plan(
    entry: &crate::job_entry::JobEntry,
    priv_drop: Option<&crate::privdrop::PrivDrop>,
    command: &[String],
) -> LaunchPlan {
    if entry.has_namespaces() && entry.pid > 0 {
        LaunchPlan {
            program: "nsenter".to_string(),
            args: build_nsenter_argv(entry, priv_drop, command),
            apply_priv_in_child: false,
        }
    } else {
        LaunchPlan {
            program: command[0].clone(),
            args: command[1..].to_vec(),
            apply_priv_in_child: true,
        }
    }
}

fn cleanup_step_scripts(dir: &std::path::Path, paths: &[&std::path::Path]) {
    for path in paths {
        let _ = std::fs::remove_file(path);
    }
    let _ = std::fs::remove_dir(dir);
}

struct StepScriptCleanup {
    dir: std::path::PathBuf,
    paths: Vec<std::path::PathBuf>,
}

impl Drop for StepScriptCleanup {
    fn drop(&mut self) {
        let path_refs: Vec<&std::path::Path> =
            self.paths.iter().map(std::path::PathBuf::as_path).collect();
        cleanup_step_scripts(&self.dir, &path_refs);
    }
}

/// Removes a step's container rootfs on drop, so a rootfs is torn down even when
/// the `run_command` future is dropped mid-flight (srun Ctrl-C, client
/// disconnect, controller RPC timeout) between `setup_rootfs` and the normal
/// cleanup — otherwise every such attempt leaks an extracted/mounted rootfs.
/// Removes a step's container rootfs on drop, so a rootfs is torn down even when
/// the `run_command` future is dropped mid-flight (srun Ctrl-C, client
/// disconnect, controller RPC timeout) between `setup_rootfs` and the normal
/// cleanup — otherwise every such attempt leaks an extracted/mounted rootfs.
///
/// `pid` is the container child's pid (set once known). On drop, any live child
/// is killed before the rootfs is removed so `cleanup_rootfs` never races an
/// `rm -rf`/umount against a process still pivoted into the directory.
struct StepRootfsGuard {
    base: String,
    mode: crate::container::RootfsMode,
    pid: Option<i32>,
}

impl Drop for StepRootfsGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid {
            // Kill the container child (SIGKILL — it may be PID 1 in its
            // namespace and ignore SIGTERM) and reap it so the process is
            // fully gone before we unmount/remove the rootfs it lives in.
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
            let _ = nix::sys::wait::waitpid(nix::unistd::Pid::from_raw(pid), None);
        }
        crate::container::cleanup_rootfs(&self.base, &self.mode);
    }
}

/// Build the bash job script for a launch request.
///
/// A non-empty `script` is used verbatim. Otherwise `argv` is a literal
/// argument vector whose elements are shell-escaped, so metacharacters stay
/// data rather than being interpreted by the wrapper shell (a redirect leaking
/// to the outer shell would escape an argv-wrapped sandbox).
///
/// When `script_args` is non-empty and a script body is present, a `set --`
/// line is injected so the script receives positional parameters (`$1`, `$@`).
fn build_job_script(
    script: &str,
    argv: &[String],
    script_args: &[String],
) -> Result<String, Status> {
    if !script.is_empty() {
        return inject_script_args(script, script_args);
    }
    if argv.is_empty() {
        return Err(Status::invalid_argument("no script or argv"));
    }
    let joined = shlex::try_join(argv.iter().map(String::as_str))
        .map_err(|e| Status::invalid_argument(format!("argv is not shell-safe: {e}")))?;
    Ok(format!("#!/bin/bash\n{joined}\n"))
}

/// Inject `set -- <args>` into a script so it receives positional parameters.
/// Placed right after the shebang line (if present), otherwise at the top.
fn inject_script_args(script: &str, args: &[String]) -> Result<String, Status> {
    if args.is_empty() {
        return Ok(script.to_string());
    }
    let escaped = shlex::try_join(args.iter().map(String::as_str))
        .map_err(|e| Status::invalid_argument(format!("script args not shell-safe: {e}")))?;
    let set_line = format!("set -- {escaped}");

    let first_newline = script.find('\n');
    let has_shebang = script.starts_with("#!");

    if has_shebang {
        if let Some(pos) = first_newline {
            let shebang = script[..pos].trim_end_matches('\r');
            let rest = &script[pos + 1..];
            return Ok(format!("{shebang}\n{set_line}\n{rest}"));
        }
        return Ok(format!("{script}\n{set_line}\n"));
    }

    Ok(format!("{set_line}\n{script}"))
}

async fn report_completion(
    controller_addr: &str,
    job_id: u32,
    exit_code: i32,
    signal: i32,
    run_attempt: u32,
    reporting_node: &str,
    drain: Option<&DrainRequest>,
) {
    // Wire `state` is derived from `exit_code` alone (advisory): a signaled job
    // reports Completed/0 because the controller's validator requires
    // state<->exit_code agreement. The controller rederives the true Failed /
    // RaisedSignal outcome from the reported `signal`.
    let state = spur_core::job::JobState::completion_state_for_exit_code(exit_code).to_proto_i32();

    let result = retry_controller_rpc(move |attempt| async move {
        let channel = spur_client::connect_channel(controller_addr)
            .await
            .map_err(|e| {
                warn!(
                    job_id,
                    attempt,
                    error = %e,
                    "failed to connect to controller for completion report"
                );
                ControllerRpcError::Connect(e)
            })?;
        let req = ReportJobStatusRequest {
            job_id,
            state,
            exit_code,
            signal,
            message: format!("exit_code={}", exit_code),
            drain_node: drain.is_some(),
            drain_reason: drain.as_ref().map(|d| d.reason.clone()).unwrap_or_default(),
            reporting_node: reporting_node.to_string(),
            run_attempt,
        };
        spur_proto::controller_client(channel)
            .report_job_status(req)
            .await
            .map_err(|e| {
                warn!(
                    job_id,
                    attempt,
                    error = %e,
                    "ReportJobStatus RPC failed"
                );
                ControllerRpcError::Rpc(e)
            })
    })
    .await;

    match result {
        Ok(_) => info!(
            job_id,
            exit_code,
            controller = %controller_addr,
            "reported completion to controller"
        ),
        Err(e) if e.retryable() => error!(
            job_id,
            exit_code,
            attempts = CONTROLLER_RPC_ATTEMPTS,
            error = %e,
            "gave up reporting completion to controller"
        ),
        Err(e) => error!(
            job_id,
            exit_code,
            error = %e,
            "ReportJobStatus failed with non-retryable error"
        ),
    }
}

fn warn_mpi_mpirun_skipped_affinity(job_id: u32, source: &HashMap<String, String>) {
    use spur_core::task_launch::{mpi_mpirun_skips_cpu_bind, mpi_mpirun_skips_gpu_bind};
    let cpu_bind = mpi_mpirun_skips_cpu_bind(source);
    let gpu_bind = mpi_mpirun_skips_gpu_bind(source);
    if cpu_bind || gpu_bind {
        warn!(
            job_id,
            cpu_bind,
            gpu_bind,
            "multi-rank --mpi=pmix launches via mpirun --bind-to none; Spur CPU/GPU bind env is not applied to MPI ranks"
        );
    }
}

/// Drain this node without reporting a job completion. The controller's dispatch
/// path already owns the job's fate, so reporting an exit code here would race it
/// and could finalize a still-retryable job to Failed, which no requeue recovers.
async fn request_node_drain(controller_addr: &str, node_name: &str, reason: &str, job_id: u32) {
    let result = retry_controller_rpc(move |attempt| async move {
        let channel = spur_client::connect_channel(controller_addr)
            .await
            .map_err(|e| {
                warn!(
                    job_id,
                    node = %node_name,
                    attempt,
                    error = %e,
                    "failed to connect to controller for drain request"
                );
                ControllerRpcError::Connect(e)
            })?;
        let req = DrainNodeRequest {
            name: node_name.to_string(),
            reason: reason.to_string(),
        };
        spur_proto::controller_client(channel)
            .drain_node(req)
            .await
            .map_err(|e| {
                warn!(
                    job_id,
                    node = %node_name,
                    attempt,
                    error = %e,
                    "DrainNode RPC failed"
                );
                ControllerRpcError::Rpc(e)
            })
    })
    .await;

    match result {
        Ok(resp) => warn!(
            job_id,
            node = %node_name,
            state = %resp.into_inner().actual_state,
            reason = %reason,
            "requested node drain after launch failure"
        ),
        Err(e) if e.retryable() => error!(
            job_id,
            node = %node_name,
            attempts = CONTROLLER_RPC_ATTEMPTS,
            error = %e,
            "gave up requesting node drain"
        ),
        Err(e) => error!(
            job_id,
            node = %node_name,
            error = %e,
            "DrainNode failed with non-retryable error"
        ),
    }
}

#[tonic::async_trait]
impl SlurmAgent for AgentService {
    type StreamJobOutputStream = ReceiverStream<Result<StreamJobOutputChunk, Status>>;
    type InteractiveSessionStream = ReceiverStream<Result<InteractiveOutput, Status>>;

    async fn launch_job(
        &self,
        request: Request<LaunchJobRequest>,
    ) -> Result<Response<LaunchJobResponse>, Status> {
        Self::require_controller(&request)?;
        let req = request.into_inner();
        let job_id = req.job_id;
        // A launch names the node the controller scheduled it onto. If it does not name this host it
        // was aimed at the wrong agent — refuse rather than run another node's allocation here.
        if !req.target_node.is_empty() && !self.agent_owns_node(&req.target_node) {
            return Err(Status::failed_precondition(format!(
                "launch targeted node '{}' but this agent serves '{}'",
                req.target_node, self.reporter.hostname
            )));
        }
        let peer_nodes = req.peer_nodes;
        let task_offset = req.task_offset;
        // Per-task array identity is controller-assigned on the launch request,
        // not part of the (user-supplied) job spec.
        let array_job_id = req.array_job_id;
        let array_task_id = req.array_task_id;
        let run_attempt = req.run_attempt;
        let spec = req
            .spec
            .ok_or_else(|| Status::invalid_argument("missing job spec"))?;

        // The uid is part of the (user-supplied) job spec and no RPC authenticates its caller, so
        // refuse root execution here — before anything is spawned — rather than relying on the
        // privilege drop, which treats uid 0 as "nothing to drop".
        if let Err(msg) = crate::privdrop::check_root_execution_allowed(
            spec.uid,
            self.allow_root_jobs,
            self.spurd_is_root,
        ) {
            warn!(job_id, uid = spec.uid, "{msg}");
            return Err(Status::permission_denied(msg));
        }

        info!(
            job_id,
            name = %spec.name,
            task_offset,
            num_peers = peer_nodes.len(),
            "received job launch request"
        );

        let work_dir = if spec.work_dir.is_empty() {
            spur_core::job::DEFAULT_WORK_DIR.to_string()
        } else {
            spec.work_dir.clone()
        };

        let script =
            if batch_script_uses_step_launch(&spec.script) && task_offset > 0 && !req.task_fanout {
                batch_companion_hold_script().to_string()
            } else {
                build_job_script(&spec.script, &spec.argv, &spec.script_args)?
            };

        // Compute tasks_per_node for both single- and multi-node jobs
        let tasks_per_node = if spec.tasks_per_node > 0 {
            spec.tasks_per_node
        } else {
            (spec.num_tasks / spec.num_nodes.max(1)).max(1)
        };
        let node_rank = task_offset / tasks_per_node.max(1);
        let hostname = hostname::get()
            .map(|h| h.to_string_lossy().to_string())
            .unwrap_or_else(|_| "localhost".into());
        let mut senv = SpurEnv::new();
        senv.extend(&spec.environment);

        // Ensure the Spur CLI binaries (srun/sbatch/... symlinks to `spur`) are
        // on the job's PATH so `srun` works inside batch scripts.
        if let Some(bin_dir) = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        {
            let bin_dir = bin_dir.to_string_lossy().to_string();
            let base = spec
                .environment
                .get("PATH")
                .cloned()
                .unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".to_string());
            if !base.split(':').any(|p| p == bin_dir) {
                senv.set("PATH", format!("{}:{}", bin_dir, base));
            }
        }

        // SPUR+SLURM twins
        senv.set_with_slurm_twin("SPUR_JOB_ID", job_id);
        senv.set_with_slurm_twin("SPUR_JOBID", job_id);
        senv.set_with_slurm_twin("SPUR_JOB_NAME", &spec.name);
        senv.set_with_slurm_twin("SPUR_JOB_PARTITION", &spec.partition);
        senv.set_with_slurm_twin("SPUR_JOB_ACCOUNT", &spec.account);
        senv.set_with_slurm_twin("SPUR_JOB_QOS", &spec.qos);
        senv.set_with_slurm_twin("SPUR_SUBMIT_DIR", &work_dir);
        senv.set_with_slurm_twin("SPUR_NNODES", peer_nodes.len());
        senv.set_with_slurm_twin("SPUR_JOB_NUM_NODES", peer_nodes.len());
        senv.set_with_slurm_twin("SPUR_NTASKS", spec.num_tasks);
        senv.set_with_slurm_twin("SPUR_NPROCS", spec.num_tasks);
        senv.set_with_slurm_twin("SPUR_CPUS_PER_TASK", spec.cpus_per_task);
        senv.set_with_slurm_twin("SPUR_TASKS_PER_NODE", tasks_per_node);
        senv.set_with_slurm_twin("SPUR_NODEID", node_rank);
        senv.set_with_slurm_twin("SPUR_NODELIST", &spec.nodelist);
        senv.set_with_slurm_twin("SPUR_JOB_NODELIST", &spec.nodelist);
        senv.set_with_slurm_twin("SPURD_NODENAME", &hostname);
        senv.set_with_slurm_twin(
            "SPUR_CPUS_ON_NODE",
            tasks_per_node.saturating_mul(spec.cpus_per_task.max(1)),
        );

        if array_job_id != 0 {
            senv.set_with_slurm_twin("SPUR_ARRAY_JOB_ID", array_job_id);
            senv.set_with_slurm_twin("SPUR_ARRAY_TASK_ID", array_task_id);
        }

        let pmix_multi_task = spec.mpi == MPI_PMIX
            && use_multi_task_launch(tasks_per_node, req.task_fanout, &spec.mpi, &spec.script);

        // Spur-only vars
        senv.set("SPUR_NODE_RANK", node_rank);
        if pmix_multi_task {
            // Match standalone `srun --mpi=pmix`: batch direct launch is step 0 of
            // the allocation, not a batch-script singleton world.
            let num_nodes = peer_nodes.len().max(1) as u32;
            SpurEnv::apply_step_scope(&mut senv, job_id, 0, spec.num_tasks, node_rank, num_nodes);
            senv.set_with_slurm_twin("SPUR_MPI_TYPE", MPI_PMIX);
            senv.set("SPUR_TASK_OFFSET", task_offset);
        } else if tasks_per_node == 1 {
            SpurEnv::apply_task_rank(&mut senv, task_offset, 0);
        } else {
            senv.set("SPUR_TASK_OFFSET", task_offset);
        }
        if !peer_nodes.is_empty() {
            senv.set("SPUR_PEER_NODES", peer_nodes.join(","));
        }
        if !req.target_node.is_empty() {
            senv.set("SPUR_TARGET_NODE", &req.target_node);
        }
        if !spec.burst_buffer.is_empty() {
            senv.set("SPUR_BURST_BUFFER", &spec.burst_buffer);
        }

        let mut env = senv.into_map();

        // If container image is specified, prepare rootfs and config for
        // the Rust container runtime (fork + container_init + pivot_root).
        let mut container_config: Option<crate::container::ContainerConfig> = None;
        let mut rootfs_path: Option<std::path::PathBuf> = None;

        let (launch_script, rootfs_mode) = if !spec.container_image.is_empty() {
            info!(job_id, image = %spec.container_image, "launching containerized job");

            let mounts: Vec<crate::container::BindMount> = spec
                .container_mounts
                .iter()
                .filter_map(|m| crate::container::parse_mount(m).ok())
                .collect();

            let username = spec.user.clone();
            let uid = spec.uid;
            let gid = spec.gid;
            let home_dir = std::env::var("HOME").unwrap_or_else(|_| format!("/home/{}", username));

            let cfg = crate::container::ContainerConfig {
                image: spec.container_image.clone(),
                mounts,
                workdir: if spec.container_workdir.is_empty() {
                    None
                } else {
                    Some(spec.container_workdir.clone())
                },
                name: if spec.container_name.is_empty() {
                    None
                } else {
                    Some(spec.container_name.clone())
                },
                readonly: spec.container_readonly,
                mount_home: spec.container_mount_home,
                remap_root: spec.container_remap_root,
                gpu_devices: vec![], // overwritten below after GRES allocation
                environment: env.clone(),
                container_env: spec.container_env.clone(),
                entrypoint: if spec.container_entrypoint.is_empty() {
                    None
                } else {
                    Some(spec.container_entrypoint.clone())
                },
                uid,
                gid,
                username: if username.is_empty() {
                    "spur".to_string()
                } else {
                    username
                },
                home_dir,
                device_plan: None, // set after GRES allocation
            };

            let image_path = crate::container::resolve_image(
                &spec.container_image,
                Some(&spec.user),
                Some(spec.uid),
            )
            .map_err(|e| Status::failed_precondition(e.to_string()))?;

            let (rootfs, rootfs_mode) = crate::container::setup_rootfs(
                &image_path,
                &crate::container::job_rootfs_base(job_id),
                cfg.name.as_deref(),
            )
            .map_err(|e| Status::internal(format!("container setup failed: {}", e)))?;

            // Copy user script into rootfs/tmp/ so it's accessible after pivot_root
            let container_script = format!("{}/tmp/spur_job_{}.sh", rootfs.display(), job_id);
            std::fs::write(&container_script, &script).map_err(|e| {
                Status::internal(format!("failed to write container script: {}", e))
            })?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(
                    &container_script,
                    std::fs::Permissions::from_mode(0o755),
                );
            }

            rootfs_path = Some(rootfs);
            container_config = Some(cfg);

            // The launch_script passed to executor is the user's script
            // (used as fallback for non-container path; for container path,
            // the executor reads from rootfs/tmp/ directly).
            (script, rootfs_mode)
        } else {
            (script, crate::container::RootfsMode::Extracted)
        };

        let mut pmix_guard = None;
        let mut pmix_plan: Option<PmixLaunchPlan> = None;
        let mut pmix_per_local_rank_env: Option<Vec<HashMap<String, String>>> = None;
        if spec.mpi == MPI_PMIX && !batch_script_uses_step_launch(&spec.script) {
            let proto = req.pmix_plan.as_ref().ok_or_else(|| {
                Status::failed_precondition("missing PMIx launch plan for --mpi=pmix job")
            })?;
            let (guard, plan, per_local_rank_env) = start_pmix_launch(
                self.mpi_host.clone(),
                proto,
                req.pmix_prepared,
                task_offset,
                tasks_per_node,
            )?;
            pmix_guard = Some(guard);
            pmix_plan = Some(plan);
            pmix_per_local_rank_env = per_local_rank_env;
        }

        // Batch scripts run once per node unless fan-out is requested. Spur fans
        // out when `task_fanout` is set (standalone `srun` routed through the batch
        // path) or when `--mpi=pmix` is set so a direct batch launch spawns one
        // MPI rank per local task without requiring an inner `srun`.
        let launch_script =
            if use_multi_task_launch(tasks_per_node, req.task_fanout, &spec.mpi, &spec.script) {
                // Write the user script to disk first so the wrapper can reference it
                let user_script_path = format!("{}/.spur_user_{}.sh", work_dir, job_id);
                std::fs::write(&user_script_path, &launch_script)
                    .map_err(|e| Status::internal(format!("failed to write user script: {}", e)))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(
                        &user_script_path,
                        std::fs::Permissions::from_mode(0o755),
                    );
                }

                if spec.mpi == MPI_PMIX {
                    warn_mpi_mpirun_skipped_affinity(job_id, &spec.environment);
                    build_multi_task_pmix_wrapper(
                        &user_script_path,
                        tasks_per_node,
                        pmix_per_local_rank_env.as_ref().ok_or_else(|| {
                            Status::internal("missing PMIx per-rank env for multi-task launch")
                        })?,
                        Some(&spec.environment),
                    )
                    .map_err(Status::failed_precondition)?
                } else {
                    build_multi_task_wrapper(&user_script_path, tasks_per_node, None)
                }
            } else {
                launch_script
            };

        let (cpus, memory_mb) =
            resolve_cgroup_budget(req.allocated.as_ref(), &spec, tasks_per_node);

        let (alloc_result, allocated_device_ids) = self
            .allocate_local_resources(job_id, &spec, req.allocated.as_ref(), cpus, memory_mb)
            .await?;

        // Release the reservation on any exit before commit, including a
        // cancelled launch future; disarmed once committed to `running`.
        let mut reservation_guard = LaunchReservationGuard::new(self.allocation.clone(), job_id);

        let injection = {
            let reg = self.device_registry.lock().await;
            reg.build_job_injection_plans("gpu", &allocated_device_ids, spec.uid, spec.gid)
        };
        let (host_device_plan, container_device_plan) = match injection {
            Ok(plans) => plans,
            Err(e) => {
                error!(job_id, error = %e, "device registry resolution failed");
                return Err(Status::failed_precondition(format!(
                    "device resolution failed: {}",
                    e
                )));
            }
        };

        // Wire allocated device IDs and injection plan into container config.
        if let Some(ref mut cfg) = container_config {
            cfg.gpu_devices = allocated_device_ids.clone();
            cfg.device_plan = Some(container_device_plan);
        }

        if let Some(ref prolog) = self.hooks.prolog {
            let ctx = spur_core::hooks::HookContext {
                job_id,
                work_dir: work_dir.clone(),
                uid: spec.uid,
                gid: spec.gid,
                partition: spec.partition.clone(),
                nodelist: spec.nodelist.clone(),
                script_context: "prolog_slurmd".into(),
                gpu_devices: allocated_device_ids.clone(),
                cpus,
                memory_mb,
            };
            if let Err(e) = spur_core::hooks::run_hook(prolog, &ctx).await {
                // No completion report and no self-drain: the controller owns
                // both decisions here, because only it can pair the drain with
                // the hold that stops the job walking the cluster.
                let err_msg = format!("prolog failed: {e:#}");
                error!(job_id, error = %err_msg, "prolog hook failed before launch");
                return Ok(Response::new(LaunchJobResponse {
                    success: false,
                    error: err_msg,
                    stdout_path: String::new(),
                    stderr_path: String::new(),
                    failure_kind: LaunchFailureKind::LaunchFailureProlog as i32,
                }));
            }
        }

        if let Some(plan) = pmix_plan.as_ref() {
            if pmix_per_local_rank_env.is_none() {
                mpi_plugin::apply_pmix_setup_fork_env(&self.mpi_host, plan, task_offset, &mut env)
                    .map_err(Status::failed_precondition)?;
            }
        }

        maybe_deny_gpu_env(&mut env, &allocated_device_ids);

        if let Some(ref mut cfg) = container_config {
            cfg.environment = env.clone();
            // container_env (user `--container-env`) is layered over environment
            // at launch, so a zero-GPU job could re-enable visibility through it.
            maybe_deny_gpu_env(&mut cfg.container_env, &allocated_device_ids);
        }

        let cpu_ids: Vec<u32> = alloc_result.cpu_ids.clone();

        // Guard rather than unwrap: these are always Some when the image is
        // set. An early return here releases the reservation via the guard.
        let container_launch = if !spec.container_image.is_empty() {
            match (container_config.take(), rootfs_path.take()) {
                (Some(config), Some(rootfs)) => {
                    Some(executor::ContainerLaunchConfig { config, rootfs })
                }
                _ => {
                    return Err(Status::internal(
                        "internal error: container config missing after setup",
                    ));
                }
            }
        } else {
            None
        };

        let launch_cfg = executor::JobLaunchConfig {
            job_id,
            script: launch_script,
            work_dir: work_dir.clone(),
            name: spec.name.clone(),
            user: spec.user.clone(),
            node: req.target_node.clone(),
            array_job_id: (array_job_id != 0).then_some(array_job_id),
            array_task_id: (array_job_id != 0).then_some(array_task_id),
            environment: env,
            pmix_multi_task,
            stdout_path: spec.stdout_path.clone(),
            stderr_path: spec.stderr_path.clone(),
            stdin_path: spec.stdin_path.clone(),
            cpus,
            memory_mb,
            gpu_devices: allocated_device_ids,
            cpu_ids,
            open_mode: if spec.open_mode.is_empty() {
                None
            } else {
                Some(spec.open_mode.clone())
            },
            uid: spec.uid,
            gid: spec.gid,
            container: container_launch,
            prolog_script: None,
            partition: spec.partition.clone(),
            nodelist: spec.nodelist.clone(),
            host_device_plan: Some(host_device_plan),
            memlock: self.limits.memlock,
            cgroup: self.cgroup.clone(),
            io_mode: if spec.pty {
                executor::LaunchIo::Pty
            } else {
                executor::LaunchIo::File
            },
        };

        match executor::launch_job(&launch_cfg, (*self.spank).as_ref()).await {
            Ok(mut result) => {
                pmix_guard.as_mut().map(PmixLaunchGuard::disarm);
                let mut jobs = self.running.lock().await;
                // Commit the reservation: the job now has a tracked process, so
                // it is no longer exempt from reconcile. Take the running lock
                // first so a job is never briefly absent from BOTH `running` and
                // `launching` (which would let reconcile reclaim it).
                let committed = self.allocation.lock().await.commit_job(job_id);
                reservation_guard.disarm();

                // reconcile reclaimed the reservation mid-launch (launch exceeded
                // the TTL). Don't track a job with no backing allocation — kill,
                // reap, and clean up its cgroup/rootfs/spool (mirroring the
                // monitor loop's completion teardown, which never runs since the
                // job never enters `running`), then fail the launch.
                if !committed {
                    drop(jobs);
                    warn!(
                        job_id,
                        "reservation reclaimed during launch; aborting to avoid running unbacked"
                    );
                    if let Err(e) = self.mpi_host.stop_pmix_server(job_id) {
                        warn!(job_id, error = %e, "PMIx stop failed after reclaimed reservation");
                    }
                    let _ = result.job.kill_signal(nix::sys::signal::Signal::SIGKILL);
                    let cgroup = result.job.take_cgroup();
                    let running = self.running.clone();
                    tokio::spawn(async move {
                        reap_killed_job(result.job).await;
                        // rootfs/spool paths are derived from job_id, so the
                        // controller re-dispatching the same id to this node
                        // would reuse them. Skip that cleanup if a live run for
                        // job_id reappeared, or this reap would delete its files.
                        // The cgroup handle is this launch's own, so it is always
                        // safe to release.
                        if !running.lock().await.contains_key(&job_id) {
                            crate::container::cleanup_rootfs(
                                &crate::container::job_rootfs_base(job_id),
                                &rootfs_mode,
                            );
                            crate::executor::cleanup_job_spool(job_id);
                        }
                        if let Some(ref cg) = cgroup {
                            crate::executor::cleanup_cgroup(cg);
                        }
                    });
                    return Ok(Response::new(LaunchJobResponse {
                        success: false,
                        error: "reservation reclaimed during launch".into(),
                        stdout_path: String::new(),
                        stderr_path: String::new(),
                        failure_kind: LaunchFailureKind::LaunchFailureUnspecified as i32,
                    }));
                }

                info!(job_id, gpus = ?launch_cfg.gpu_devices, "job launched successfully");
                let is_root = nix::unistd::geteuid().is_root();
                let is_container = launch_cfg.container.is_some();
                // Report the real resolved paths back so the controller can
                // surface where output actually landed (e.g. the /tmp fallback).
                let stdout_path = result.stdout_path.clone();
                let stderr_path = result.stderr_path.clone();
                let displaced = jobs.insert(
                    job_id,
                    TrackedJob {
                        job: result.job,
                        rootfs_mode: rootfs_mode.clone(),
                        stdout_path: result.stdout_path,
                        stderr_path: result.stderr_path,
                        has_pid_namespace: is_root || is_container,
                        has_user_namespace: is_container && !is_root,
                        has_mount_namespace: is_root || is_container,
                        _pty_master: result.pty_master,
                        work_dir: launch_cfg.work_dir,
                        uid: launch_cfg.uid,
                        gid: launch_cfg.gid,
                        user: launch_cfg.user,
                        partition: launch_cfg.partition,
                        gpu_devices: launch_cfg.gpu_devices,
                        cpus: launch_cfg.cpus,
                        memory_mb: launch_cfg.memory_mb,
                        nodelist: launch_cfg.nodelist,
                        mpi: spec.mpi.clone(),
                        run_attempt,
                    },
                );
                drop(jobs);
                // Re-dispatch onto the same node reuses job_id and displaces an
                // older run. If its process ignored SIGTERM and outlived the
                // requeue, kill and reap it here — the monitor loop no longer
                // tracks it, so without this it would leak as an orphan/zombie.
                if let Some(old) = displaced {
                    if old.run_attempt < run_attempt {
                        let _ = old.job.kill_signal(nix::sys::signal::Signal::SIGKILL);
                        tokio::spawn(reap_killed_job(old.job));
                    }
                }
                Ok(Response::new(LaunchJobResponse {
                    success: true,
                    error: String::new(),
                    stdout_path,
                    stderr_path,
                    failure_kind: LaunchFailureKind::LaunchFailureUnspecified as i32,
                }))
            }
            Err(e) => {
                // reservation_guard releases the allocation and PMI on this return.
                let drain_reason = e.drain_reason();
                let failure_kind = match e {
                    executor::LaunchError::PrologFailed(_) => {
                        LaunchFailureKind::LaunchFailureProlog
                    }
                    _ => LaunchFailureKind::LaunchFailureUnspecified,
                };
                let err_msg = e.to_string();
                error!(job_id, error = %err_msg, "failed to launch job");

                if let Some(drain_reason) = drain_reason {
                    let controller = self.reporter.controller_addr.clone();
                    let node_name = self.reporter.hostname.clone();
                    tokio::spawn(async move {
                        request_node_drain(&controller, &node_name, &drain_reason, job_id).await;
                    });
                }

                Ok(Response::new(LaunchJobResponse {
                    success: false,
                    error: err_msg,
                    stdout_path: String::new(),
                    stderr_path: String::new(),
                    failure_kind: failure_kind as i32,
                }))
            }
        }
    }

    async fn prepare_pmix(
        &self,
        request: Request<PreparePmixRequest>,
    ) -> Result<Response<PreparePmixResponse>, Status> {
        let req = request.into_inner();
        let plan = req
            .pmix_plan
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing PMIx launch plan"))
            .and_then(|proto| {
                mpi_plugin::plan_from_proto(proto).map_err(Status::invalid_argument)
            })?;
        match self.mpi_host.prepare_pmix_server(&plan, req.run_attempt) {
            Ok(()) => Ok(Response::new(PreparePmixResponse {
                success: true,
                error: String::new(),
            })),
            Err(err) => Ok(Response::new(PreparePmixResponse {
                success: false,
                error: err,
            })),
        }
    }

    async fn release_pmix(
        &self,
        request: Request<ReleasePmixRequest>,
    ) -> Result<Response<ReleasePmixResponse>, Status> {
        let job_id = request.into_inner().job_id;
        if let Err(err) = self.mpi_host.release_prepared_pmix(job_id) {
            warn!(job_id, error = %err, "PMIx prepare release failed");
        }
        Ok(Response::new(ReleasePmixResponse {}))
    }

    async fn cancel_job(
        &self,
        request: Request<AgentCancelJobRequest>,
    ) -> Result<Response<()>, Status> {
        Self::require_controller(&request)?;
        let req = request.into_inner();
        let job_id = req.job_id;

        if req.signal > 0 {
            self.send_explicit_signal(job_id, req.signal).await;
        } else {
            self.graceful_cancel(job_id).await;
        }

        // The signal paths only act on a running job; release a still-launching
        // reservation so a cancel-during-eviction doesn't strand it until the
        // TTL. Hold the running lock across the release (matching launch_job's
        // commit order) so this can't free a job that just became running.
        let jobs = self.running.lock().await;
        if !jobs.contains_key(&job_id) {
            self.allocation.lock().await.release_job(job_id);
        }
        drop(jobs);

        if let Err(err) = self.mpi_host.release_prepared_pmix(job_id) {
            warn!(job_id, error = %err, "PMIx prepare release on cancel failed");
        }

        Ok(Response::new(()))
    }

    async fn cancel_step(
        &self,
        request: Request<CancelStepRequest>,
    ) -> Result<Response<()>, Status> {
        Self::require_controller(&request)?;
        let req = request.into_inner();
        let step_key = (req.job_id, req.step_id);
        let signal = if req.signal > 0 {
            req.signal
        } else {
            nix::sys::signal::Signal::SIGTERM as i32
        };
        let pid = {
            let mut steps = self.active_steps.lock().await;
            match steps.get_mut(&step_key) {
                Some(step) => {
                    step.cancel_requested = true;
                    step.pid
                }
                None => return Ok(Response::new(())),
            }
        };
        if let Some(pid) = pid {
            signal_step_tree(pid, signal);
        }
        Ok(Response::new(()))
    }

    async fn suspend_job(
        &self,
        request: Request<AgentSuspendJobRequest>,
    ) -> Result<Response<()>, Status> {
        Self::require_controller(&request)?;
        let req = request.into_inner();
        self.suspend_signal(req.job_id, req.resume).await;
        Ok(Response::new(()))
    }

    async fn get_node_resources(
        &self,
        _request: Request<()>,
    ) -> Result<Response<NodeResourcesResponse>, Status> {
        let resources = &self.reporter.resources;
        Ok(Response::new(NodeResourcesResponse {
            total: Some(crate::reporter::resource_to_proto(resources)),
            used: Some(crate::reporter::allocations_to_proto(
                &spur_core::resource::ResourceAllocations::default(),
            )),
        }))
    }

    async fn exec_in_job(
        &self,
        request: Request<ExecInJobRequest>,
    ) -> Result<Response<ExecInJobResponse>, Status> {
        let identity = Self::verified_identity(&request).cloned();
        let req = request.into_inner();

        self.check_job_access(req.job_id, identity.as_ref(), &req.user, "exec into")
            .await?;

        let entry = self.job_entry(req.job_id).await?;

        if req.command.is_empty() {
            return Err(Status::invalid_argument("no command specified"));
        }

        info!(
            job_id = req.job_id,
            pid = entry.pid,
            command = ?req.command,
            "exec into running job"
        );

        // Defense in depth: the uid here comes from the tracked job (validated at launch), not the
        // wire, so this is only reachable for a job that was already running when allow_root_jobs
        // was turned off. Checking anyway keeps the invariant total — spurd never executes as uid 0
        // unless the operator opted in — instead of true only at the wire entry points.
        if let Err(msg) = crate::privdrop::check_root_execution_allowed(
            entry.uid,
            self.allow_root_jobs,
            self.spurd_is_root,
        ) {
            warn!(job_id = req.job_id, uid = entry.uid, "{msg}");
            return Err(Status::permission_denied(msg));
        }

        let priv_drop = crate::privdrop::PrivDrop::resolve_if_needed(entry.uid, entry.gid);

        let plan = build_launch_plan(&entry, priv_drop.as_ref(), &req.command);
        let mut cmd = tokio::process::Command::new(&plan.program);
        cmd.args(&plan.args);
        // Direct-spawn path drops privilege in the child; the nsenter path
        // already does it inside the namespace via setpriv.
        if plan.apply_priv_in_child {
            cmd.current_dir(&entry.work_dir);
            if let Some(pd) = priv_drop {
                unsafe {
                    cmd.pre_exec(move || {
                        pd.apply()
                            .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
                        Ok(())
                    });
                }
            }
        }

        let output = cmd
            .output()
            .await
            .map_err(|e| Status::internal(format!("nsenter failed: {}", e)))?;

        Ok(Response::new(ExecInJobResponse {
            success: output.status.success(),
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        }))
    }

    /// Record a standalone-srun allocation on this node without launching a
    /// batch script.
    async fn register_job_allocation(
        &self,
        request: Request<RegisterJobAllocationRequest>,
    ) -> Result<Response<RegisterJobAllocationResponse>, Status> {
        let req = request.into_inner();
        if req.job_id == 0 {
            return Err(Status::invalid_argument("job_id is required"));
        }

        let allocated = req.allocated.as_ref();
        let mut controller_gpu_ids: Vec<u32> = allocated
            .and_then(|a| a.devices.get("gpu"))
            .map(|d| d.devices.iter().map(|dev| dev.device_id).collect())
            .unwrap_or_default();
        if controller_gpu_ids.is_empty() {
            controller_gpu_ids = req
                .gpu_devices
                .iter()
                .filter_map(|s| s.parse().ok())
                .collect();
        }

        let cpus = allocated.map(|a| a.cpus).unwrap_or(req.cpus).max(1);
        let memory_mb = allocated.map(|a| a.memory_mb).unwrap_or(req.memory_mb);

        // Hold the running lock across the duplicate check, reserve+commit, and
        // insert (running → allocation, as in commit) so the job is never
        // committed-but-absent-from-running, which the reclaim reads as stale.
        let mut jobs = self.running.lock().await;
        if jobs.contains_key(&req.job_id) {
            return Err(Status::already_exists(format!(
                "job {} already registered on this node",
                req.job_id
            )));
        }
        {
            let mut alloc = self.allocation.lock().await;
            alloc
                .allocate_for_job(req.job_id, cpus, memory_mb, &controller_gpu_ids)
                .map_err(|e| match e {
                    AllocError::GpusUnavailable => Status::resource_exhausted(
                        "controller-allocated GPUs unavailable on this node",
                    ),
                    AllocError::DuplicateJob => Status::already_exists(format!(
                        "job {} already registered on this node",
                        req.job_id
                    )),
                })?;
            let _ = alloc.commit_job(req.job_id);
        }

        info!(
            job_id = req.job_id,
            cpus,
            memory_mb,
            gpus = ?controller_gpu_ids,
            "registered srun allocation"
        );

        jobs.insert(
            req.job_id,
            TrackedJob {
                job: executor::RunningJob::AllocationOnly,
                rootfs_mode: crate::container::RootfsMode::Extracted,
                stdout_path: String::new(),
                stderr_path: String::new(),
                has_pid_namespace: false,
                has_user_namespace: false,
                has_mount_namespace: false,
                _pty_master: None,
                work_dir: req.work_dir.clone(),
                uid: req.uid,
                gid: req.gid,
                user: req.user,
                partition: req.partition,
                gpu_devices: controller_gpu_ids,
                cpus,
                memory_mb,
                nodelist: req.nodelist,
                mpi: req.mpi,
                // srun allocation-only jobs use their own cancel lifecycle;
                // epoch 0 leaves the stale-report guard disabled for them.
                run_attempt: 0,
            },
        );
        drop(jobs);

        Ok(Response::new(RegisterJobAllocationResponse {}))
    }

    /// Run a one-shot command on this node, used by srun inside an allocation.
    /// Unlike ExecInJob, this does not require a tracked job process — salloc
    /// allocations don't run anything until srun dispatches a step.
    async fn run_command(
        &self,
        request: Request<RunCommandRequest>,
    ) -> Result<Response<RunCommandResponse>, Status> {
        Self::require_controller(&request)?;
        let req = request.into_inner();
        if req.command.is_empty() {
            return Err(Status::invalid_argument("no command specified"));
        }
        // Steps carry their own uid straight from the wire — gate them exactly like a batch launch.
        if let Err(msg) = crate::privdrop::check_root_execution_allowed(
            req.uid,
            self.allow_root_jobs,
            self.spurd_is_root,
        ) {
            warn!(job_id = req.job_id, uid = req.uid, "{msg}");
            return Err(Status::permission_denied(msg));
        }

        let work_dir = if req.work_dir.is_empty() {
            "/tmp".to_string()
        } else {
            req.work_dir
        };

        let job_id = req.job_id;
        if job_id == 0 {
            return Err(Status::invalid_argument("job_id is required"));
        }

        let num_tasks = req.num_tasks.max(1);
        let step_num_tasks = if req.step_num_tasks > 0 {
            req.step_num_tasks
        } else {
            num_tasks
        };
        let step_id = req.step_id;
        let step_key = (job_id, step_id);
        {
            self.active_steps.lock().await.insert(
                step_key,
                ActiveStep {
                    epoch: next_step_epoch(),
                    ..Default::default()
                },
            );
        }
        let _active_step_guard = ActiveStepGuard {
            steps: self.active_steps.clone(),
            key: step_key,
        };

        // No retry on a miss: a step only reaches a Running job, i.e. one every
        // node already confirmed via LaunchJob (confirm_dispatch_on_nodes) — so a
        // miss is a wrong job/node pairing, not a launch race. The one uncovered
        // case is a spurd restart mid-job, which starts `running` empty.
        let (gpu_devices, partition, cpus, memory_mb, nodelist, job_mpi, job_entry) = {
            let jobs = self.running.lock().await;
            let tracked = jobs.get(&job_id).ok_or_else(|| {
                Status::not_found(format!("job {} not running on this node", job_id))
            })?;
            let nodelist = if tracked.nodelist.is_empty() {
                hostname::get()
                    .map(|h| h.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "localhost".into())
            } else {
                tracked.nodelist.clone()
            };
            let pid = tracked.job.pid().unwrap_or(0);
            let entry = crate::job_entry::JobEntry {
                pid: pid as i32,
                has_pid_namespace: tracked.has_pid_namespace,
                has_user_namespace: tracked.has_user_namespace,
                has_mount_namespace: tracked.has_mount_namespace,
                uid: tracked.uid,
                gid: tracked.gid,
                work_dir: tracked.work_dir.clone(),
            };
            (
                tracked.gpu_devices.clone(),
                tracked.partition.clone(),
                tracked.cpus,
                tracked.memory_mb,
                nodelist,
                tracked.mpi.clone(),
                entry,
            )
        };

        let agent_hostname = self.reporter.hostname.clone();
        let node_names: Vec<&str> = nodelist.split(',').filter(|s| !s.is_empty()).collect();
        let num_nodes = node_names.len().max(1) as u32;
        let node_id = node_names
            .iter()
            .position(|n| *n == agent_hostname)
            .unwrap_or(0) as u32;

        let (mut gpu_env, container_device_plan) = if gpu_devices.is_empty() {
            (HashMap::new(), None)
        } else {
            let (host_plan, container_plan) = self
                .device_registry
                .lock()
                .await
                .build_job_injection_plans("gpu", &gpu_devices, req.uid, req.gid)
                .map_err(|e| {
                    Status::failed_precondition(format!("GPU injection plan failed: {}", e))
                })?;
            (host_plan.env, Some(container_plan))
        };
        maybe_deny_gpu_env(&mut gpu_env, &gpu_devices);

        let mut senv = SpurEnv::new();
        senv.extend(&req.environment);
        senv.set_with_slurm_twin("SPUR_JOB_ID", job_id);
        senv.set_with_slurm_twin("SPUR_JOBID", job_id);
        senv.set_with_slurm_twin("SPUR_JOB_PARTITION", &partition);
        senv.set_with_slurm_twin("SPUR_NODELIST", &nodelist);
        senv.set_with_slurm_twin("SPUR_JOB_NODELIST", &nodelist);
        senv.set_with_slurm_twin("SPUR_CPUS_ON_NODE", cpus);
        senv.extend(&gpu_env);
        let mut bind_env = HashMap::new();
        spur_core::task_launch::apply_gpu_bind_env(&mut bind_env, &req.environment, &gpu_devices);
        senv.extend(&bind_env);
        if let Some(cpu_bind) = spur_core::task_launch::unsupported_cpu_bind(&req.environment) {
            warn!(
                job_id,
                cpu_bind = %cpu_bind,
                "topology CPU bind modes are not applied in srun step mode"
            );
        }
        if let Some(err) =
            spur_core::task_launch::map_cpu_bind_error(&req.environment, step_num_tasks).or_else(
                || spur_core::task_launch::mask_cpu_bind_error(&req.environment, step_num_tasks),
            )
        {
            return Err(Status::invalid_argument(err));
        }
        SpurEnv::apply_step_scope(
            &mut senv,
            job_id,
            step_id,
            step_num_tasks,
            node_id,
            num_nodes,
        );
        if req.label {
            senv.set("SPUR_LABEL", "1");
        }

        let step_mpi_type = resolve_step_mpi(req.mpi.as_str(), job_mpi.as_str());
        if !step_mpi_type.is_empty() && step_mpi_type != MPI_NONE && step_mpi_type != MPI_PMIX {
            return Err(Status::invalid_argument(format!(
                "invalid step mpi type '{step_mpi_type}'"
            )));
        }
        let step_mpi = step_mpi_type == MPI_PMIX;
        if req.pmix_plan.is_some() && !step_mpi {
            return Err(Status::invalid_argument("pmix_plan requires step mpi=pmix"));
        }
        if step_mpi && req.pmix_plan.is_none() {
            return Err(Status::invalid_argument(
                "step mpi=pmix requires a PMIx launch plan",
            ));
        }

        let mut pmix_step_guard = None;
        let mut pmix_plan: Option<PmixLaunchPlan> = None;
        let mut pmix_per_local_rank_env: Option<Vec<HashMap<String, String>>> = None;
        if step_mpi {
            let proto = req
                .pmix_plan
                .as_ref()
                .ok_or_else(|| Status::invalid_argument("missing PMIx launch plan"))?;
            let (guard, plan, per_local_rank_env) = start_pmix_launch(
                self.mpi_host.clone(),
                proto,
                req.pmix_prepared,
                req.task_offset,
                num_tasks,
            )?;
            pmix_step_guard = Some(guard);
            pmix_plan = Some(plan);
            pmix_per_local_rank_env = per_local_rank_env;
        }

        if step_cancel_requested(&self.active_steps, step_key).await {
            return Ok(Response::new(cancelled_step_response()));
        }

        let (program, program_args, step_script_cleanup) = if num_tasks > 1 || req.label {
            let step_dir =
                crate::executor::prepare_step_script_dir(&work_dir, job_id, req.uid, req.gid)
                    .map_err(|e| {
                        Status::internal(format!("failed to create step script dir: {e}"))
                    })?;
            let mut guard = StepScriptCleanup {
                dir: step_dir.clone(),
                paths: Vec::new(),
            };

            let user_script_path = step_dir.join(format!("cmd_{node_id}.sh"));
            let user_script = build_one_shot_command_script(&req.command)?;
            crate::executor::write_job_scratch(&user_script_path, &user_script, req.uid, req.gid)
                .map_err(|e| Status::internal(format!("failed to write step script: {e}")))?;
            guard.paths.push(user_script_path.clone());

            let wrapper_path = step_dir.join(format!("wrapper_{node_id}.sh"));
            let wrapper = if num_tasks > 1 {
                if step_mpi {
                    build_multi_task_pmix_wrapper(
                        user_script_path.to_string_lossy().as_ref(),
                        num_tasks,
                        pmix_per_local_rank_env.as_ref().ok_or_else(|| {
                            Status::internal("missing PMIx per-rank env for multi-task step")
                        })?,
                        Some(&req.environment),
                    )
                    .map_err(Status::failed_precondition)?
                } else {
                    build_multi_task_wrapper(
                        user_script_path.to_string_lossy().as_ref(),
                        num_tasks,
                        Some(&req.environment),
                    )
                }
            } else {
                spur_core::task_launch::build_labeled_single_task_wrapper(
                    user_script_path.to_string_lossy().as_ref(),
                    req.task_offset,
                    Some(&req.environment),
                )
            };
            crate::executor::write_job_scratch(&wrapper_path, &wrapper, req.uid, req.gid)
                .map_err(|e| Status::internal(format!("failed to write step wrapper: {e}")))?;
            guard.paths.push(wrapper_path.clone());

            if num_tasks > 1 {
                senv.set("SPUR_TASK_OFFSET", req.task_offset);
            } else {
                SpurEnv::apply_task_rank(&mut senv, req.task_offset, 0);
            }
            let wrapper_path_string = wrapper_path.to_string_lossy().into_owned();
            ("bash".to_string(), vec![wrapper_path_string], Some(guard))
        } else {
            SpurEnv::apply_task_rank(&mut senv, req.task_offset, 0);
            let (program, args) = spur_core::task_launch::wrap_command_with_cpu_bind(
                &req.command[0],
                &req.command[1..],
                &req.environment,
                req.task_offset,
            );
            (program, args, None)
        };
        let _step_script_guard = step_script_cleanup;

        if let Some(ref task_prolog) = self.hooks.task_prolog {
            let ctx = spur_core::hooks::HookContext {
                job_id,
                work_dir: work_dir.clone(),
                uid: req.uid,
                gid: req.gid,
                partition: partition.clone(),
                nodelist: nodelist.clone(),
                script_context: "prolog_task".into(),
                gpu_devices: gpu_devices.clone(),
                cpus,
                memory_mb,
            };
            if let Err(e) = spur_core::hooks::run_hook(task_prolog, &ctx).await {
                return Err(Status::aborted(format!("TaskProlog failed: {}", e)));
            }
        }

        let mut env = senv.into_map();
        if num_tasks > 1 && step_mpi {
            mpi_plugin::strip_launcher_mpi_env(&mut env);
        }
        if step_mpi && pmix_per_local_rank_env.is_none() {
            let plan = pmix_plan
                .as_ref()
                .ok_or_else(|| Status::internal("missing PMIx plan for step"))?;
            mpi_plugin::apply_pmix_setup_fork_env(&self.mpi_host, plan, req.task_offset, &mut env)
                .map_err(Status::failed_precondition)?;
        }
        let _pmix_step_guard = pmix_step_guard;

        if step_cancel_requested(&self.active_steps, step_key).await {
            return Ok(Response::new(cancelled_step_response()));
        }

        let memlock = self.limits.memlock;

        info!(
            command = ?req.command,
            num_tasks,
            task_offset = req.task_offset,
            uid = req.uid,
            work_dir = %work_dir,
            container_image = req.container.as_ref().map(|c| c.image.as_str()).unwrap_or(""),
            parent_namespaces = job_entry.has_namespaces(),
            "RunCommand: executing step"
        );

        // Redirect the step's stdout/stderr to per-step spool files so
        // stream_job_output can tail them live and output stays bounded on this
        // node. Paths are recorded in active_steps so the tail can find them.
        let step_files = crate::executor::open_step_output_files(job_id, step_id, req.uid, req.gid)
            .map_err(|e| Status::internal(format!("step output files: {e}")))?;
        let stdout_path = step_files.stdout_path.to_string_lossy().into_owned();
        let stderr_path = step_files.stderr_path.to_string_lossy().into_owned();
        {
            let mut steps = self.active_steps.lock().await;
            if let Some(step) = steps.get_mut(&step_key) {
                step.stdout_path = stdout_path.clone();
                step.stderr_path = stderr_path.clone();
            }
        }

        // Three dispatch paths, each wiring the step's stdio to the spool files
        // above. `None` means the step was cancelled before it ran.
        let maybe_status: Option<std::process::ExitStatus> = if job_entry.has_namespaces()
            && job_entry.pid > 0
        {
            // Case 1: parent job is containerized — enter its namespaces via nsenter
            // (srun inside sbatch/salloc --container-image).
            //
            // If the step asked for its own image, it is not honored here: the
            // step joins the parent's live container. Leave a trace rather than
            // silently dropping it (the #777 anti-pattern).
            if let Some(c) = req.container.as_ref() {
                if !c.image.is_empty() {
                    warn!(
                        job_id,
                        step_id,
                        image = %c.image,
                        "step --container-image ignored: joining the parent job's \
                         running container instead of building a new one"
                    );
                }
            }
            // Enter the step's work_dir *inside* the container by cd-ing in a
            // shell that runs after nsenter — a host-side chdir does not
            // survive entering the container's pivoted mount namespace, and
            // nsenter --wd is unreliable under a rootless user namespace. The
            // cd is best-effort (`;`, not `&&`) so a work_dir absent inside
            // the container still runs the command rather than failing it.
            let inner = shlex::try_join(
                std::iter::once(program.as_str()).chain(program_args.iter().map(String::as_str)),
            )
            .map_err(|e| {
                Status::invalid_argument(format!("step command is not shell-safe: {e}"))
            })?;
            let wd = shlex::try_quote(&work_dir)
                .map(|s| s.into_owned())
                .unwrap_or_else(|_| work_dir.clone());
            let full_command = vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("cd {wd} 2>/dev/null; exec {inner}"),
            ];
            let priv_drop = crate::privdrop::PrivDrop::resolve_if_needed(req.uid, req.gid);
            let plan = build_launch_plan(&job_entry, priv_drop.as_ref(), &full_command);
            let mut cmd = tokio::process::Command::new(&plan.program);
            // Restore the pre-change unconditional cwd (a plain host or
            // rootful-namespaced step honours it). Entering a rootless
            // container's pivoted mount namespace does not preserve it — that
            // remains best-effort.
            cmd.args(&plan.args).current_dir(&work_dir).process_group(0);
            for (k, v) in env {
                cmd.env(k, v);
            }
            if plan.apply_priv_in_child {
                if let Some(pd) = priv_drop {
                    unsafe {
                        cmd.pre_exec(move || {
                            crate::executor::apply_memlock(memlock);
                            pd.apply()
                                .map_err(|e| std::io::Error::from_raw_os_error(e as i32))
                        });
                    }
                }
            } else {
                unsafe {
                    cmd.pre_exec(move || {
                        crate::executor::apply_memlock(memlock);
                        Ok(())
                    });
                }
            }
            run_tokio_step_to_spool(cmd, step_files, &self.active_steps, step_key).await?
        } else if req.container.as_ref().is_some_and(|c| !c.image.is_empty()) {
            // Case 2: standalone srun --container-image with no running parent
            // container — set up a fresh rootfs for this step.
            let c = req
                .container
                .as_ref()
                .expect("container present in this arm");
            let step_uid = req.uid;
            let step_gid = req.gid;

            let mounts: Vec<crate::container::BindMount> = c
                .mounts
                .iter()
                .filter_map(|m| crate::container::parse_mount(m).ok())
                .collect();

            let username = env
                .get("USER")
                .or_else(|| env.get("LOGNAME"))
                .cloned()
                .unwrap_or_default();
            let home_dir = env
                .get("HOME")
                .cloned()
                .unwrap_or_else(|| format!("/home/{username}"));

            let container_cfg = crate::container::ContainerConfig {
                image: c.image.clone(),
                mounts,
                // --container-workdir wins; fall back to --chdir (req.work_dir)
                // so `srun --chdir=/foo --container-image=X` lands in /foo
                // rather than /tmp (container_init's default).
                workdir: if !c.workdir.is_empty() {
                    Some(c.workdir.clone())
                } else if !work_dir.is_empty() {
                    Some(work_dir.clone())
                } else {
                    None
                },
                name: if c.name.is_empty() {
                    None
                } else {
                    Some(c.name.clone())
                },
                readonly: c.readonly,
                mount_home: c.mount_home,
                remap_root: c.remap_root,
                gpu_devices: gpu_devices.clone(),
                environment: env.clone(),
                // Deny GPU visibility in container_env for zero-GPU steps, matching
                // the batch path (launch_job). Without this a user could smuggle
                // ROCR_VISIBLE_DEVICES=0 through --container-env on a step that
                // received no GPU allocation.
                container_env: {
                    let mut ce = c.env.clone();
                    maybe_deny_gpu_env(&mut ce, &gpu_devices);
                    ce
                },
                entrypoint: if c.entrypoint.is_empty() {
                    None
                } else {
                    Some(c.entrypoint.clone())
                },
                uid: step_uid,
                gid: step_gid,
                username: if username.is_empty() {
                    "spur".to_string()
                } else {
                    username
                },
                home_dir,
                device_plan: container_device_plan,
            };

            let image_path = crate::container::resolve_image(&c.image, None, Some(step_uid))
                .map_err(|e| Status::failed_precondition(e.to_string()))?;

            // Per-step rootfs namespace, disjoint from the batch job's
            // (`job_<id>`), so a step can never resolve to or delete a batch
            // rootfs, and with no id arithmetic that could overflow.
            let step_base = crate::container::step_rootfs_base(job_id, step_id);
            let (rootfs, rootfs_mode) = crate::container::setup_rootfs(
                &image_path,
                &step_base,
                container_cfg.name.as_deref(),
            )
            .map_err(|e| Status::internal(format!("step container setup failed: {e}")))?;
            // Tear the rootfs down on any exit from here on. The pid is set
            // after the fork (inside run_containerized_step) so the guard
            // kills any live child before unmounting/removing the rootfs.
            let mut rootfs_guard = StepRootfsGuard {
                base: step_base.clone(),
                mode: rootfs_mode.clone(),
                pid: None,
            };

            // Write the step command as a script inside the rootfs so it's
            // accessible after pivot_root hides the host filesystem.
            let step_script_content = {
                let joined = shlex::try_join(
                    std::iter::once(program.as_str())
                        .chain(program_args.iter().map(String::as_str)),
                )
                .map_err(|e| {
                    Status::invalid_argument(format!("step command is not shell-safe: {e}"))
                })?;
                format!("#!/bin/bash\n{joined}\n")
            };
            let script_in_rootfs = format!(
                "{}/tmp/spur_step_{}_{}.sh",
                rootfs.display(),
                job_id,
                step_id
            );
            std::fs::write(&script_in_rootfs, &step_script_content)
                .map_err(|e| Status::internal(format!("failed to write step script: {e}")))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(
                    &script_in_rootfs,
                    std::fs::Permissions::from_mode(0o755),
                );
            }

            // Script path inside the container (after pivot_root, host paths are gone).
            let script_in_container = format!("/tmp/spur_step_{}_{}.sh", job_id, step_id);

            // Run the container step. The guard kills the child before
            // removing the rootfs — preventing a live-process vs rm-rf race
            // when the future is dropped mid-flight. We set the pid as soon
            // as we know it (immediately after the fork, before the wait).
            let (maybe_status, child_pid) = run_containerized_step(
                container_cfg,
                rootfs,
                script_in_container,
                env,
                step_files,
                &self.active_steps,
                step_key,
                memlock,
            )
            .await?;
            if child_pid != 0 {
                rootfs_guard.pid = Some(child_pid);
            }
            maybe_status
        } else {
            // Case 3: no container — plain host process.
            let mut cmd = tokio::process::Command::new(&program);
            cmd.args(&program_args)
                .current_dir(&work_dir)
                .process_group(0);
            for (k, v) in env {
                cmd.env(k, v);
            }
            let priv_drop = crate::privdrop::PrivDrop::resolve_if_needed(req.uid, req.gid);
            unsafe {
                cmd.pre_exec(move || {
                    crate::executor::apply_memlock(memlock);
                    if let Some(ref pd) = priv_drop {
                        pd.apply()
                            .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
                    }
                    Ok(())
                });
            }
            run_tokio_step_to_spool(cmd, step_files, &self.active_steps, step_key).await?
        };

        let status = match maybe_status {
            Some(s) => s,
            None => return Ok(Response::new(cancelled_step_response())),
        };

        if let Some(ref task_epilog) = self.hooks.task_epilog {
            let ctx = spur_core::hooks::HookContext {
                job_id,
                work_dir: work_dir.clone(),
                uid: req.uid,
                gid: req.gid,
                partition,
                nodelist,
                script_context: "epilog_task".into(),
                gpu_devices,
                cpus,
                memory_mb,
            };
            if let Err(e) = spur_core::hooks::run_hook(task_epilog, &ctx).await {
                warn!(error = %e, "TaskEpilog failed");
            }
        }

        // Backward-compatible: the response still carries the step's output by
        // reading the spool files back. #781 replaces this with a client-side
        // StreamJobOutput tail and drops the read-back, removing the memory bound.
        let read_back = |path: String| async move {
            match tokio::fs::read(&path).await {
                Ok(b) => String::from_utf8_lossy(&b).into_owned(),
                Err(e) => {
                    // Don't fail the step over a read-back error, but log it so a
                    // missing response body can be correlated with a filesystem
                    // fault rather than looking like the step produced no output.
                    warn!(path = %path, error = %e, "failed to read back step output");
                    String::new()
                }
            }
        };
        Ok(Response::new(RunCommandResponse {
            exit_code: spur_core::process::shell_exit_code(&status),
            stdout: read_back(stdout_path).await,
            stderr: read_back(stderr_path).await,
        }))
    }

    async fn stream_job_output(
        &self,
        request: Request<StreamJobOutputRequest>,
    ) -> Result<Response<Self::StreamJobOutputStream>, Status> {
        let identity = Self::verified_identity(&request).cloned();
        let req = request.into_inner();
        let job_id = req.job_id;

        self.check_job_access(job_id, identity.as_ref(), &req.user, "read output of")
            .await?;

        // Step output: tail the per-step spool file recorded by run_command and
        // finish when the step leaves active_steps (rather than the batch file,
        // which ends only when the whole allocation does). This is what lets an
        // srun step stream live and terminate at step exit (#781).
        if req.step_id != 0 {
            let active_steps = self.active_steps.clone();
            let want_stderr = req.stream == "stderr";
            let step_id = req.step_id;
            let step_key = (job_id, step_id);
            let (tx, rx) = tokio::sync::mpsc::channel(32);
            tokio::spawn(async move {
                // run_command records the step's output path right before it
                // spawns the child (once the spool file exists). Wait for that,
                // falling back to the deterministic spool path if the file is
                // already on disk (a step that finished before we observed its
                // active_steps entry), and giving up only if the step comes and
                // goes without ever producing a file.
                const START_POLLS: u32 = 150; // 150 * 200ms = 30s startup grace
                let mut file_path = String::new();
                let mut seen = false;
                for _ in 0..START_POLLS {
                    {
                        let steps = active_steps.lock().await;
                        if let Some(step) = steps.get(&step_key) {
                            seen = true;
                            let path = if want_stderr {
                                &step.stderr_path
                            } else {
                                &step.stdout_path
                            };
                            if !path.is_empty() {
                                file_path = path.clone();
                                break;
                            }
                        }
                    }
                    if let Some(existing) =
                        crate::executor::existing_step_output_path(job_id, step_id, want_stderr)
                    {
                        file_path = existing;
                        break;
                    }
                    // Present then gone without a file: the step failed to spawn.
                    if seen && !active_steps.lock().await.contains_key(&step_key) {
                        break;
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                }
                if file_path.is_empty() {
                    // Could not resolve the step's spool file (the step failed to
                    // start, or the step_id is wrong). Signal an error rather than
                    // a clean EOF, so the client falls back to the buffered RunStep
                    // output instead of treating an empty stream as success.
                    let _ = tx
                        .send(Err(Status::not_found(format!(
                            "no output stream for job {job_id} step {step_id}"
                        ))))
                        .await;
                    return;
                }
                use tokio::io::{AsyncReadExt, AsyncSeekExt};
                let mut file = match tokio::fs::File::open(&file_path).await {
                    Ok(f) => f,
                    Err(e) => {
                        let _ = tx
                            .send(Err(Status::internal(format!("open step output file: {e}"))))
                            .await;
                        return;
                    }
                };
                // Tail incrementally: seek to the last offset and read only the
                // newly appended bytes each poll, instead of re-reading the whole
                // file (which is O(n^2) for high-volume steps).
                let mut offset: u64 = 0;
                loop {
                    if file.seek(std::io::SeekFrom::Start(offset)).await.is_ok() {
                        let mut buf = Vec::new();
                        if let Ok(n) = file.read_to_end(&mut buf).await {
                            if n > 0 {
                                offset += n as u64;
                                if tx
                                    .send(Ok(StreamJobOutputChunk {
                                        data: buf,
                                        eof: false,
                                    }))
                                    .await
                                    .is_err()
                                {
                                    break; // client disconnected
                                }
                            }
                        }
                    }
                    let still_running = active_steps.lock().await.contains_key(&step_key);
                    if !still_running {
                        // Final read to drain anything written after the last poll.
                        if file.seek(std::io::SeekFrom::Start(offset)).await.is_ok() {
                            let mut buf = Vec::new();
                            if let Ok(n) = file.read_to_end(&mut buf).await {
                                if n > 0 {
                                    let _ = tx
                                        .send(Ok(StreamJobOutputChunk {
                                            data: buf,
                                            eof: false,
                                        }))
                                        .await;
                                }
                            }
                        }
                        let _ = tx
                            .send(Ok(StreamJobOutputChunk {
                                data: Vec::new(),
                                eof: true,
                            }))
                            .await;
                        break;
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                }
            });
            return Ok(Response::new(ReceiverStream::new(rx)));
        }

        // No retry on a miss, same as run_command: a Running job has been
        // confirmed on every node (confirm_dispatch_on_nodes). Callers here
        // (srun --attach, sattach) hit the agent directly with no controller
        // proxy, so they inherit their own job.state check. Restart mid-job
        // (empty `running`) is the one uncovered case.
        let file_path = {
            let jobs = self.running.lock().await;
            match jobs.get(&job_id) {
                Some(tracked) => {
                    if req.stream == "stderr" {
                        tracked.stderr_path.clone()
                    } else {
                        tracked.stdout_path.clone()
                    }
                }
                None => {
                    return Err(Status::not_found(format!(
                        "job {} not running on this node",
                        job_id
                    )));
                }
            }
        };

        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let running = self.running.clone();

        tokio::spawn(async move {
            // Wait for the output file to appear
            let mut waited = 0;
            while !std::path::Path::new(&file_path).exists() && waited < 30 {
                tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                waited += 1;
            }

            let mut offset = 0u64;
            loop {
                // Read new data from the file
                if let Ok(data) = tokio::fs::read(&file_path).await {
                    if data.len() as u64 > offset {
                        let new_data = data[offset as usize..].to_vec();
                        offset = data.len() as u64;
                        if tx
                            .send(Ok(StreamJobOutputChunk {
                                data: new_data,
                                eof: false,
                            }))
                            .await
                            .is_err()
                        {
                            break; // Client disconnected
                        }
                    }
                }

                // Check if job is still running
                let still_running = running.lock().await.contains_key(&job_id);
                if !still_running {
                    // Final read to get any remaining output
                    if let Ok(data) = tokio::fs::read(&file_path).await {
                        if data.len() as u64 > offset {
                            let _ = tx
                                .send(Ok(StreamJobOutputChunk {
                                    data: data[offset as usize..].to_vec(),
                                    eof: false,
                                }))
                                .await;
                        }
                    }
                    let _ = tx
                        .send(Ok(StreamJobOutputChunk {
                            data: Vec::new(),
                            eof: true,
                        }))
                        .await;
                    break;
                }

                tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn interactive_session(
        &self,
        request: Request<tonic::Streaming<InteractiveInput>>,
    ) -> Result<Response<Self::InteractiveSessionStream>, Status> {
        use crate::pty::WindowSize as PtyWinSize;

        let identity = Self::verified_identity(&request).cloned();
        let mut inbound = request.into_inner();

        let first = inbound
            .message()
            .await
            .map_err(|e| Status::internal(format!("stream recv error: {e}")))?
            .ok_or_else(|| Status::invalid_argument("empty stream: expected InitSession"))?;

        let init = match first.msg {
            Some(interactive_input::Msg::Init(init)) => init,
            _ => {
                return Err(Status::invalid_argument(
                    "first message must be InitSession",
                ));
            }
        };

        self.check_job_access(init.job_id, identity.as_ref(), &init.user, "attach to")
            .await?;

        let entry = self.job_entry(init.job_id).await?;

        let winsize = init.winsize.as_ref().map(|ws| PtyWinSize {
            rows: ws.rows as u16,
            cols: ws.cols as u16,
            xpixel: ws.xpixel as u16,
            ypixel: ws.ypixel as u16,
        });

        let argv: Vec<String> = init.argv.clone();

        // Same defense-in-depth gate as exec_in_job: the uid comes from the tracked job, but an
        // interactive PTY into a root job must obey allow_root_jobs too. Checked here rather than
        // inside spawn_pty_in_job, which is a static helper with no access to the agent config.
        if let Err(msg) = crate::privdrop::check_root_execution_allowed(
            entry.uid,
            self.allow_root_jobs,
            self.spurd_is_root,
        ) {
            warn!(job_id = init.job_id, uid = entry.uid, "{msg}");
            return Err(Status::permission_denied(msg));
        }

        let (master_fd, child, child_pid) =
            Self::spawn_pty_in_job(&entry, &argv, init.job_id, winsize.as_ref())?;

        info!(
            job_id = init.job_id,
            child_pid,
            overlap = init.overlap,
            "interactive session started"
        );

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<InteractiveOutput, Status>>(64);

        tokio::spawn(Self::run_pty_bridge(
            master_fd, child, child_pid, inbound, tx,
        ));

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    // -- Native cluster component control: drive this node's k0s systemd unit. --
    async fn start_cluster_component(
        &self,
        request: Request<StartClusterComponentRequest>,
    ) -> Result<Response<StartClusterComponentResponse>, Status> {
        let req = request.into_inner();
        let role = crate::cluster::ClusterRole::from_str(&req.role)
            .ok_or_else(|| Status::invalid_argument(format!("unknown role: {}", req.role)))?;
        match self
            .k0s
            .start(role, req.join_token, req.k0s_config, req.node_ip)
            .await
        {
            Ok(state) => Ok(Response::new(StartClusterComponentResponse {
                started: true,
                component_state: state,
                message: String::new(),
            })),
            Err(e) => Ok(Response::new(StartClusterComponentResponse {
                started: false,
                component_state: "failed".to_string(),
                message: e.to_string(),
            })),
        }
    }

    async fn stop_cluster_component(
        &self,
        request: Request<StopClusterComponentRequest>,
    ) -> Result<Response<StopClusterComponentResponse>, Status> {
        match self.k0s.stop(request.into_inner().reset).await {
            Ok(()) => Ok(Response::new(StopClusterComponentResponse {
                stopped: true,
                message: String::new(),
            })),
            Err(e) => Ok(Response::new(StopClusterComponentResponse {
                stopped: false,
                message: e.to_string(),
            })),
        }
    }

    async fn get_cluster_component_status(
        &self,
        _request: Request<GetClusterComponentStatusRequest>,
    ) -> Result<Response<GetClusterComponentStatusResponse>, Status> {
        let (role, component_state, enabled) = self.k0s.status().await;
        Ok(Response::new(GetClusterComponentStatusResponse {
            role,
            component_state,
            enabled,
        }))
    }

    async fn create_k0s_join_token(
        &self,
        request: Request<CreateK0sJoinTokenRequest>,
    ) -> Result<Response<CreateK0sJoinTokenResponse>, Status> {
        Self::require_controller(&request)?;
        let req = request.into_inner();
        match self
            .k0s
            .create_join_token(&req.role, req.expiry_seconds)
            .await
        {
            Ok(join_token) => Ok(Response::new(CreateK0sJoinTokenResponse { join_token })),
            Err(e) => Err(Status::internal(format!("k0s token create failed: {e}"))),
        }
    }

    async fn drain_k8s_node(
        &self,
        request: Request<DrainK8sNodeRequest>,
    ) -> Result<Response<DrainK8sNodeResponse>, Status> {
        let req = request.into_inner();
        match self
            .k0s
            .drain_node(&req.node, req.timeout_secs, req.force)
            .await
        {
            Ok(()) => Ok(Response::new(DrainK8sNodeResponse {
                drained: true,
                message: String::new(),
            })),
            // In-band failure (drain blocked/timed out): the controller decides whether to proceed
            // (with --force) or leave the node cordoned, so report it as a normal response.
            Err(e) => Ok(Response::new(DrainK8sNodeResponse {
                drained: false,
                message: e.to_string(),
            })),
        }
    }

    async fn delete_k8s_node(
        &self,
        request: Request<DeleteK8sNodeRequest>,
    ) -> Result<Response<DeleteK8sNodeResponse>, Status> {
        let req = request.into_inner();
        match self.k0s.delete_node(&req.node).await {
            Ok(()) => Ok(Response::new(DeleteK8sNodeResponse {
                deleted: true,
                message: String::new(),
            })),
            Err(e) => Ok(Response::new(DeleteK8sNodeResponse {
                deleted: false,
                message: e.to_string(),
            })),
        }
    }

    async fn get_kubeconfig(
        &self,
        request: Request<GetKubeconfigRequest>,
    ) -> Result<Response<GetKubeconfigResponse>, Status> {
        Self::require_controller(&request)?;
        let req = request.into_inner();
        // Empty user -> cluster-admin kubeconfig; set -> a scoped kubeconfig (SA + bound token in the
        // user's account namespace). The controller already gates admin kubeconfig behind
        // `is_k0s_admin` + `allow_admin_kubeconfig`; requiring the controller here keeps that from
        // being sidestepped by dialing the agent directly.
        let result = if req.user.is_empty() {
            self.k0s.admin_kubeconfig().await
        } else {
            self.k0s
                .user_kubeconfig(&req.user, &req.namespace, &req.service_account)
                .await
        };
        match result {
            Ok(kubeconfig) => Ok(Response::new(GetKubeconfigResponse { kubeconfig })),
            Err(e) => Err(Status::internal(format!("get kubeconfig failed: {e}"))),
        }
    }

    async fn apply_mesh(
        &self,
        request: Request<MeshMembership>,
    ) -> Result<Response<ApplyMeshResponse>, Status> {
        // Use the interface spurd resolved at startup (via the reporter), not a fresh env read
        // that would ignore spur.conf and could diverge from the rest of spurd.
        let iface = self.reporter.wg_iface.clone();
        // proto -> spur-net mesh types.
        let members: Vec<spur_net::mesh::MeshNode> = request
            .into_inner()
            .nodes
            .into_iter()
            .map(|n| spur_net::mesh::MeshNode {
                hostname: n.hostname,
                public_key: n.public_key,
                mesh_ip: n.mesh_ip,
                endpoint: n.endpoint,
                pod_cidr: n.pod_cidr,
            })
            .collect();
        let self_host = self.reporter.hostname.clone();

        // All of this shells out to `wg` (blocking) — run it off the async runtime. Native-routing
        // CNI owns the FIB routes, so program_routes = false.
        let result =
            tokio::task::spawn_blocking(move || -> anyhow::Result<(bool, usize, String)> {
                // Identify self in the membership (so it's excluded from the peer set): prefer the local
                // WireGuard public key, fall back to hostname.
                let self_pubkey = spur_net::wireguard::interface_public_key(&iface).ok();
                let self_mesh_ip = members
                    .iter()
                    .find(|n| {
                        self_pubkey.as_deref() == Some(n.public_key.as_str())
                            || n.hostname == self_host
                    })
                    .map(|n| n.mesh_ip.clone());
                let Some(self_mesh_ip) = self_mesh_ip else {
                    return Ok((
                        false,
                        0,
                        "this node is not in the pushed mesh membership".to_string(),
                    ));
                };
                // Reconcile: prune peers no longer in the membership, then add/update the desired peers.
                let current = spur_net::wireguard::list_peers(&iface).unwrap_or_default();
                let (added, pruned) = spur_net::mesh::reconcile_mesh(
                    &iface,
                    &self_mesh_ip,
                    &members,
                    &current,
                    false,
                )?;
                Ok((
                    true,
                    added,
                    format!("reconciled mesh: {added} peers, {pruned} pruned"),
                ))
            })
            .await
            .map_err(|e| Status::internal(format!("apply_mesh task panicked: {e}")))?;

        match result {
            Ok((applied, peers, message)) => {
                if applied {
                    info!(peers, message = %message, "applied WireGuard mesh");
                } else {
                    warn!(message = %message, "mesh not applied");
                }
                Ok(Response::new(ApplyMeshResponse {
                    applied,
                    peers: peers as u32,
                    message,
                }))
            }
            Err(e) => Ok(Response::new(ApplyMeshResponse {
                applied: false,
                peers: 0,
                message: e.to_string(),
            })),
        }
    }
}

impl AgentService {
    async fn drop_tracked_job(&self, job_id: u32) {
        if self.running.lock().await.remove(&job_id).is_some() {
            self.allocation.lock().await.release_job(job_id);
            if let Err(e) = self.mpi_host.stop_pmix_server(job_id) {
                warn!(job_id, error = %e, "PMIx stop failed on job drop");
            }
        }
    }

    /// Record controller-allocated GPUs and reserve the local CPU/memory budget.
    /// `cpus`/`memory_mb` are the resolved grant, so the core IDs match it.
    async fn allocate_local_resources(
        &self,
        job_id: u32,
        spec: &JobSpec,
        allocated: Option<&ResourceAllocations>,
        cpus: u32,
        memory_mb: u64,
    ) -> Result<(AllocationResult, Vec<u32>), Status> {
        let controller_gpu_ids: Vec<u32> = allocated
            .and_then(|a| a.devices.get("gpu"))
            .map(|d| d.devices.iter().map(|dev| dev.device_id).collect())
            .unwrap_or_default();

        let (gres_gpu_count, gres_gpu_type) = Self::parse_gpu_gres(&spec.gres);

        if controller_gpu_ids.is_empty() && gres_gpu_count > 0 {
            return Err(Status::internal(format!(
                "job requests {} GPUs (type: {}) but controller sent no device IDs",
                gres_gpu_count,
                gres_gpu_type.as_deref().unwrap_or("any"),
            )));
        }

        // Hold running across the reclaim (running-then-allocation, as in commit)
        // so a concurrent commit can't make a live owner look stale.
        let running = self.running.lock().await;
        let live: std::collections::HashSet<u32> = running.keys().copied().collect();

        let mut alloc = self.allocation.lock().await;

        let result = match alloc.allocate_for_job(job_id, cpus, memory_mb, &controller_gpu_ids) {
            Ok(result) => result,
            Err(AllocError::GpusUnavailable) => {
                // A conflicting owner absent from the live set is stale (the
                // controller only re-launches after freeing it); reclaim and retry.
                let stale: Vec<u32> = alloc
                    .conflicting_owners(&controller_gpu_ids)
                    .into_iter()
                    .filter(|owner| !live.contains(owner))
                    .collect();
                if !stale.is_empty() {
                    warn!(
                        job_id,
                        reclaimed = ?stale,
                        requested = ?controller_gpu_ids,
                        "reclaiming stale GPU owners no longer running, then retrying dispatch"
                    );
                    for owner in &stale {
                        alloc.release_job(*owner);
                    }
                }
                match alloc.allocate_for_job(job_id, cpus, memory_mb, &controller_gpu_ids) {
                    Ok(result) => result,
                    Err(_) => {
                        warn!(
                            job_id,
                            requested = ?controller_gpu_ids,
                            already_allocated = ?alloc.allocated_gpu_ids(),
                            "rejecting dispatch: controller-allocated GPUs already in use in the \
                             local allocation table by a still-running or launching job"
                        );
                        return Err(Status::resource_exhausted(
                            "controller-allocated GPUs unavailable on this node",
                        ));
                    }
                }
            }
            Err(AllocError::DuplicateJob) => {
                // A launch is already in flight for this job id (reserved, not
                // yet committed or released). This is a concurrent duplicate,
                // not resource exhaustion. A stale reservation from a prior,
                // already-torn-down run is superseded inside allocate_for_job
                // and does not reach here.
                warn!(
                    job_id,
                    "rejecting duplicate launch: a launch is already in flight for this job"
                );
                return Err(Status::already_exists(format!(
                    "job {job_id} already has a launch in flight on this node"
                )));
            }
        };

        let gpu_ids = controller_gpu_ids;
        Ok((result, gpu_ids))
    }

    /// Resolve the per-node budget the way `launch_job` does, then reserve, so
    /// GPU-path tests exercise the real resolution instead of fixed numbers.
    #[cfg(test)]
    async fn allocate_local_for_test(
        &self,
        job_id: u32,
        spec: &JobSpec,
        allocated: Option<&ResourceAllocations>,
    ) -> Result<(AllocationResult, Vec<u32>), Status> {
        let (cpus, memory_mb) = resolve_cgroup_budget(allocated, spec, 1);
        self.allocate_local_resources(job_id, spec, allocated, cpus, memory_mb)
            .await
    }

    fn parse_gpu_gres(gres: &[String]) -> (u32, Option<String>) {
        let mut count = 0;
        let mut gpu_type = None;
        for g in gres {
            if let Some((name, gtype, n)) = spur_core::resource::parse_gres(g) {
                if name == "gpu" {
                    count += n;
                    if gtype.is_some() {
                        gpu_type = gtype;
                    }
                }
            }
        }
        (count, gpu_type)
    }

    /// Send a user-specified signal to a running job.
    async fn send_explicit_signal(&self, job_id: u32, signal: i32) {
        let is_allocation_only = {
            let jobs = self.running.lock().await;
            jobs.get(&job_id)
                .is_some_and(|tracked| tracked.job.is_allocation_only())
        };
        if is_allocation_only {
            self.cancel_active_steps_for_job(job_id, signal).await;
            self.drop_tracked_job(job_id).await;
            return;
        }

        let jobs = self.running.lock().await;
        let Some(tracked) = jobs.get(&job_id) else {
            return;
        };
        let sig =
            nix::sys::signal::Signal::try_from(signal).unwrap_or(nix::sys::signal::Signal::SIGTERM);
        info!(job_id, signal, "sending explicit signal to job");
        let _ = tracked.job.kill_signal(sig);
    }

    /// Freeze (SIGSTOP) or thaw (SIGCONT) a running job's process(es).
    async fn suspend_signal(&self, job_id: u32, resume: bool) {
        let jobs = self.running.lock().await;
        let Some(tracked) = jobs.get(&job_id) else {
            return;
        };
        let sig = if resume {
            nix::sys::signal::Signal::SIGCONT
        } else {
            nix::sys::signal::Signal::SIGSTOP
        };
        info!(job_id, resume, "sending suspend/resume signal to job");
        let _ = tracked.job.kill_signal(sig);
    }

    /// Signal every in-flight step of a job. Allocation-only jobs (standalone
    /// `srun` / `salloc`) have no tracked batch process that owns the step
    /// processes, so a job-level cancel must reach the steps directly or they
    /// orphan. This matters most for a containerized step, whose rootfs is torn
    /// down only when its `run_command` returns — an unsignaled step would leak
    /// both the container process and its rootfs.
    ///
    /// A containerized step runs as PID 1 of its own PID namespace. Signals from
    /// an ancestor namespace to a namespace's init are *discarded* unless init
    /// installed a handler for them — SIGTERM/SIGINT to a `bash`/`sleep` init are
    /// dropped. Only SIGKILL and SIGSTOP are force-delivered. So the requested
    /// signal is sent first (graceful for host steps), then SIGKILL after a short
    /// grace period guarantees a container init dies.
    async fn cancel_active_steps_for_job(&self, job_id: u32, signal: i32) {
        // Snapshot (key, pid, epoch) under the lock, then signal *outside* it:
        // signal_step_tree walks /proc, so holding active_steps across it would
        // block every concurrent run_command (and the ActiveStepGuard's try_lock
        // cleanup, which silently skips on contention).
        let targets: Vec<((u32, u32), u32, u64)> = {
            let mut steps = self.active_steps.lock().await;
            let mut targets = Vec::new();
            for (key, step) in steps.iter_mut() {
                if key.0 == job_id {
                    step.cancel_requested = true;
                    if let Some(pid) = step.pid {
                        targets.push((*key, pid, step.epoch));
                    }
                }
            }
            targets
        };
        if targets.is_empty() {
            return;
        }
        for (_key, pid, _epoch) in &targets {
            signal_step_tree(*pid, signal);
        }
        // Escalate to SIGKILL: a container-init step ignores the graceful signal.
        // Compare epochs (not pids) so a step that reused the (job, step) key
        // with a recycled pid is never signalled.
        let active_steps = self.active_steps.clone();
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
            let still: Vec<u32> = {
                let steps = active_steps.lock().await;
                targets
                    .iter()
                    .filter(|(key, _pid, epoch)| steps.get(key).map(|s| s.epoch) == Some(*epoch))
                    .map(|(_key, pid, _epoch)| *pid)
                    .collect()
            };
            for pid in still {
                signal_step_tree(pid, nix::sys::signal::Signal::SIGKILL as i32);
            }
        });
    }

    async fn graceful_cancel(&self, job_id: u32) {
        let is_allocation_only = {
            let jobs = self.running.lock().await;
            jobs.get(&job_id)
                .is_some_and(|tracked| tracked.job.is_allocation_only())
        };
        if is_allocation_only {
            self.cancel_active_steps_for_job(job_id, nix::sys::signal::Signal::SIGTERM as i32)
                .await;
            self.drop_tracked_job(job_id).await;
            return;
        }

        // Epoch of the run we're cancelling; the delayed SIGKILL below must not
        // touch a newer run that reused this job_id after a requeue.
        let cancel_attempt = {
            let jobs = self.running.lock().await;
            let Some(tracked) = jobs.get(&job_id) else {
                return;
            };
            info!(job_id, "graceful cancel: SIGTERM → 5s grace → SIGKILL");
            let _ = tracked.job.kill_signal(nix::sys::signal::Signal::SIGTERM);
            tracked.run_attempt
        };

        let running = self.running.clone();
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            let jobs = running.lock().await;
            if let Some(tracked) = jobs.get(&job_id) {
                // Skip if job_id was reused by a newer run after requeue.
                if tracked.run_attempt != cancel_attempt {
                    return;
                }
                info!(job_id, "grace period expired, sending SIGKILL");
                let _ = tracked.job.kill_signal(nix::sys::signal::Signal::SIGKILL);
                // Job stays in `running` and monitor loop reaps it and does full cleanup.
            }
        });
    }

    /// The verified identity for this request, if the auth layer authenticated one.
    ///
    /// `None` means the caller presented no credential — allowed only under `permissive`/`disabled`,
    /// where the pre-auth behavior is preserved (`required` never reaches a handler unauthenticated,
    /// the auth layer rejects first). Only [`crate::auth_middleware`] inserts this, so its presence
    /// always means "verified".
    fn verified_identity<T>(request: &Request<T>) -> Option<&spur_core::auth::Identity> {
        request.extensions().get::<spur_core::auth::Identity>()
    }

    /// Refuse a controller-only RPC unless the verified caller is the cluster controller.
    ///
    /// These RPCs (launch/run/cancel/suspend, kubeconfig, join-token) drive work and secrets that
    /// only the control plane may request; a valid *user* token — which verifies identically to the
    /// controller's under the shared cluster key — must not reach them by dialing the agent directly.
    /// An unauthenticated caller is tolerated only under `permissive`/`disabled` (there is no
    /// identity to check), matching the rest of the agent's no-auth behavior.
    fn require_controller<T>(request: &Request<T>) -> Result<(), Status> {
        match Self::verified_identity(request) {
            Some(id) if id.is_controller() => Ok(()),
            Some(id) => Err(Status::permission_denied(format!(
                "this RPC is reachable only by the cluster controller; caller '{}' is not the \
                 controller — route the request through spurctld",
                id.user
            ))),
            None => Ok(()),
        }
    }

    /// Whether `node` names this agent's own host.
    ///
    /// A launch carries the node the controller scheduled it onto; if it does not name this host the
    /// request was misrouted (or aimed straight at the wrong node's agent) and must not run here.
    /// Accepts either the reporter's node name or the OS hostname to tolerate short/long-name skew.
    fn agent_owns_node(&self, node: &str) -> bool {
        if node == self.reporter.hostname {
            return true;
        }
        hostname::get()
            .map(|h| h.to_string_lossy() == node)
            .unwrap_or(false)
    }

    /// Gate a user-facing attach/exec/stream on job `job_id` for `action`.
    ///
    /// The caller is the *verified* identity, not a wire-supplied `user`: the owner reaches their own
    /// job, an admin (or the controller) reaches any job, and everyone else is refused. With no
    /// verified identity (`permissive`/`disabled` and no credential) the asserted `user` is trusted
    /// as a plain, non-privileged principal — never as an internal caller — so an empty or `"root"`
    /// string can no longer stand in for one.
    ///
    /// Enforced here as well as on the controller because `sattach` and the output stream dial the
    /// agent's port directly.
    async fn check_job_access(
        &self,
        job_id: u32,
        identity: Option<&spur_core::auth::Identity>,
        asserted_user: &str,
        action: &str,
    ) -> Result<(), Status> {
        let jobs = self.running.lock().await;
        let tracked = jobs
            .get(&job_id)
            .ok_or_else(|| Status::not_found(format!("job {} not running on this node", job_id)))?;

        let (user, is_internal) = match identity {
            Some(id) => (id.user.as_str(), id.is_admin),
            None => (asserted_user, false),
        };
        spur_core::auth::check_job_owner(user, is_internal, &tracked.user, action)
            .map_err(|e| Status::permission_denied(e.to_string()))
    }

    /// Extract a `JobEntry` from a tracked running job for namespace entry.
    ///
    /// Backs `exec_in_job` and `interactive_session` (attach), both reachable
    /// only after the controller's `job.state == Running` check — i.e. every
    /// node has confirmed LaunchJob (confirm_dispatch_on_nodes) — so no retry
    /// on a miss. Restart mid-job (empty `running`) is the one uncovered case.
    async fn job_entry(&self, job_id: u32) -> Result<crate::job_entry::JobEntry, Status> {
        let jobs = self.running.lock().await;
        let tracked = jobs
            .get(&job_id)
            .ok_or_else(|| Status::not_found(format!("job {} not running on this node", job_id)))?;

        let pid = tracked.job.pid().unwrap_or(0);

        Ok(crate::job_entry::JobEntry {
            pid: pid as i32,
            has_pid_namespace: tracked.has_pid_namespace,
            has_user_namespace: tracked.has_user_namespace,
            has_mount_namespace: tracked.has_mount_namespace,
            uid: tracked.uid,
            gid: tracked.gid,
            work_dir: tracked.work_dir.clone(),
        })
    }

    /// Bidirectional PTY bridge: reads master fd, forwards inbound messages
    /// (stdin, resize, signal), and drains remaining output after child exit.
    async fn run_pty_bridge<S>(
        master: std::os::fd::OwnedFd,
        mut child: tokio::process::Child,
        child_pid: i32,
        mut inbound: S,
        tx: tokio::sync::mpsc::Sender<Result<InteractiveOutput, Status>>,
    ) where
        S: tokio_stream::Stream<Item = Result<InteractiveInput, Status>> + Unpin + Send,
    {
        use crate::pty::WindowSize as PtyWinSize;
        use std::os::fd::AsRawFd;
        use tokio::io::unix::AsyncFd;
        use tokio_stream::StreamExt;

        let master_raw = master.as_raw_fd();
        let async_fd = match AsyncFd::new(master) {
            Ok(fd) => fd,
            Err(e) => {
                let _ = tx
                    .send(Err(Status::internal(format!("AsyncFd setup: {e}"))))
                    .await;
                return;
            }
        };

        let mut read_buf = vec![0u8; 4096];
        let mut child_exited = false;
        let mut exit_code: i32 = 128;

        loop {
            tokio::select! {
                readable = async_fd.readable() => {
                    match readable {
                        Ok(mut guard) => {
                            match Self::try_read_pty(&mut guard, &mut read_buf) {
                                Ok(None) => break,
                                Ok(Some(0)) => continue,
                                Ok(Some(n)) => {
                                    let msg = InteractiveOutput {
                                        msg: Some(interactive_output::Msg::Data(
                                            read_buf[..n].to_vec(),
                                        )),
                                    };
                                    if tx.send(Ok(msg)).await.is_err() {
                                        break;
                                    }
                                }
                                Err(e) => {
                                    warn!(error = %e, "PTY read error");
                                    break;
                                }
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, "AsyncFd readable error");
                            break;
                        }
                    }
                }

                item = inbound.next(), if !child_exited => {
                    match item {
                        Some(Ok(input)) => {
                            match input.msg {
                                Some(interactive_input::Msg::Stdin(data)) => {
                                    if Self::async_write_pty(&async_fd, &data).await.is_err() {
                                        break;
                                    }
                                }
                                Some(interactive_input::Msg::Resize(ws)) => {
                                    let _ = crate::pty::resize(master_raw, &PtyWinSize {
                                        rows: ws.rows as u16,
                                        cols: ws.cols as u16,
                                        xpixel: ws.xpixel as u16,
                                        ypixel: ws.ypixel as u16,
                                    });
                                }
                                Some(interactive_input::Msg::Signal(sig)) => {
                                    let _ = crate::pty::signal_foreground(
                                        master_raw, child_pid, sig,
                                    );
                                }
                                Some(interactive_input::Msg::Init(_)) | None => {}
                            }
                        }
                        Some(Err(_)) | None => {
                            let _ = crate::pty::signal_foreground(
                                master_raw, child_pid, libc::SIGHUP,
                            );
                            break;
                        }
                    }
                }

                status = child.wait(), if !child_exited => {
                    exit_code = match status {
                        Ok(s) => s.code().unwrap_or(128),
                        Err(_) => 128,
                    };
                    child_exited = true;
                }
            }
        }

        if !child_exited {
            exit_code = match child.wait().await {
                Ok(s) => s.code().unwrap_or(128),
                Err(_) => 128,
            };
        }

        let _ = tx
            .send(Ok(InteractiveOutput {
                msg: Some(interactive_output::Msg::ExitStatus(exit_code)),
            }))
            .await;
    }

    /// Non-blocking read from a PTY master via an AsyncFd ready guard.
    /// Returns `Ok(Some(n))` on data, `Ok(None)` on EOF/EIO, `Err` on
    /// fatal error. `Some(0)` means WouldBlock (caller should continue).
    fn try_read_pty(
        guard: &mut tokio::io::unix::AsyncFdReadyGuard<'_, std::os::fd::OwnedFd>,
        buf: &mut [u8],
    ) -> Result<Option<usize>, std::io::Error> {
        use std::os::fd::AsRawFd;
        match guard.try_io(|fd| {
            let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr() as *mut _, buf.len()) };
            if n < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        }) {
            Ok(Ok(0)) => Ok(None),
            Ok(Ok(n)) => Ok(Some(n)),
            Ok(Err(e)) if e.raw_os_error() == Some(libc::EIO) => Ok(None),
            Ok(Err(e)) => Err(e),
            Err(_would_block) => Ok(Some(0)),
        }
    }

    /// Non-blocking write to a PTY master via AsyncFd.
    async fn async_write_pty(
        async_fd: &tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>,
        data: &[u8],
    ) -> Result<(), std::io::Error> {
        use std::os::fd::AsRawFd;
        let mut written = 0;
        while written < data.len() {
            let mut guard = async_fd.writable().await?;
            match guard.try_io(|fd| {
                let n = unsafe {
                    libc::write(
                        fd.as_raw_fd(),
                        data[written..].as_ptr() as *const _,
                        data.len() - written,
                    )
                };
                if n < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            }) {
                Ok(Ok(n)) => written += n,
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => continue,
            }
        }
        Ok(())
    }

    fn spawn_pty_in_job(
        entry: &crate::job_entry::JobEntry,
        argv: &[String],
        job_id: u32,
        winsize: Option<&crate::pty::WindowSize>,
    ) -> Result<(std::os::fd::OwnedFd, tokio::process::Child, i32), Status> {
        use std::os::fd::AsRawFd;
        use std::process::Stdio;

        let (master, slave) = crate::pty::openpty_with_winsize(winsize)
            .map_err(|e| Status::internal(format!("openpty: {e}")))?;

        let shell = if argv.is_empty() {
            let bash_exists = if entry.pid > 0 && entry.has_mount_namespace {
                std::path::Path::new(&format!("/proc/{}/root/bin/bash", entry.pid)).exists()
            } else {
                std::path::Path::new("/bin/bash").exists()
            };
            if bash_exists {
                vec!["/bin/bash".to_string()]
            } else {
                vec!["/bin/sh".to_string()]
            }
        } else {
            argv.to_vec()
        };

        let priv_drop = crate::privdrop::PrivDrop::resolve_if_needed(entry.uid, entry.gid);

        let plan = build_launch_plan(entry, priv_drop.as_ref(), &shell);
        let launch_cmd = plan.program;
        let launch_args = plan.args;
        let apply_priv_in_child = plan.apply_priv_in_child;

        let mut cmd = tokio::process::Command::new(&launch_cmd);
        let work_dir = if entry.work_dir.is_empty() {
            "/tmp"
        } else {
            &entry.work_dir
        };
        cmd.args(&launch_args)
            .current_dir(work_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        if entry.pid > 0 {
            for (k, v) in Self::read_proc_environ(entry.pid as u32) {
                cmd.env(k, v);
            }
        }
        for (k, v) in entry.env_vars(job_id) {
            cmd.env(k, v);
        }
        if entry.uid > 0 {
            if let Some(user) = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(entry.uid))
                .ok()
                .flatten()
            {
                cmd.env("HOME", user.dir.to_string_lossy().as_ref());
                cmd.env("USER", &user.name);
                cmd.env("LOGNAME", &user.name);
                cmd.env("SHELL", user.shell.to_string_lossy().as_ref());
            }
        }

        let raw = crate::executor::JobIoRaw::Pty {
            master: master.as_raw_fd(),
            slave: slave.as_raw_fd(),
        };
        let priv_drop_for_child = if apply_priv_in_child { priv_drop } else { None };
        unsafe {
            cmd.pre_exec(move || {
                raw.wire()?;
                if let Some(ref pd) = priv_drop_for_child {
                    pd.apply()
                        .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
                }
                Ok(())
            });
        }

        let child = cmd
            .spawn()
            .map_err(|e| Status::internal(format!("spawn PTY shell: {e}")))?;
        let child_pid = child
            .id()
            .ok_or_else(|| Status::internal("spawned PTY child exited before pid could be read"))?
            as i32;

        drop(slave);

        // Set non-blocking so AsyncFd reads/writes are correct.
        nix::fcntl::fcntl(
            &master,
            nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
        )
        .map_err(|e| Status::internal(format!("fcntl O_NONBLOCK: {e}")))?;

        Ok((master, child, child_pid))
    }

    /// Read environment variables from a running process via /proc.
    fn read_proc_environ(pid: u32) -> Vec<(String, String)> {
        const MAX_ENVIRON: usize = 1 << 20; // 1 MiB
        let path = format!("/proc/{}/environ", pid);
        let mut buf = vec![0u8; MAX_ENVIRON];
        let n = match std::fs::File::open(&path).and_then(|mut f| {
            use std::io::Read;
            f.read(&mut buf)
        }) {
            Ok(n) => n,
            Err(_) => return Vec::new(),
        };
        buf.truncate(n);
        buf.split(|&b| b == 0)
            .filter_map(|entry| {
                let s = std::str::from_utf8(entry).ok()?;
                let (k, v) = s.split_once('=')?;
                Some((k.to_string(), v.to_string()))
            })
            .collect()
    }
}

#[cfg(test)]
impl TrackedJob {
    fn dummy(_pid: u32) -> Self {
        // Spawn in its own process group, matching how real managed jobs are
        // launched, so group-targeted signals (kill_signal) land correctly.
        // kill_on_drop keeps the long sleep from outliving the test that owns it.
        let child = tokio::process::Command::new("sleep")
            .arg("3600")
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .expect("failed to spawn dummy process");
        Self {
            job: executor::RunningJob::Managed {
                child,
                cgroup_path: None,
            },
            rootfs_mode: crate::container::RootfsMode::Extracted,
            stdout_path: "/dev/null".into(),
            stderr_path: "/dev/null".into(),
            has_pid_namespace: false,
            has_user_namespace: false,
            has_mount_namespace: false,
            _pty_master: None,
            work_dir: "/tmp".into(),
            uid: 0,
            gid: 0,
            user: "testuser".into(),
            partition: String::new(),
            gpu_devices: Vec::new(),
            cpus: 1,
            memory_mb: 0,
            nodelist: String::new(),
            mpi: String::new(),
            run_attempt: 0,
        }
    }
}

#[cfg(test)]
impl AgentService {
    async fn insert_test_job(&self, job_id: u32, job: TrackedJob) {
        self.running.lock().await.insert(job_id, job);
    }

    async fn free_gpu_count(&self) -> u32 {
        self.allocation.lock().await.free_gpus(None)
    }

    async fn register_test_step(&self, job_id: u32, step_id: u32, pid: Option<u32>) {
        self.active_steps.lock().await.insert(
            (job_id, step_id),
            ActiveStep {
                cancel_requested: false,
                pid,
                ..Default::default()
            },
        );
    }

    async fn step_cancel_requested(&self, job_id: u32, step_id: u32) -> bool {
        self.active_steps
            .lock()
            .await
            .get(&(job_id, step_id))
            .is_some_and(|step| step.cancel_requested)
    }

    async fn wait_for_active_step(&self, job_id: u32, step_id: u32) {
        for _ in 0..100 {
            if self
                .active_steps
                .lock()
                .await
                .contains_key(&(job_id, step_id))
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("step ({job_id}, {step_id}) was not registered");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_core::resource::ResourceSet;
    use tonic::Request;

    fn nsenter_job_entry(uid: u32, gid: u32) -> crate::job_entry::JobEntry {
        crate::job_entry::JobEntry {
            pid: 1234,
            has_pid_namespace: true,
            has_user_namespace: false,
            has_mount_namespace: true,
            uid,
            gid,
            work_dir: "/home/user".into(),
        }
    }

    #[test]
    fn build_nsenter_argv_non_root_wraps_with_setpriv_init_groups() {
        let entry = nsenter_job_entry(1000, 1000);
        let pd = crate::privdrop::PrivDrop::for_test(1000, 1000);
        let argv = build_nsenter_argv(&entry, Some(&pd), &["id".to_string()]);

        // nsenter itself must not carry uid/gid: it enters as root so it can
        // read /proc/<pid>/ns/*; priv drop happens inside via setpriv.
        assert!(
            !argv.iter().any(|a| a.starts_with("--setuid=")),
            "nsenter portion must not use --setuid: {argv:?}"
        );
        assert!(
            !argv.iter().any(|a| a.starts_with("--setgid=")),
            "nsenter portion must not use --setgid: {argv:?}"
        );

        let sep = argv.iter().position(|a| a == "--").expect("missing --");
        assert_eq!(
            &argv[sep..],
            &[
                "--",
                "setpriv",
                "--reuid=1000",
                "--regid=1000",
                "--init-groups",
                "--",
                "id"
            ]
        );
    }

    #[test]
    fn build_nsenter_argv_pty_shell_wraps_with_setpriv_init_groups() {
        // spawn_pty_in_job passes the resolved shell as the command.
        let entry = nsenter_job_entry(1000, 1000);
        let pd = crate::privdrop::PrivDrop::for_test(1000, 1000);
        let argv = build_nsenter_argv(&entry, Some(&pd), &["/bin/bash".to_string()]);

        assert!(
            !argv.iter().any(|a| a.starts_with("--setuid=")),
            "PTY nsenter portion must not use --setuid: {argv:?}"
        );
        assert!(
            !argv.iter().any(|a| a.starts_with("--setgid=")),
            "PTY nsenter portion must not use --setgid: {argv:?}"
        );
        let sep = argv.iter().position(|a| a == "--").expect("missing --");
        assert_eq!(
            &argv[sep..],
            &[
                "--",
                "setpriv",
                "--reuid=1000",
                "--regid=1000",
                "--init-groups",
                "--",
                "/bin/bash"
            ]
        );
    }

    #[test]
    fn build_nsenter_argv_root_job_runs_command_directly() {
        let entry = nsenter_job_entry(0, 0);
        // Root job: resolve_if_needed returns None → no setpriv prefix.
        let pd = crate::privdrop::PrivDrop::resolve_if_needed(0, 0);
        assert!(pd.is_none());
        let argv = build_nsenter_argv(&entry, pd.as_ref(), &["id".to_string()]);

        assert!(
            !argv.iter().any(|a| a == "setpriv"),
            "root job must not invoke setpriv: {argv:?}"
        );
        let sep = argv.iter().position(|a| a == "--").expect("missing --");
        assert_eq!(&argv[sep..], &["--", "id"]);
    }

    /// Rootless container (user namespace): entering must preserve credentials
    /// and skip setpriv, because setgroups() is forbidden in an unprivileged
    /// user namespace. The job user is already mapped inside, so no in-namespace
    /// drop is needed.
    #[test]
    fn build_nsenter_argv_rootless_container_preserves_credentials() {
        let mut entry = nsenter_job_entry(1000, 1000);
        entry.has_user_namespace = true;
        let pd = crate::privdrop::PrivDrop::for_test(1000, 1000);
        let argv = build_nsenter_argv(&entry, Some(&pd), &["id".to_string()]);

        assert!(
            argv.iter().any(|a| a == "--preserve-credentials"),
            "rootless container entry must preserve credentials: {argv:?}"
        );
        assert!(
            !argv.iter().any(|a| a == "setpriv"),
            "rootless container entry must not invoke setpriv (setgroups is \
             forbidden in a user namespace): {argv:?}"
        );
        assert!(
            !argv.iter().any(|a| a == "--init-groups"),
            "rootless container entry must not init groups: {argv:?}"
        );
        let sep = argv.iter().position(|a| a == "--").expect("missing --");
        assert_eq!(&argv[sep..], &["--", "id"]);
    }

    #[test]
    fn build_launch_plan_namespaced_job_uses_nsenter_no_child_drop() {
        let entry = nsenter_job_entry(1000, 1000);
        let pd = crate::privdrop::PrivDrop::for_test(1000, 1000);
        let plan = build_launch_plan(&entry, Some(&pd), &["id".to_string()]);

        assert_eq!(plan.program, "nsenter");
        // Privilege is dropped inside the namespace via setpriv, so the child
        // pre_exec hook must be skipped.
        assert!(!plan.apply_priv_in_child);
        assert_eq!(
            plan.args,
            build_nsenter_argv(&entry, Some(&pd), &["id".to_string()])
        );
    }

    #[test]
    fn build_launch_plan_no_namespaces_spawns_directly_with_child_drop() {
        let entry = crate::job_entry::JobEntry {
            pid: 0,
            has_pid_namespace: false,
            has_user_namespace: false,
            has_mount_namespace: false,
            uid: 1000,
            gid: 1000,
            work_dir: "/home/user".into(),
        };
        let pd = crate::privdrop::PrivDrop::for_test(1000, 1000);
        let plan = build_launch_plan(&entry, Some(&pd), &["echo".to_string(), "hi".to_string()]);

        // No namespaces → spawn the command directly and drop privilege in the
        // child via pre_exec.
        assert_eq!(plan.program, "echo");
        assert_eq!(plan.args, vec!["hi".to_string()]);
        assert!(plan.apply_priv_in_child);
    }

    #[tokio::test]
    async fn spawn_pty_in_job_direct_spawn_runs_command() {
        // Drives the real spawn_pty_in_job handler (not just the pure planner)
        // on the direct-spawn path: no namespaces, uid 0 so no privilege drop.
        // This exercises the build_launch_plan call site inside the handler.
        let entry = crate::job_entry::JobEntry {
            pid: 0,
            has_pid_namespace: false,
            has_user_namespace: false,
            has_mount_namespace: false,
            uid: 0,
            gid: 0,
            work_dir: "/tmp".into(),
        };
        let (master, mut child, pid) =
            AgentService::spawn_pty_in_job(&entry, &["true".to_string()], 7, None)
                .expect("spawn_pty_in_job should succeed for a direct /usr/bin/true");
        assert!(pid > 0);
        let status = child.wait().await.expect("child should be reapable");
        assert!(status.success(), "`true` should exit 0");
        drop(master);
    }

    #[test]
    fn build_launch_plan_namespaced_but_no_pid_spawns_directly() {
        // has_namespaces() is true but pid is 0 (no live process to enter):
        // fall back to a direct spawn rather than a broken nsenter.
        let entry = crate::job_entry::JobEntry {
            pid: 0,
            has_pid_namespace: true,
            has_user_namespace: false,
            has_mount_namespace: true,
            uid: 1000,
            gid: 1000,
            work_dir: "/home/user".into(),
        };
        let pd = crate::privdrop::PrivDrop::for_test(1000, 1000);
        let plan = build_launch_plan(&entry, Some(&pd), &["id".to_string()]);

        assert_eq!(plan.program, "id");
        assert!(plan.args.is_empty());
        assert!(plan.apply_priv_in_child);
    }

    #[test]
    fn build_job_script_uses_explicit_script_verbatim() {
        let s = build_job_script("#!/bin/sh\nmake -j4\n", &[], &[]).unwrap();
        assert_eq!(s, "#!/bin/sh\nmake -j4\n");
    }

    #[test]
    fn build_job_script_errors_on_empty() {
        assert!(build_job_script("", &[], &[]).is_err());
    }

    #[test]
    fn build_job_script_escapes_argv_so_redirect_stays_in_arg() {
        let argv: Vec<String> = [
            "axis",
            "run",
            "--policy",
            "p.yaml",
            "--",
            "bash",
            "-c",
            "echo pwned > /tmp/out.txt",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let s = build_job_script("", &argv, &[]).unwrap();
        let cmd = s.strip_prefix("#!/bin/bash\n").unwrap().trim_end();
        let reparsed = shlex::split(cmd).expect("generated command must be shell-parseable");
        assert_eq!(reparsed, argv);
    }

    #[test]
    fn build_job_script_simple_argv_round_trips() {
        let argv: Vec<String> = ["echo", "hello"].iter().map(|s| s.to_string()).collect();
        let s = build_job_script("", &argv, &[]).unwrap();
        assert_eq!(s, "#!/bin/bash\necho hello\n");
    }

    #[test]
    fn build_job_script_injects_args_after_shebang() {
        let args: Vec<String> = ["uuid-123", "--flag"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let s = build_job_script("#!/bin/bash\necho hello\n", &[], &args).unwrap();
        assert_eq!(s, "#!/bin/bash\nset -- uuid-123 --flag\necho hello\n");
    }

    #[test]
    fn build_job_script_injects_args_no_shebang() {
        let args: Vec<String> = ["arg1"].iter().map(|s| s.to_string()).collect();
        let s = build_job_script("echo $1\n", &[], &args).unwrap();
        assert_eq!(s, "set -- arg1\necho $1\n");
    }

    #[test]
    fn build_job_script_injects_args_with_env_shebang() {
        let args: Vec<String> = ["a", "b c"].iter().map(|s| s.to_string()).collect();
        let s = build_job_script("#!/usr/bin/env bash\necho $@\n", &[], &args).unwrap();
        assert_eq!(s, "#!/usr/bin/env bash\nset -- a 'b c'\necho $@\n");
    }

    #[test]
    fn build_job_script_injects_args_crlf_shebang() {
        let args: Vec<String> = ["x"].iter().map(|s| s.to_string()).collect();
        let s = build_job_script("#!/bin/bash\r\necho hi\n", &[], &args).unwrap();
        assert_eq!(s, "#!/bin/bash\nset -- x\necho hi\n");
    }

    async fn run_command_test_setup() -> (AgentService, u32) {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        // A unique job_id per test so each gets its own step spool dir
        // (temp/spur/job<id>/step0.out) and parallel tests do not clobber one
        // another's step output. Production step_ids are unique per job via
        // create_job_step; only these fixed-id tests would otherwise collide.
        static NEXT_JOB_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(100);
        let job_id = NEXT_JOB_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        svc.insert_test_job(job_id, TrackedJob::dummy(0)).await;
        (svc, job_id)
    }

    fn test_gpu_registry() -> DeviceRegistry {
        use spur_devices::cdi::cache::CdiCache;
        use spur_devices::{GresCache, GresEntry};

        let gres = vec![GresEntry {
            name: "gpu".into(),
            r#type: Some("mi300x".into()),
            file: Some("/dev/dri/renderD[128-129]".into()),
            count: Some(2),
            flags: vec!["amd_gpu_env".into()],
            ..Default::default()
        }];
        let gres_cache = GresCache::from_entries(&gres);
        let mut reg = DeviceRegistry::new();
        reg.populate(&CdiCache::new(), &gres_cache);
        reg
    }

    fn test_reporter() -> Arc<NodeReporter> {
        Arc::new(NodeReporter::new(
            "test-node".into(),
            "http://localhost:6817".into(),
            ResourceSet {
                cpus: 4,
                memory_mb: 8192,
                ..Default::default()
            },
            spur_net::NodeAddress {
                ip: "127.0.0.1".into(),
                hostname: "test-node".into(),
                port: 6818,
                source: spur_net::AddressSource::Static,
            },
            std::collections::HashMap::new(),
            String::new(),
            String::new(),
            new_running_jobs(),
        ))
    }

    /// An executable script at a temp path that exits with `code`.
    fn failing_hook_script(code: i32) -> tempfile::TempPath {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "#!/bin/bash\nexit {code}").unwrap();
        let path = f.into_temp_path();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// The refusal driven through the real RPC entry point, not just the helper: a launch asking to
    /// run as root on a root spurd must be denied before anything is spawned. `with_root_override`
    /// makes this deterministic on an unprivileged runner, where the guard would otherwise be inert.
    #[tokio::test]
    async fn launch_job_refuses_uid_zero_when_spurd_is_root_and_not_opted_in() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        )
        .with_root_override(true);

        let err = svc
            .launch_job(Request::new(LaunchJobRequest {
                job_id: 4242,
                spec: Some(JobSpec {
                    // uid 0 is the default, but state it so the test's subject is unmissable.
                    uid: 0,
                    name: "root-job".into(),
                    script: "#!/bin/bash\ntrue\n".into(),
                    num_tasks: 1,
                    num_nodes: 1,
                    cpus_per_task: 1,
                    work_dir: std::env::temp_dir().to_string_lossy().into_owned(),
                    ..Default::default()
                }),
                ..Default::default()
            }))
            .await
            .expect_err("a uid-0 launch must be refused, not accepted");

        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        assert!(
            err.message().contains("allow_root_jobs"),
            "the refusal should tell the operator which option governs it: {}",
            err.message()
        );
    }

    /// The same launch is accepted once the operator opts in, so the test above is measuring the
    /// policy rather than some unrelated rejection.
    #[tokio::test]
    async fn launch_job_permits_uid_zero_when_opted_in() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        )
        .with_root_override(true);
        let svc = AgentService {
            allow_root_jobs: true,
            ..svc
        };

        let resp = svc
            .launch_job(Request::new(LaunchJobRequest {
                job_id: 4243,
                spec: Some(JobSpec {
                    uid: 0,
                    name: "root-job".into(),
                    script: "#!/bin/bash\ntrue\n".into(),
                    num_tasks: 1,
                    num_nodes: 1,
                    cpus_per_task: 1,
                    work_dir: std::env::temp_dir().to_string_lossy().into_owned(),
                    ..Default::default()
                }),
                ..Default::default()
            }))
            .await;
        assert!(
            resp.is_ok(),
            "with allow_root_jobs the guard must not reject: {resp:?}"
        );
    }

    #[tokio::test]
    async fn a_failed_prolog_is_reported_to_the_controller_as_a_prolog_failure() {
        // The controller drains and holds on this kind, so the launch must come
        // back classified rather than as an opaque rejection the controller can
        // only string-match. The agent itself neither drains nor reports a
        // completion: pairing the drain with the hold is the controller's job.
        let prolog = failing_hook_script(1);
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig {
                prolog: Some(prolog.to_str().unwrap().to_string()),
                ..Default::default()
            },
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        let resp = svc
            .launch_job(Request::new(LaunchJobRequest {
                job_id: 7,
                spec: Some(JobSpec {
                    name: "prolog-fail".into(),
                    script: "#!/bin/bash\ntrue\n".into(),
                    num_tasks: 1,
                    num_nodes: 1,
                    cpus_per_task: 1,
                    work_dir: std::env::temp_dir().to_string_lossy().into_owned(),
                    ..Default::default()
                }),
                ..Default::default()
            }))
            .await
            .expect("a prolog failure is a launch outcome, not a transport error")
            .into_inner();

        assert!(!resp.success);
        assert_eq!(
            resp.failure_kind,
            LaunchFailureKind::LaunchFailureProlog as i32
        );
        assert!(
            resp.error.contains("prolog_slurmd script exited with"),
            "the operator needs the script's own failure, got {:?}",
            resp.error
        );
    }

    #[tokio::test]
    async fn a_prolog_that_cannot_even_start_reports_the_underlying_errno() {
        // Issue 520: `{e}` renders only the outermost context, reducing this to
        // "prolog_slurmd script failed to execute: ..." and dropping the errno
        // that says whether the script is missing, unreadable or not executable.
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig {
                prolog: Some("/nonexistent/prolog.sh".into()),
                ..Default::default()
            },
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        let resp = svc
            .launch_job(Request::new(LaunchJobRequest {
                job_id: 8,
                spec: Some(JobSpec {
                    name: "prolog-missing".into(),
                    script: "#!/bin/bash\ntrue\n".into(),
                    num_tasks: 1,
                    num_nodes: 1,
                    cpus_per_task: 1,
                    work_dir: std::env::temp_dir().to_string_lossy().into_owned(),
                    ..Default::default()
                }),
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner();

        assert!(!resp.success);
        assert_eq!(
            resp.failure_kind,
            LaunchFailureKind::LaunchFailureProlog as i32
        );
        assert!(
            resp.error.contains("No such file or directory"),
            "the cause chain must survive into the reported error, got {:?}",
            resp.error
        );
    }

    #[tokio::test]
    async fn exec_in_job_returns_without_deadlock() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        let pid = std::process::id();
        svc.insert_test_job(42, TrackedJob::dummy(pid)).await;

        let req = Request::new(ExecInJobRequest {
            job_id: 42,
            command: vec!["echo".into(), "hello".into()],
            user: "testuser".into(),
        });

        let result = svc.exec_in_job(req).await;
        assert!(result.is_ok() || result.is_err());
    }

    #[tokio::test]
    async fn exec_in_job_not_found() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        let req = Request::new(ExecInJobRequest {
            job_id: 999,
            command: vec!["echo".into()],
            user: "testuser".into(),
        });

        let err = svc.exec_in_job(req).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn exec_in_job_rejects_non_owner() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        svc.insert_test_job(43, TrackedJob::dummy(std::process::id()))
            .await;

        let err = svc
            .exec_in_job(Request::new(ExecInJobRequest {
                job_id: 43,
                command: vec!["whoami".into()],
                user: "intruder".into(),
            }))
            .await
            .expect_err("a non-owner must not exec inside another user's job");

        assert_eq!(err.code(), tonic::Code::PermissionDenied);
    }

    #[tokio::test]
    async fn stream_job_output_tails_step_spool_file() {
        use tokio_stream::StreamExt as _;
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        let job_id = 77;
        svc.insert_test_job(job_id, TrackedJob::dummy(std::process::id()))
            .await;

        // A step whose spool file already holds some output, recorded as an
        // active step (as run_command would).
        let step_id = 5;
        let dir = std::env::temp_dir().join(format!("spur-stream-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("step5.out");
        std::fs::write(&path, b"part1\n").unwrap();
        svc.active_steps.lock().await.insert(
            (job_id, step_id),
            ActiveStep {
                stdout_path: path.to_string_lossy().into_owned(),
                ..Default::default()
            },
        );

        let mut stream = svc
            .stream_job_output(Request::new(StreamJobOutputRequest {
                job_id,
                step_id,
                stream: "stdout".into(),
                user: "testuser".into(),
            }))
            .await
            .unwrap()
            .into_inner();

        // The already-written bytes stream out before the step ends.
        let first = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
            .await
            .expect("a chunk should arrive")
            .expect("stream still open")
            .unwrap();
        assert_eq!(first.data, b"part1\n");
        assert!(!first.eof);

        // More output appears, then the step finishes.
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(b"part2\n").unwrap();
        }
        svc.active_steps.lock().await.remove(&(job_id, step_id));

        let mut rest = Vec::new();
        loop {
            let chunk = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
                .await
                .expect("a chunk should arrive")
                .expect("stream still open")
                .unwrap();
            if chunk.eof {
                break;
            }
            rest.extend_from_slice(&chunk.data);
        }
        assert_eq!(rest, b"part2\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stream_job_output_rejects_non_owner() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        svc.insert_test_job(44, TrackedJob::dummy(std::process::id()))
            .await;

        let err = svc
            .stream_job_output(Request::new(StreamJobOutputRequest {
                job_id: 44,
                stream: "stdout".into(),
                user: "intruder".into(),
                ..Default::default()
            }))
            .await
            .expect_err("a non-owner must not read another user's job output");

        assert_eq!(err.code(), tonic::Code::PermissionDenied);
    }

    #[tokio::test]
    async fn exec_in_job_allows_owner() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        svc.insert_test_job(45, TrackedJob::dummy(std::process::id()))
            .await;

        // The owner clears the gate; the exec itself may still fail in a test
        // sandbox, so only the absence of PermissionDenied is asserted.
        let code = svc
            .exec_in_job(Request::new(ExecInJobRequest {
                job_id: 45,
                command: vec!["echo".into(), "hello".into()],
                user: "testuser".into(),
            }))
            .await
            .err()
            .map(|e| e.code());

        assert_ne!(code, Some(tonic::Code::PermissionDenied));
    }

    fn user_identity(name: &str) -> spur_core::auth::Identity {
        spur_core::auth::Identity {
            user: name.into(),
            uid: 1000,
            gid: 1000,
            is_admin: false,
        }
    }

    fn controller_identity() -> spur_core::auth::Identity {
        spur_core::auth::Identity {
            user: spur_core::auth::CONTROLLER_SUBJECT.into(),
            uid: 0,
            gid: 0,
            is_admin: true,
        }
    }

    /// `interactive_session` (the `sattach` and `srun --pty` path) gates on
    /// `check_job_access`, but its handler consumes a gRPC stream that cannot be
    /// built in-process, so the gate is exercised directly here.
    #[tokio::test]
    async fn check_job_access_gates_attach_by_owner() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        svc.insert_test_job(46, TrackedJob::dummy(std::process::id()))
            .await;

        // Verified owner and a verified admin (the controller mints an admin credential) are allowed.
        svc.check_job_access(46, Some(&user_identity("testuser")), "", "attach to")
            .await
            .expect("the owner must be allowed to attach");
        svc.check_job_access(46, Some(&controller_identity()), "", "attach to")
            .await
            .expect("an admin/controller is an override");

        // A verified non-owner is refused even if it claims the owner's name on the wire — the
        // verified identity wins over the asserted `user`.
        let err = svc
            .check_job_access(
                46,
                Some(&user_identity("intruder")),
                "testuser",
                "attach to",
            )
            .await
            .expect_err("a non-owner must not attach to another user's job");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);

        // With no verified identity (permissive/disabled) the asserted user is trusted only as a
        // plain principal: the owner's name clears, another user's does not.
        svc.check_job_access(46, None, "testuser", "attach to")
            .await
            .expect("the asserted owner clears under permissive");
        let err = svc
            .check_job_access(46, None, "intruder", "attach to")
            .await
            .expect_err("an asserted non-owner must be denied");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);

        let missing = svc
            .check_job_access(999, Some(&user_identity("testuser")), "", "attach to")
            .await
            .expect_err("an untracked job must report not-found");
        assert_eq!(missing.code(), tonic::Code::NotFound);
    }

    /// An empty-owner job runs as root, so only an internal caller (a verified admin/controller) is
    /// allowed — an empty or `"root"` string no longer stands in for one.
    #[tokio::test]
    async fn check_job_access_denies_non_root_on_empty_owner() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        let mut job = TrackedJob::dummy(std::process::id());
        job.user = String::new();
        svc.insert_test_job(47, job).await;

        // A verified admin/controller reaches the root-owned job.
        svc.check_job_access(47, Some(&controller_identity()), "", "attach to")
            .await
            .expect("an admin/controller must reach a root-owned job");

        // A verified non-admin user must not.
        let err = svc
            .check_job_access(47, Some(&user_identity("alice")), "", "attach to")
            .await
            .expect_err("empty-owner jobs run as root; a named user must be denied");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);

        // The removed bypass: an empty or literal-"root" asserted user (no verified identity) is now
        // rejected instead of silently authorized.
        for forged in ["", "root"] {
            let err = svc
                .check_job_access(47, None, forged, "attach to")
                .await
                .expect_err("empty/root string must not bypass the ownership check");
            assert_eq!(err.code(), tonic::Code::PermissionDenied);
        }
    }

    /// The controller gate on the controller-only RPCs: a verified user token is refused, the
    /// controller's own credential passes, and an unauthenticated caller is left to the permissive
    /// path (no identity to check).
    #[test]
    fn require_controller_admits_only_the_controller() {
        let mut user_req = Request::new(());
        user_req.extensions_mut().insert(user_identity("attacker"));
        let err = AgentService::require_controller(&user_req)
            .expect_err("a plain user credential must not drive a controller-only RPC");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);

        let mut ctl_req = Request::new(());
        ctl_req.extensions_mut().insert(controller_identity());
        AgentService::require_controller(&ctl_req)
            .expect("the controller's own credential must pass");

        let anon_req = Request::new(());
        AgentService::require_controller(&anon_req)
            .expect("no credential is tolerated (permissive/disabled)");
    }

    /// End-to-end: a verified *user* identity in the request extensions cannot cancel a job through
    /// the agent — the controller-only gate refuses it before any signal is sent.
    #[tokio::test]
    async fn cancel_job_rejects_a_non_controller_caller() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        svc.insert_test_job(48, TrackedJob::dummy(std::process::id()))
            .await;

        let mut req = Request::new(AgentCancelJobRequest {
            job_id: 48,
            signal: 9,
        });
        req.extensions_mut().insert(user_identity("attacker"));
        let err = svc
            .cancel_job(req)
            .await
            .expect_err("a user token must not cancel jobs by dialing the agent directly");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
    }

    /// A launch aimed at another node's name must be refused: the agent only runs allocations
    /// scheduled onto its own host.
    #[tokio::test]
    async fn launch_job_rejects_a_foreign_target_node() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        let err = svc
            .launch_job(Request::new(LaunchJobRequest {
                job_id: 7,
                target_node: "some-other-node".into(),
                spec: Some(JobSpec {
                    uid: 1000,
                    name: "j".into(),
                    script: "#!/bin/bash\ntrue\n".into(),
                    num_tasks: 1,
                    num_nodes: 1,
                    cpus_per_task: 1,
                    work_dir: std::env::temp_dir().to_string_lossy().into_owned(),
                    ..Default::default()
                }),
                ..Default::default()
            }))
            .await
            .expect_err("a launch targeting another node must be refused");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn run_command_executes_simple_command() {
        let (svc, job_id) = run_command_test_setup().await;
        let req = Request::new(RunCommandRequest {
            command: vec!["echo".into(), "hello-from-agent".into()],
            uid: 0,
            gid: 0,
            work_dir: String::new(),
            environment: HashMap::new(),
            job_id,
            ..Default::default()
        });
        let resp = svc.run_command(req).await.unwrap().into_inner();
        assert_eq!(resp.exit_code, 0);
        assert_eq!(resp.stdout.trim(), "hello-from-agent");
        assert!(resp.stderr.is_empty());
    }

    #[tokio::test]
    async fn run_command_propagates_nonzero_exit_code() {
        let (svc, job_id) = run_command_test_setup().await;
        let req = Request::new(RunCommandRequest {
            command: vec!["false".into()],
            uid: 0,
            gid: 0,
            work_dir: String::new(),
            environment: HashMap::new(),
            job_id,
            ..Default::default()
        });
        let resp = svc.run_command(req).await.unwrap().into_inner();
        assert_eq!(resp.exit_code, 1, "false exits 1");
    }

    #[tokio::test]
    async fn run_command_passes_environment() {
        let (svc, job_id) = run_command_test_setup().await;
        let mut env = HashMap::new();
        env.insert("SPUR_TEST_VAR".into(), "step-dispatched".into());
        let req = Request::new(RunCommandRequest {
            command: vec!["/bin/sh".into(), "-c".into(), "echo $SPUR_TEST_VAR".into()],
            uid: 0,
            gid: 0,
            work_dir: String::new(),
            environment: env,
            job_id,
            ..Default::default()
        });
        let resp = svc.run_command(req).await.unwrap().into_inner();
        assert_eq!(resp.exit_code, 0);
        assert_eq!(resp.stdout.trim(), "step-dispatched");
    }

    #[tokio::test]
    async fn run_command_empty_command_is_rejected() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        let req = Request::new(RunCommandRequest {
            command: vec![],
            uid: 0,
            gid: 0,
            work_dir: String::new(),
            environment: HashMap::new(),
            job_id: 0,
            ..Default::default()
        });
        let err = svc.run_command(req).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn run_command_requires_job_id() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        let req = Request::new(RunCommandRequest {
            command: vec!["echo".into(), "hi".into()],
            uid: 0,
            gid: 0,
            work_dir: String::new(),
            environment: HashMap::new(),
            job_id: 0,
            ..Default::default()
        });
        let err = svc.run_command(req).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn run_command_not_found_without_tracked_job() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        let req = Request::new(RunCommandRequest {
            command: vec!["echo".into(), "hi".into()],
            uid: 0,
            gid: 0,
            work_dir: String::new(),
            environment: HashMap::new(),
            job_id: 999,
            ..Default::default()
        });
        let err = svc.run_command(req).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    /// Regression for #777: a step carrying `container_image` must take the
    /// container path, not silently run the command on the host. A bogus image
    /// makes the container path fail at image resolution *before* the command
    /// would run. The command (`echo`) would exit 0 on the host, so an Ok result
    /// here would mean the flags were dropped — the exact bug being fixed.
    #[tokio::test]
    async fn run_command_with_container_image_takes_container_path_not_host() {
        let (svc, job_id) = run_command_test_setup().await;
        let req = Request::new(RunCommandRequest {
            command: vec!["echo".into(), "would-succeed-on-host".into()],
            uid: 0,
            gid: 0,
            work_dir: String::new(),
            environment: HashMap::new(),
            job_id,
            container: Some(spur_proto::proto::ContainerSpec {
                image: "/nonexistent/bogus-step-image.sqsh".into(),
                ..Default::default()
            }),
            ..Default::default()
        });
        let err = svc
            .run_command(req)
            .await
            .expect_err("a container_image step must not fall through to a host echo");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(
            err.message().contains("not found"),
            "expected an image-resolution failure, got: {}",
            err.message()
        );
    }

    /// The parent-container (nsenter) path takes precedence over setting up a
    /// fresh container: a step whose tracked job already has live namespaces must
    /// enter them, never call `resolve_image`. Proven by the *absence* of the
    /// image-not-found error even though a bogus image is supplied.
    #[tokio::test]
    async fn run_command_enters_parent_namespaces_before_new_container() {
        let (svc, job_id) = run_command_test_setup().await;
        {
            let mut jobs = svc.running.lock().await;
            let job = jobs.get_mut(&job_id).unwrap();
            job.has_pid_namespace = true;
            job.has_mount_namespace = true;
        }
        let req = Request::new(RunCommandRequest {
            command: vec!["echo".into(), "hi".into()],
            uid: 0,
            gid: 0,
            work_dir: String::new(),
            environment: HashMap::new(),
            job_id,
            container: Some(spur_proto::proto::ContainerSpec {
                image: "/nonexistent/bogus-step-image.sqsh".into(),
                ..Default::default()
            }),
            ..Default::default()
        });
        // The nsenter path was taken, not Case 2: the bogus image is never
        // resolved. On a typical (unprivileged) test runner setns is denied, so
        // the step comes back as a run with an nsenter trace on stderr — the
        // observable branch — rather than the "image not found" resolution error.
        match svc.run_command(req).await {
            Ok(resp) => {
                let r = resp.into_inner();
                assert!(
                    !r.stdout.contains("not found in [") && !r.stderr.contains("not found in ["),
                    "namespaced job must nsenter, not resolve a new image: {r:?}"
                );
                assert!(
                    r.exit_code == 0
                        || r.stderr.to_lowercase().contains("nsenter")
                        || r.stderr.contains("Operation not permitted")
                        || !r.stderr.is_empty(),
                    "expected the nsenter path (success, or an nsenter/permission \
                     failure on an unprivileged runner), got: {r:?}"
                );
            }
            Err(err) => {
                // A spawn error (e.g. no nsenter binary) is acceptable, as long as
                // it is not image resolution — that would mean Case 2 was entered.
                assert!(
                    !err.message().contains("not found in ["),
                    "namespaced job should nsenter, not resolve a new image: {}",
                    err.message()
                );
            }
        }
    }

    /// Without a container_image and without namespaces, the step runs directly
    /// on the host (Case 3) — the flag is what gates the container path.
    #[tokio::test]
    async fn run_command_without_container_image_runs_on_host() {
        let (svc, job_id) = run_command_test_setup().await;
        let req = Request::new(RunCommandRequest {
            command: vec!["echo".into(), "host-step".into()],
            uid: 0,
            gid: 0,
            work_dir: String::new(),
            environment: HashMap::new(),
            job_id,
            ..Default::default()
        });
        let resp = svc.run_command(req).await.unwrap().into_inner();
        assert_eq!(resp.exit_code, 0);
        assert_eq!(resp.stdout.trim(), "host-step");
    }

    /// Cancelling an allocation-only job (standalone srun / salloc) must signal
    /// its in-flight steps, not just drop the tracked allocation. Otherwise a
    /// containerized step orphans its container and leaks its rootfs. Uses a real
    /// child in its own process group so the group-targeted signal is observable.
    #[tokio::test]
    async fn cancelling_allocation_only_job_signals_inflight_steps() {
        use std::os::unix::process::CommandExt;
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        svc.register_job_allocation(Request::new(RegisterJobAllocationRequest {
            job_id: 77,
            cpus: 1,
            ..Default::default()
        }))
        .await
        .expect("register allocation");

        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("300");
        cmd.process_group(0);
        let mut child = cmd.spawn().expect("spawn sleep");
        svc.register_test_step(77, 0, Some(child.id())).await;

        svc.graceful_cancel(77).await;

        // The step must be marked cancelled and its process signaled dead.
        {
            let steps = svc.active_steps.lock().await;
            assert!(
                steps.get(&(77, 0)).unwrap().cancel_requested,
                "step should be marked cancel_requested"
            );
        }
        // try_wait both reaps the child and reports its exit, avoiding the
        // zombie-looks-alive trap of a signal-0 probe.
        let mut exited = false;
        for _ in 0..50 {
            if child.try_wait().expect("try_wait").is_some() {
                exited = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        if !exited {
            let _ = child.kill();
            let _ = child.wait();
        }
        assert!(exited, "the in-flight step process must be signaled dead");
    }

    #[tokio::test]
    async fn run_command_uses_provided_work_dir() {
        // The bug repro: the user's workflow is `salloc; srun hostname`.
        // hostname runs in whatever cwd the agent picks; we can't easily
        // assert it's a specific directory without mounting a tempdir as
        // the agent's cwd. Instead use `pwd` and assert it matches the
        // dir we passed.
        let (svc, job_id) = run_command_test_setup().await;
        let tmp = std::env::temp_dir();
        // Resolve symlinks (e.g., macOS /tmp -> /private/tmp).
        let tmp_canonical = std::fs::canonicalize(&tmp).unwrap_or(tmp.clone());
        let req = Request::new(RunCommandRequest {
            command: vec!["pwd".into()],
            uid: 0,
            gid: 0,
            work_dir: tmp_canonical.to_string_lossy().into_owned(),
            environment: HashMap::new(),
            job_id,
            ..Default::default()
        });
        let resp = svc.run_command(req).await.unwrap().into_inner();
        assert_eq!(resp.exit_code, 0);
        let observed_canonical = std::fs::canonicalize(resp.stdout.trim()).unwrap();
        assert_eq!(observed_canonical, tmp_canonical);
    }

    #[tokio::test]
    async fn run_command_writes_step_output_to_spool_file() {
        let (svc, job_id) = run_command_test_setup().await;
        let req = Request::new(RunCommandRequest {
            command: vec!["echo".into(), "spooled-marker".into()],
            uid: 0,
            gid: 0,
            job_id,
            ..Default::default()
        });
        let resp = svc.run_command(req).await.unwrap().into_inner();
        assert_eq!(resp.exit_code, 0);
        // The step's stdout must live in a spool file the agent can tail, not
        // only in the RPC response — this file is what StreamJobOutput follows
        // (#781). step_id defaults to 0 here, so the file is step0.out.
        let contents = [
            std::path::PathBuf::from("/var/spool/spur"),
            std::env::temp_dir().join("spur"),
        ]
        .iter()
        .find_map(|base| {
            std::fs::read_to_string(base.join(format!("job{job_id}")).join("step0.out")).ok()
        })
        .expect("step stdout spool file should exist on disk");
        assert_eq!(contents.trim(), "spooled-marker");
    }

    #[tokio::test]
    async fn cancel_step_sets_flag_when_step_has_no_pid() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        svc.register_test_step(10, 1, None).await;
        svc.cancel_step(Request::new(CancelStepRequest {
            job_id: 10,
            step_id: 1,
            signal: 0,
        }))
        .await
        .unwrap();
        assert!(svc.step_cancel_requested(10, 1).await);
    }

    #[tokio::test]
    async fn cancel_step_before_spawn_aborts_run_command() {
        use std::sync::Arc;
        use std::time::Duration;

        let (svc, job_id) = run_command_test_setup().await;
        let svc = Arc::new(svc);
        let step_id = 3;
        let svc_run = svc.clone();
        let run_handle = tokio::spawn(async move {
            svc_run
                .run_command(Request::new(RunCommandRequest {
                    command: vec!["sleep".into(), "60".into()],
                    uid: 0,
                    gid: 0,
                    work_dir: String::new(),
                    environment: HashMap::new(),
                    job_id,
                    step_id,
                    ..Default::default()
                }))
                .await
        });

        svc.wait_for_active_step(job_id, step_id).await;
        svc.cancel_step(Request::new(CancelStepRequest {
            job_id,
            step_id,
            signal: 0,
        }))
        .await
        .unwrap();

        let resp = tokio::time::timeout(Duration::from_secs(5), run_handle)
            .await
            .expect("run_command did not finish after CancelStep")
            .unwrap()
            .unwrap()
            .into_inner();
        let sigterm_exit = 128 + nix::sys::signal::Signal::SIGTERM as i32;
        assert!(
            resp.stderr == "step cancelled" || resp.exit_code == sigterm_exit,
            "expected cancelled step, got stderr={:?} exit={}",
            resp.stderr,
            resp.exit_code
        );
    }

    #[tokio::test]
    async fn cancel_step_kills_registered_step_process_group() {
        use std::time::Duration;

        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        let job_id = 1;
        let step_id = 2;
        let mut child = tokio::process::Command::new("bash")
            .arg("-c")
            .arg("sleep 3600 & sleep 3600 & wait")
            .process_group(0)
            .spawn()
            .expect("failed to spawn process group");
        let pid = child.id().expect("spawned child should have pid");
        svc.register_test_step(job_id, step_id, Some(pid)).await;

        svc.cancel_step(Request::new(CancelStepRequest {
            job_id,
            step_id,
            signal: 0,
        }))
        .await
        .unwrap();

        let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("process group did not exit after CancelStep")
            .expect("wait failed");
        assert!(!status.success());
        assert!(svc.step_cancel_requested(job_id, step_id).await);
    }

    #[tokio::test]
    async fn signal_step_tree_terminates_child_processes() {
        use std::time::Duration;

        let mut child = tokio::process::Command::new("bash")
            .arg("-c")
            .arg("sleep 3600 & sleep 3600 & wait")
            .process_group(0)
            .spawn()
            .expect("failed to spawn process group");
        let pid = child.id().expect("spawned child should have pid");
        tokio::time::sleep(Duration::from_millis(50)).await;

        signal_step_tree(pid, nix::sys::signal::Signal::SIGTERM as i32);

        let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("process group did not exit after signal")
            .expect("wait failed");
        assert!(!status.success());
    }

    // G1: the enforced budget — and therefore the cpuset — must cover every task
    // the controller placed here, not just one task's `--cpus-per-task`.
    #[tokio::test]
    async fn cpuset_covers_every_task_on_the_node() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        // 2 tasks x 2 cpus each, on the 4-cpu test node.
        let spec = JobSpec {
            cpus_per_task: 2,
            tasks_per_node: 2,
            ..Default::default()
        };
        let (cpus, memory_mb) = resolve_cgroup_budget(None, &spec, 2);
        assert_eq!(cpus, 4, "budget must count both tasks");

        let (result, _) = svc
            .allocate_local_resources(7, &spec, None, cpus, memory_mb)
            .await
            .expect("allocation succeeds");
        assert_eq!(result.cpu_ids, vec![0, 1, 2, 3]);
    }

    // The allocation is authoritative even when it disagrees with the spec.
    #[tokio::test]
    async fn cpuset_follows_the_controller_allocation_over_the_spec() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        let spec = JobSpec {
            cpus_per_task: 1,
            ..Default::default()
        };
        let allocated = ResourceAllocations {
            cpus: 3,
            memory_mb: 2048,
            ..Default::default()
        };
        let (cpus, memory_mb) = resolve_cgroup_budget(Some(&allocated), &spec, 1);
        assert_eq!((cpus, memory_mb), (3, 2048));

        let (result, _) = svc
            .allocate_local_resources(7, &spec, Some(&allocated), cpus, memory_mb)
            .await
            .expect("allocation succeeds");
        assert_eq!(result.cpu_ids, vec![0, 1, 2]);
        assert_eq!(result.memory_mb, 2048);
    }

    fn test_reporter_with_gpus(device_ids: &[u32]) -> Arc<NodeReporter> {
        use spur_core::resource::{GpuLinkType, GpuResource};
        let gpus = device_ids
            .iter()
            .map(|&device_id| GpuResource {
                device_id,
                gpu_type: "mi300x".into(),
                memory_mb: 192_000,
                peer_gpus: vec![],
                link_type: GpuLinkType::XGMI,
            })
            .collect();
        Arc::new(NodeReporter::new(
            "test-node".into(),
            "http://localhost:6817".into(),
            ResourceSet {
                cpus: 4,
                memory_mb: 8192,
                gpus,
                ..Default::default()
            },
            spur_net::NodeAddress {
                ip: "127.0.0.1".into(),
                hostname: "test-node".into(),
                port: 6818,
                source: spur_net::AddressSource::Static,
            },
            std::collections::HashMap::new(),
            String::new(),
            "spur0".into(),
            new_running_jobs(),
        ))
    }

    // A dispatch that records GPUs but fails before the job is
    // tracked (here: device-registry resolution fails) must release those GPUs.
    // Otherwise the node keeps rejecting every future dispatch ("controller-
    // allocated GPUs unavailable") while the controller still sees it IDLE,
    // stranding the node until spurd restart -> JobHoldMaxRequeue.
    #[tokio::test]
    async fn launch_failure_after_gpu_record_releases_allocation() {
        // Reporter advertises GPU device_id 0 so allocate_for_job succeeds, but the
        // device registry is empty so build_job_injection_plans fails.
        let svc = AgentService::new(
            test_reporter_with_gpus(&[0]),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        assert_eq!(svc.free_gpu_count().await, 1);

        let mut devices = std::collections::HashMap::new();
        devices.insert(
            "gpu".to_string(),
            DeviceAllocations {
                devices: vec![AllocatedDevice {
                    device_id: 0,
                    count: 1,
                }],
            },
        );

        let req = Request::new(LaunchJobRequest {
            job_id: 65,
            spec: Some(JobSpec {
                script: "#!/bin/sh\ntrue\n".into(),
                cpus_per_task: 1,
                gres: vec!["gpu:1".into()],
                ..Default::default()
            }),
            allocated: Some(ResourceAllocations {
                cpus: 1,
                memory_mb: 0,
                devices,
            }),
            ..Default::default()
        });

        let result = svc.launch_job(req).await;
        assert!(
            result.is_err(),
            "expected launch to fail on registry resolution"
        );

        assert_eq!(
            svc.free_gpu_count().await,
            1,
            "GPU allocation must be released after a post-record launch failure"
        );
    }

    // A successful launch must report the real resolved output path back so the
    // controller can surface where output landed. With an empty stdout_path the
    // agent defaults to spur-<id>.out anchored to the job's work_dir.
    #[tokio::test]
    async fn launch_reports_resolved_output_paths() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        let work_dir = tempfile::tempdir().unwrap();
        let work_dir_str = work_dir.path().to_string_lossy().to_string();

        let req = Request::new(LaunchJobRequest {
            job_id: 77,
            spec: Some(JobSpec {
                script: "#!/bin/sh\ntrue\n".into(),
                cpus_per_task: 1,
                work_dir: work_dir_str.clone(),
                ..Default::default()
            }),
            allocated: Some(ResourceAllocations {
                cpus: 1,
                memory_mb: 0,
                devices: std::collections::HashMap::new(),
            }),
            ..Default::default()
        });

        let resp = svc.launch_job(req).await.expect("launch should succeed");
        let inner = resp.into_inner();
        assert!(inner.success, "launch failed: {}", inner.error);
        let expected = format!("{}/spur-77.out", work_dir_str);
        assert_eq!(inner.stdout_path, expected);
        assert_eq!(inner.stderr_path, expected);
    }

    /// Poll `path` until its content stabilizes (unchanged across two
    /// consecutive checks) or `timeout_ms` elapses, then return it. Used to
    /// wait out a script's execution(s) without depending on job-completion
    /// reporting to a controller (which these unit tests don't run).
    async fn wait_for_stable_file(path: &std::path::Path, timeout_ms: u64) -> String {
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(timeout_ms);
        let mut last = String::new();
        loop {
            let current = std::fs::read_to_string(path).unwrap_or_default();
            if current == last && !current.is_empty() {
                return current;
            }
            last = current;
            if tokio::time::Instant::now() >= deadline {
                return last;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
    }

    // A file that's still being actively rewritten never satisfies the
    // "two consecutive identical reads" stability check above; the helper
    // must give up after `timeout_ms` and return the last-seen value rather
    // than hang forever (relevant if a launched script never converges or
    // never completes). Exercises `wait_for_stable_file`'s timeout branch,
    // which the tests above never reach since their scripts finish quickly.
    #[tokio::test]
    async fn wait_for_stable_file_gives_up_after_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("churn.txt");
        let writer_path = path.clone();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_writer = stop.clone();
        let writer = tokio::spawn(async move {
            let mut n: u64 = 0;
            while !stop_writer.load(std::sync::atomic::Ordering::Relaxed) {
                std::fs::write(&writer_path, format!("v{n}")).unwrap();
                n += 1;
                tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
            }
        });

        let result = wait_for_stable_file(&path, 200).await;

        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = writer.await;

        assert!(
            result.starts_with('v'),
            "expected a churned placeholder value, got {result:?}"
        );
    }

    // A plain (mpi=none) batch script must execute exactly once per node
    // regardless of --ntasks-per-node: task multiplicity is only advertised via
    // environment variables; further fan-out is the script's own responsibility,
    // typically via `srun`.
    // Without this, `launch_job` wraps every batch script in
    // `build_multi_task_wrapper` whenever tasks_per_node > 1, forking that
    // many concurrent copies of the ENTIRE script — corrupting any script
    // with more than a single trivial command. Reproduces that failure mode
    // directly: an unconditional counter step plus an `mkdir` step that
    // collides when run more than once concurrently.
    #[tokio::test]
    async fn sbatch_script_runs_exactly_once_regardless_of_ntasks_per_node() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        let work_dir = tempfile::tempdir().unwrap();
        let work_dir_str = work_dir.path().to_string_lossy().to_string();
        let counter_path = work_dir.path().join("run_count.txt");
        let collide_dir = work_dir.path().join("only_once_dir");

        let script = format!(
            "#!/bin/bash\necho ran >> \"{counter}\"\nmkdir \"{collide}\" 2>/dev/null || true\n",
            counter = counter_path.display(),
            collide = collide_dir.display(),
        );

        let req = Request::new(LaunchJobRequest {
            job_id: 5350,
            spec: Some(JobSpec {
                script,
                num_tasks: 4,
                num_nodes: 1,
                tasks_per_node: 4,
                cpus_per_task: 1,
                work_dir: work_dir_str,
                ..Default::default()
            }),
            allocated: Some(ResourceAllocations {
                cpus: 4,
                memory_mb: 0,
                devices: std::collections::HashMap::new(),
            }),
            // Default (false): a genuine sbatch batch script, not an
            // explicit srun task fan-out.
            ..Default::default()
        });

        let resp = svc.launch_job(req).await.expect("launch should succeed");
        assert!(resp.into_inner().success, "launch should succeed");

        let content = wait_for_stable_file(&counter_path, 2_000).await;
        let runs = content.lines().filter(|l| *l == "ran").count();
        assert_eq!(
            runs, 1,
            "batch script must run exactly once regardless of tasks_per_node=4, got {runs} run(s): {content:?}"
        );
    }

    // Counterpart to the test above: a standalone `srun` request routed
    // through the batch dispatch path (Kubernetes-inclusive allocations;
    // see `dispatch_job_to_nodes` in scheduler_loop.rs) sets
    // `task_fanout: true` because there the dispatched "script" is the
    // literal command srun was asked to run `tasks_per_node` times — real
    // srun semantics that must not regress into running only once.
    #[tokio::test]
    async fn task_fanout_dispatch_still_replicates_per_ntasks_per_node() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        let work_dir = tempfile::tempdir().unwrap();
        let work_dir_str = work_dir.path().to_string_lossy().to_string();
        let counter_path = work_dir.path().join("run_count.txt");

        let script = format!(
            "#!/bin/bash\necho ran >> \"{counter}\"\n",
            counter = counter_path.display(),
        );

        let req = Request::new(LaunchJobRequest {
            job_id: 5351,
            spec: Some(JobSpec {
                script,
                num_tasks: 4,
                num_nodes: 1,
                tasks_per_node: 4,
                cpus_per_task: 1,
                work_dir: work_dir_str,
                ..Default::default()
            }),
            allocated: Some(ResourceAllocations {
                cpus: 4,
                memory_mb: 0,
                devices: std::collections::HashMap::new(),
            }),
            task_fanout: true,
            ..Default::default()
        });

        let resp = svc.launch_job(req).await.expect("launch should succeed");
        assert!(resp.into_inner().success, "launch should succeed");

        let content = wait_for_stable_file(&counter_path, 2_000).await;
        let runs = content.lines().filter(|l| *l == "ran").count();
        assert_eq!(
            runs, 4,
            "task_fanout dispatch must still run tasks_per_node=4 copies, got {runs} run(s): {content:?}"
        );
    }

    // Genuine sbatch with `--mpi=pmix` still requires a PMIx launch plan from
    // the controller; without one the launch fails before any script runs.
    #[tokio::test]
    async fn sbatch_mpi_pmix_without_pmix_plan_fails() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        let work_dir = tempfile::tempdir().unwrap();
        let work_dir_str = work_dir.path().to_string_lossy().to_string();

        let req = Request::new(LaunchJobRequest {
            job_id: 5352,
            spec: Some(JobSpec {
                script: "#!/bin/bash\ntrue\n".into(),
                num_tasks: 4,
                num_nodes: 1,
                tasks_per_node: 4,
                cpus_per_task: 1,
                mpi: MPI_PMIX.into(),
                work_dir: work_dir_str.clone(),
                ..Default::default()
            }),
            allocated: Some(ResourceAllocations {
                cpus: 4,
                memory_mb: 0,
                devices: std::collections::HashMap::new(),
            }),
            // Default (false): a genuine sbatch job, not a routed srun
            // request — no pmix_plan is supplied either, so if this reached
            // the multi-task wrapper it would need one regardless.
            ..Default::default()
        });

        let result = svc.launch_job(req).await;
        assert!(
            result.is_err(),
            "a missing PMIx launch plan must still fail the launch"
        );
    }

    // `#SBATCH --mpi=pmix` with an inner `srun` runs the batch script without
    // batch-level PMIx; the step owns PMIx setup.
    #[tokio::test]
    async fn sbatch_mpi_pmix_inner_srun_runs_without_batch_pmix_plan() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        let work_dir = tempfile::tempdir().unwrap();
        let work_dir_str = work_dir.path().to_string_lossy().to_string();
        let counter_path = work_dir.path().join("run_count.txt");

        let req = Request::new(LaunchJobRequest {
            job_id: 5353,
            spec: Some(JobSpec {
                script: format!(
                    "#!/bin/bash\necho ran >> \"{}\"\nsrun true\n",
                    counter_path.display()
                ),
                num_tasks: 4,
                num_nodes: 2,
                tasks_per_node: 2,
                cpus_per_task: 1,
                mpi: MPI_PMIX.into(),
                work_dir: work_dir_str,
                ..Default::default()
            }),
            allocated: Some(ResourceAllocations {
                cpus: 2,
                memory_mb: 0,
                devices: std::collections::HashMap::new(),
            }),
            ..Default::default()
        });

        let resp = svc.launch_job(req).await.expect("launch should succeed");
        assert!(
            resp.into_inner().success,
            "inner-srun batch must launch without batch PMIx"
        );

        let content = wait_for_stable_file(&counter_path, 2_000).await;
        let runs = content.lines().filter(|l| *l == "ran").count();
        assert_eq!(
            runs, 1,
            "batch script with inner srun must run exactly once on this node, got {runs}: {content:?}"
        );
    }

    // The monitor loop's reconcile step must reclaim an
    // allocation whose job is no longer tracked, while sparing a job that is
    // still in `running`. Exercises the real reconcile_orphaned_allocations
    // wiring the monitor loop calls, without driving the timed loop.
    #[tokio::test]
    async fn reconcile_reclaims_orphan_but_spares_tracked_job() {
        let svc = AgentService::new(
            test_reporter_with_gpus(&[0, 1]),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        // job 1: tracked (live) and committed.
        svc.insert_test_job(1, TrackedJob::dummy(0)).await;
        // job 2: orphan — committed allocation but never entered `running`
        // (simulating a teardown path that dropped the job without releasing).
        {
            let mut alloc = svc.allocation.lock().await;
            alloc.allocate_for_job(1, 2, 0, &[0]).unwrap();
            alloc.commit_job(1);
            alloc.allocate_for_job(2, 2, 0, &[1]).unwrap();
            alloc.commit_job(2);
        }
        assert_eq!(svc.free_gpu_count().await, 0);

        {
            let jobs = svc.running.lock().await;
            reconcile_orphaned_allocations(&jobs, &mut *svc.allocation.lock().await);
        }

        // Orphan (job 2) reclaimed; live job 1 still holds its GPU.
        assert_eq!(
            svc.free_gpu_count().await,
            1,
            "exactly the orphan's GPU must be reclaimed; the tracked job's is spared"
        );
    }

    // A conflicting owner no longer in `running` is stale and must be reclaimed
    // so the dispatch succeeds instead of stranding the node.
    #[tokio::test]
    async fn dispatch_reclaims_stale_gpu_owner_not_running() {
        let svc = AgentService::new(
            test_reporter_with_gpus(&[0]),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        // Prior job 99 owns GPU 0 (committed) but never entered `running`
        // (its completion report was force-finished by the controller).
        {
            let mut alloc = svc.allocation.lock().await;
            alloc.allocate_for_job(99, 1, 0, &[0]).unwrap();
            alloc.commit_job(99);
        }
        assert_eq!(svc.free_gpu_count().await, 0);

        let mut devices = std::collections::HashMap::new();
        devices.insert(
            "gpu".to_string(),
            DeviceAllocations {
                devices: vec![AllocatedDevice {
                    device_id: 0,
                    count: 1,
                }],
            },
        );
        let spec = JobSpec {
            cpus_per_task: 1,
            gres: vec!["gpu:1".into()],
            ..Default::default()
        };
        let allocated = ResourceAllocations {
            cpus: 1,
            memory_mb: 0,
            devices,
        };

        let res = svc
            .allocate_local_for_test(100, &spec, Some(&allocated))
            .await;
        assert!(
            res.is_ok(),
            "dispatch must reclaim the stale owner's GPU and succeed, got {res:?}"
        );
    }

    // A conflicting owner still in `running` must never be reclaimed (that would
    // double-allocate the GPU); the dispatch stays rejected.
    #[tokio::test]
    async fn dispatch_rejects_when_conflicting_owner_still_running() {
        let svc = AgentService::new(
            test_reporter_with_gpus(&[0]),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        // Prior job 99 owns GPU 0 AND is actively tracked in `running`.
        svc.insert_test_job(99, TrackedJob::dummy(0)).await;
        {
            let mut alloc = svc.allocation.lock().await;
            alloc.allocate_for_job(99, 1, 0, &[0]).unwrap();
            alloc.commit_job(99);
        }

        let mut devices = std::collections::HashMap::new();
        devices.insert(
            "gpu".to_string(),
            DeviceAllocations {
                devices: vec![AllocatedDevice {
                    device_id: 0,
                    count: 1,
                }],
            },
        );
        let spec = JobSpec {
            cpus_per_task: 1,
            gres: vec!["gpu:1".into()],
            ..Default::default()
        };
        let allocated = ResourceAllocations {
            cpus: 1,
            memory_mb: 0,
            devices,
        };

        let res = svc
            .allocate_local_for_test(100, &spec, Some(&allocated))
            .await;
        let err = res.expect_err("must reject: the conflicting GPU owner is still running");
        assert_eq!(err.code(), tonic::Code::ResourceExhausted);
    }

    // A still-launching conflicting owner is a real duplicate: the reclaim must
    // spare it, so the retry fails and the dispatch stays rejected.
    #[tokio::test]
    async fn dispatch_rejects_when_conflicting_owner_still_launching() {
        let svc = AgentService::new(
            test_reporter_with_gpus(&[0]),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        // Prior job 99 owns GPU 0 and is still launching (never committed).
        {
            let mut alloc = svc.allocation.lock().await;
            alloc.allocate_for_job(99, 1, 0, &[0]).unwrap();
        }

        let mut devices = std::collections::HashMap::new();
        devices.insert(
            "gpu".to_string(),
            DeviceAllocations {
                devices: vec![AllocatedDevice {
                    device_id: 0,
                    count: 1,
                }],
            },
        );
        let spec = JobSpec {
            cpus_per_task: 1,
            gres: vec!["gpu:1".into()],
            ..Default::default()
        };
        let allocated = ResourceAllocations {
            cpus: 1,
            memory_mb: 0,
            devices,
        };

        let res = svc
            .allocate_local_for_test(100, &spec, Some(&allocated))
            .await;
        let err = res.expect_err("must reject: the conflicting GPU owner is still launching");
        assert_eq!(err.code(), tonic::Code::ResourceExhausted);

        // The launching owner must NOT have been reclaimed.
        assert_eq!(
            svc.free_gpu_count().await,
            0,
            "launching owner must be spared"
        );
    }

    fn gpu_alloc_request(device_ids: &[u32]) -> ResourceAllocations {
        let devices = device_ids
            .iter()
            .map(|id| AllocatedDevice {
                device_id: *id,
                count: 1,
            })
            .collect();
        let mut map = std::collections::HashMap::new();
        map.insert("gpu".to_string(), DeviceAllocations { devices });
        ResourceAllocations {
            cpus: 1,
            memory_mb: 0,
            devices: map,
        }
    }

    // A dispatch spanning two GPUs each held by a distinct stale owner must
    // reclaim both and succeed.
    #[tokio::test]
    async fn dispatch_reclaims_multiple_stale_owners() {
        let svc = AgentService::new(
            test_reporter_with_gpus(&[0, 1]),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        {
            let mut alloc = svc.allocation.lock().await;
            alloc.allocate_for_job(98, 1, 0, &[0]).unwrap();
            alloc.commit_job(98);
            alloc.allocate_for_job(99, 1, 0, &[1]).unwrap();
            alloc.commit_job(99);
        }
        assert_eq!(svc.free_gpu_count().await, 0);

        let spec = JobSpec {
            cpus_per_task: 1,
            gres: vec!["gpu:2".into()],
            ..Default::default()
        };
        let res = svc
            .allocate_local_for_test(100, &spec, Some(&gpu_alloc_request(&[0, 1])))
            .await;
        assert!(
            res.is_ok(),
            "both stale owners must be reclaimed, got {res:?}"
        );
    }

    // A dispatch spanning a stale GPU and a still-running GPU must reject: the
    // running owner cannot be reclaimed, so the retry still fails.
    #[tokio::test]
    async fn dispatch_rejects_partial_overlap_with_running_owner() {
        let svc = AgentService::new(
            test_reporter_with_gpus(&[0, 1]),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        // Job 98 (GPU 0) is stale; job 99 (GPU 1) is actively running.
        svc.insert_test_job(99, TrackedJob::dummy(0)).await;
        {
            let mut alloc = svc.allocation.lock().await;
            alloc.allocate_for_job(98, 1, 0, &[0]).unwrap();
            alloc.commit_job(98);
            alloc.allocate_for_job(99, 1, 0, &[1]).unwrap();
            alloc.commit_job(99);
        }

        let spec = JobSpec {
            cpus_per_task: 1,
            gres: vec!["gpu:2".into()],
            ..Default::default()
        };
        let res = svc
            .allocate_local_for_test(100, &spec, Some(&gpu_alloc_request(&[0, 1])))
            .await;
        let err = res.expect_err("must reject: GPU 1's owner is still running");
        assert_eq!(err.code(), tonic::Code::ResourceExhausted);
    }

    // A registered srun allocation must be committed AND tracked in `running`
    // so a reconcile pass spares it — the reservation is backed, not orphaned.
    #[tokio::test]
    async fn register_job_allocation_survives_reconcile() {
        let svc = AgentService::new(
            test_reporter_with_gpus(&[0]),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        let mut devices = std::collections::HashMap::new();
        devices.insert(
            "gpu".to_string(),
            DeviceAllocations {
                devices: vec![AllocatedDevice {
                    device_id: 0,
                    count: 1,
                }],
            },
        );
        svc.register_job_allocation(Request::new(RegisterJobAllocationRequest {
            job_id: 55,
            cpus: 1,
            allocated: Some(ResourceAllocations {
                cpus: 1,
                memory_mb: 0,
                devices,
            }),
            ..Default::default()
        }))
        .await
        .expect("register");

        assert_eq!(
            svc.free_gpu_count().await,
            0,
            "registered allocation holds the GPU"
        );

        // The job is in `running`, so reconcile must spare it (not orphan-reclaim).
        {
            let jobs = svc.running.lock().await;
            reconcile_orphaned_allocations(&jobs, &mut *svc.allocation.lock().await);
        }
        assert_eq!(
            svc.free_gpu_count().await,
            0,
            "committed+tracked allocation must survive reconcile"
        );
    }

    // The heartbeat's held-job source must report an allocation-only (srun/salloc)
    // job so the controller can reconcile it — the strand this fix addresses.
    #[tokio::test]
    async fn heartbeat_reports_held_allocation_only_job() {
        // One shared running map wired into both the reporter (heartbeat source)
        // and the agent service (owner) — the production wiring from main.rs.
        let running = new_running_jobs();
        let reporter = Arc::new(NodeReporter::new(
            "test-node".into(),
            "http://localhost:6817".into(),
            ResourceSet {
                cpus: 4,
                memory_mb: 8192,
                ..Default::default()
            },
            spur_net::NodeAddress {
                ip: "127.0.0.1".into(),
                hostname: "test-node".into(),
                port: 6818,
                source: spur_net::AddressSource::Static,
            },
            std::collections::HashMap::new(),
            String::new(),
            String::new(),
            running.clone(),
        ));
        let svc = AgentService::with_cluster_config(
            reporter.clone(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            &spur_core::config::ClusterConfig::default(),
            spur_core::config::JobLimits::default(),
            CgroupConfig::default(),
            MpiConfig::default(),
            running,
            false, // allow_root_jobs
        );

        assert!(
            reporter.held_job_ids().is_empty(),
            "reporter sees no jobs before registration"
        );

        svc.register_job_allocation(Request::new(RegisterJobAllocationRequest {
            job_id: 77,
            cpus: 1,
            allocated: Some(ResourceAllocations {
                cpus: 1,
                memory_mb: 0,
                devices: std::collections::HashMap::new(),
            }),
            ..Default::default()
        }))
        .await
        .expect("register");

        assert_eq!(
            reporter.held_job_ids(),
            vec![77],
            "reporter's heartbeat must observe the agent-registered allocation-only job"
        );
    }

    // CancelJob must release a still-launching (never-committed) reservation,
    // else a cancel-during-eviction strands it until the TTL.
    #[tokio::test]
    async fn cancel_releases_launching_reservation() {
        let svc = AgentService::new(
            test_reporter_with_gpus(&[0]),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        // Reserve GPU 0 as a still-launching job (not committed, not in running).
        {
            let mut alloc = svc.allocation.lock().await;
            alloc.allocate_for_job(7, 1, 0, &[0]).unwrap();
        }
        assert_eq!(
            svc.free_gpu_count().await,
            0,
            "GPU reserved while launching"
        );

        svc.cancel_job(Request::new(AgentCancelJobRequest {
            job_id: 7,
            signal: 9,
        }))
        .await
        .expect("cancel_job");

        assert_eq!(
            svc.free_gpu_count().await,
            1,
            "cancel must release a launching (never-committed) reservation"
        );
    }

    // A launch that aborts before entering `running` must tear down its PMI
    // server, since the monitor loop's completion cleanup never runs for it.
    #[tokio::test]
    async fn completion_cleanup_releases_batch_pmix_ref_without_force_stop() {
        use crate::mpi_plugin::ActiveNamespace;

        let svc = AgentService::new(
            test_reporter_with_gpus(&[0]),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        svc.mpi_host.active_namespaces.lock().unwrap().insert(
            99,
            ActiveNamespace {
                namespace: "spur.99".into(),
                refs: 2,
            },
        );

        cleanup_completed_job_mpi(99, MPI_PMIX, &svc.mpi_host).await;

        assert!(
            svc.mpi_host.has_active_pmix(99),
            "batch completion must release one ref, not force-stop an active step namespace"
        );
    }

    // The guard must release the reservation when dropped before commit
    // (the future-cancellation path), and leave it intact once disarmed.
    #[tokio::test]
    async fn reservation_guard_releases_on_drop_when_not_committed() {
        let svc = AgentService::new(
            test_reporter_with_gpus(&[0]),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        {
            svc.allocation
                .lock()
                .await
                .allocate_for_job(9, 1, 0, &[0])
                .unwrap();
            let guard = LaunchReservationGuard::new(svc.allocation.clone(), 9);
            assert_eq!(svc.free_gpu_count().await, 0, "reserved under guard");
            drop(guard);
        }
        assert_eq!(
            svc.free_gpu_count().await,
            1,
            "dropping an un-disarmed guard must release the reservation"
        );

        // A disarmed guard must NOT release (the job committed successfully).
        {
            svc.allocation
                .lock()
                .await
                .allocate_for_job(10, 1, 0, &[0])
                .unwrap();
            svc.allocation.lock().await.commit_job(10);
            let mut guard = LaunchReservationGuard::new(svc.allocation.clone(), 10);
            guard.disarm();
            drop(guard);
        }
        assert_eq!(
            svc.free_gpu_count().await,
            0,
            "a disarmed guard must leave the committed reservation intact"
        );
    }

    #[tokio::test]
    async fn run_command_injects_gpu_env_from_tracked_job() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(test_gpu_registry())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        let job_id = 700;
        let mut tracked = TrackedJob::dummy(0);
        tracked.gpu_devices = vec![0, 1];
        tracked.partition = "gpu".into();
        tracked.cpus = 8;
        tracked.memory_mb = 16384;
        svc.insert_test_job(job_id, tracked).await;

        let req = Request::new(RunCommandRequest {
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                "echo ROCR=$ROCR_VISIBLE_DEVICES CUDA=$CUDA_VISIBLE_DEVICES".into(),
            ],
            uid: 0,
            gid: 0,
            work_dir: String::new(),
            environment: HashMap::new(),
            job_id,
            ..Default::default()
        });
        let resp = svc.run_command(req).await.unwrap().into_inner();
        assert_eq!(resp.exit_code, 0);
        assert!(
            resp.stdout.contains("ROCR=0,1"),
            "expected ROCR_VISIBLE_DEVICES=0,1 in stdout, got: {}",
            resp.stdout
        );
        assert!(
            !resp.stdout.contains("CUDA=0,1"),
            "AMD registry should not set CUDA_VISIBLE_DEVICES, got: {}",
            resp.stdout
        );
    }

    /// Helper: poll until the job is removed from `running` (by the monitor).
    async fn wait_job_reaped(svc: &AgentService, job_id: u32, timeout_ms: u64) -> bool {
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(timeout_ms);
        while tokio::time::Instant::now() < deadline {
            if svc.running.lock().await.get(&job_id).is_none() {
                return true;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
        }
        false
    }

    #[tokio::test]
    async fn graceful_cancel_sigterm_responsive() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        svc.start_monitor("http://127.0.0.1:1".into());

        let job_id = 900;
        svc.insert_test_job(job_id, TrackedJob::dummy(0)).await;

        svc.graceful_cancel(job_id).await;

        assert!(
            wait_job_reaped(&svc, job_id, 5_000).await,
            "monitor should reap SIGTERM-killed job within 5s"
        );
    }

    #[tokio::test]
    async fn graceful_cancel_escalates_to_sigkill() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        svc.start_monitor("http://127.0.0.1:1".into());

        let job_id = 901;
        let child = tokio::process::Command::new("/bin/sh")
            .args(["-c", "trap '' TERM; while true; do sleep 1; done"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            // Match how real managed jobs spawn (own process group) so
            // group-targeted signals land.
            .process_group(0)
            .spawn()
            .expect("failed to spawn SIGTERM-trapping process");
        let tracked = TrackedJob {
            job: executor::RunningJob::Managed {
                child,
                cgroup_path: None,
            },
            rootfs_mode: crate::container::RootfsMode::Extracted,
            stdout_path: "/dev/null".into(),
            stderr_path: "/dev/null".into(),
            has_pid_namespace: false,
            has_user_namespace: false,
            has_mount_namespace: false,
            _pty_master: None,
            work_dir: "/tmp".into(),
            uid: 0,
            gid: 0,
            user: "testuser".into(),
            partition: String::new(),
            gpu_devices: Vec::new(),
            cpus: 1,
            memory_mb: 0,
            nodelist: String::new(),
            mpi: String::new(),
            run_attempt: 0,
        };
        svc.insert_test_job(job_id, tracked).await;

        svc.graceful_cancel(job_id).await;

        // 5s grace + up to 2s monitor tick + buffer
        assert!(
            wait_job_reaped(&svc, job_id, 10_000).await,
            "monitor should reap job after SIGKILL escalation"
        );
    }

    // The grace-period SIGKILL must not fire if job_id was reused by a newer
    // run (epoch bumped) after a requeue. Guards the preempt-requeue race.
    #[tokio::test(start_paused = true)]
    async fn graceful_cancel_skips_sigkill_for_reused_job_id() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        // No monitor: this test asserts the grace timer's epoch guard directly,
        // so nothing should reap the job out from under it.

        // SIGTERM-trapping process so epoch 1 survives its cancel; dummy()'s
        // plain sleep would die on the first SIGTERM.
        fn spawn_trap(run_attempt: u32) -> (TrackedJob, i32) {
            let child = tokio::process::Command::new("/bin/sh")
                .args(["-c", "trap '' TERM; while true; do sleep 1; done"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .process_group(0)
                .spawn()
                .expect("spawn trap process");
            let pid = child.id().expect("pid") as i32;
            let t = TrackedJob {
                job: executor::RunningJob::Managed {
                    child,
                    cgroup_path: None,
                },
                rootfs_mode: crate::container::RootfsMode::Extracted,
                stdout_path: "/dev/null".into(),
                stderr_path: "/dev/null".into(),
                has_pid_namespace: false,
                has_user_namespace: false,
                has_mount_namespace: false,
                _pty_master: None,
                work_dir: "/tmp".into(),
                uid: 0,
                gid: 0,
                user: "testuser".into(),
                partition: String::new(),
                gpu_devices: Vec::new(),
                cpus: 1,
                memory_mb: 0,
                nodelist: String::new(),
                mpi: String::new(),
                run_attempt,
            };
            (t, pid)
        }
        let job_id = 902;
        let (run1, pid1) = spawn_trap(1);
        svc.insert_test_job(job_id, run1).await;

        // Cancel epoch 1 (SIGTERM; trapped, survives) and spawn the grace timer.
        svc.graceful_cancel(job_id).await;

        // Simulate requeue + re-dispatch: same job_id, newer epoch.
        let (run2, pid2) = spawn_trap(2);
        svc.insert_test_job(job_id, run2).await;

        // Advance past the 5s grace period; the guard must skip the SIGKILL, so the
        // epoch-2 process stays alive. Assert a live state ('S'/'R'), not mere
        // /proc existence — a wrongly-killed unreaped child would be a zombie
        // ('Z'), which still has /proc and would false-pass an existence check.
        tokio::time::advance(tokio::time::Duration::from_secs(6)).await;
        tokio::task::yield_now().await;
        let state = proc_state(pid2);
        assert!(
            matches!(state, 'S' | 'R' | 'D'),
            "grace-period SIGKILL wrongly killed the re-dispatched run (state {state})"
        );

        // Cleanup: trap processes ignore SIGTERM; SIGKILL each process group
        // (negative pid) so the inner sleep child is reaped too.
        for pid in [pid1, pid2] {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(-pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        svc.running.lock().await.remove(&job_id);
    }

    fn proc_state(pid: i32) -> char {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let after = stat.rsplit(')').next().unwrap();
        after
            .split_whitespace()
            .next()
            .unwrap()
            .chars()
            .next()
            .unwrap()
    }

    /// Poll the process state until it matches `want` (or any char in it), up to ~2s.
    async fn await_proc_state(pid: i32, want: &[char]) -> char {
        for _ in 0..200 {
            let s = proc_state(pid);
            if want.contains(&s) {
                return s;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        proc_state(pid)
    }

    /// A displaced `Forked` run (root/container path) holds a raw pid that
    /// spurd must reap itself. `reap_killed_job` must collect it via `waitpid`;
    /// otherwise it lingers as a zombie once it leaves the monitor loop's map.
    #[tokio::test]
    async fn reap_killed_job_reaps_forked_variant() {
        // Fork a child that exits immediately, leaving it unreaped (a zombie)
        // until something waits on it — exactly the displaced-run situation.
        let pid = match unsafe { nix::unistd::fork() }.expect("fork") {
            nix::unistd::ForkResult::Child => unsafe { libc::_exit(0) },
            nix::unistd::ForkResult::Parent { child } => child.as_raw(),
        };

        // Let the child exit so it is a zombie before we reap it.
        assert_eq!(
            await_proc_state(pid, &['Z']).await,
            'Z',
            "forked child should be an unreaped zombie before reap_killed_job"
        );

        let job = executor::RunningJob::Forked {
            pid,
            _pidfd: None,
            cgroup_path: None,
            reaped: false,
        };
        reap_killed_job(job).await;

        // After reaping, the pid is gone from the process table entirely.
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "reap_killed_job must reap the Forked child (pid {pid} still present)"
        );
    }

    #[tokio::test]
    async fn suspend_then_resume_toggles_process_state() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        svc.start_monitor("http://127.0.0.1:1".into());

        let job_id = 903;
        let tracked = TrackedJob::dummy(0);
        let pid = tracked.job.pid().expect("dummy child should have a pid") as i32;
        svc.insert_test_job(job_id, tracked).await;

        svc.suspend_signal(job_id, false).await; // SIGSTOP
        assert_eq!(
            await_proc_state(pid, &['T']).await,
            'T',
            "process should be stopped after SIGSTOP"
        );

        svc.suspend_signal(job_id, true).await; // SIGCONT
        let state = await_proc_state(pid, &['R', 'S']).await;
        assert!(
            matches!(state, 'R' | 'S'),
            "process should run after SIGCONT, got {state}"
        );

        svc.send_explicit_signal(job_id, 9).await; // cleanup
    }

    #[tokio::test]
    async fn send_explicit_signal_kills_job() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        svc.start_monitor("http://127.0.0.1:1".into());

        let job_id = 902;
        svc.insert_test_job(job_id, TrackedJob::dummy(0)).await;

        svc.send_explicit_signal(job_id, 9).await; // SIGKILL

        assert!(
            wait_job_reaped(&svc, job_id, 5_000).await,
            "monitor should reap SIGKILL'd job within 5s"
        );
    }

    #[tokio::test]
    async fn job_entry_from_tracked_job() {
        let (svc, job_id) = run_command_test_setup().await;

        let entry = svc
            .job_entry(job_id)
            .await
            .expect("job_entry should succeed");
        assert!(entry.pid > 0);
        assert_eq!(entry.uid, 0);
        assert_eq!(entry.gid, 0);
        assert!(!entry.has_namespaces());
    }

    #[tokio::test]
    async fn job_entry_not_found() {
        let (svc, _) = run_command_test_setup().await;

        let err = svc.job_entry(9999).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn run_pty_bridge_echo_and_exit() {
        use spur_proto::proto::{interactive_input, interactive_output, InteractiveInput};

        let (master, slave) = crate::pty::openpty_with_winsize(None).expect("openpty");

        nix::fcntl::fcntl(
            &master,
            nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
        )
        .expect("O_NONBLOCK");

        let raw = crate::executor::JobIoRaw::Pty {
            master: std::os::fd::AsRawFd::as_raw_fd(&master),
            slave: std::os::fd::AsRawFd::as_raw_fd(&slave),
        };
        let mut cmd = tokio::process::Command::new("cat");
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        unsafe {
            cmd.pre_exec(move || raw.wire());
        }
        let child = cmd.spawn().expect("spawn cat");
        let child_pid = child.id().expect("child pid") as i32;
        drop(slave);

        let (in_tx, in_rx) =
            tokio::sync::mpsc::channel::<Result<InteractiveInput, tonic::Status>>(64);
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<
            Result<spur_proto::proto::InteractiveOutput, tonic::Status>,
        >(64);

        let inbound = tokio_stream::wrappers::ReceiverStream::new(in_rx);
        tokio::spawn(AgentService::run_pty_bridge(
            master, child, child_pid, inbound, out_tx,
        ));

        in_tx
            .send(Ok(InteractiveInput {
                msg: Some(interactive_input::Msg::Stdin(b"hello\n".to_vec())),
            }))
            .await
            .unwrap();

        let mut collected = Vec::new();
        for _ in 0..1000 {
            match out_rx.recv().await {
                Some(Ok(msg)) => match msg.msg {
                    Some(interactive_output::Msg::Data(d)) => {
                        collected.extend_from_slice(&d);
                        if collected.windows(5).any(|w| w == b"hello") {
                            break;
                        }
                    }
                    Some(interactive_output::Msg::ExitStatus(_)) => break,
                    None => {}
                },
                _ => break,
            }
        }
        let text = String::from_utf8_lossy(&collected);
        assert!(
            text.contains("hello"),
            "expected echoed 'hello', got: {text}"
        );

        drop(in_tx);

        for _ in 0..1000 {
            match out_rx.recv().await {
                Some(Ok(msg)) => {
                    if let Some(interactive_output::Msg::ExitStatus(_code)) = msg.msg {
                        return;
                    }
                }
                _ => break,
            }
        }
        panic!("did not receive exit status from bridge");
    }
}
