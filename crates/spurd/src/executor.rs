// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{bail, Context};
use nix::sys::signal::{self, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::unistd::Pid;
use tokio::process::Command;
use tracing::{debug, info, warn};

use spur_core::job::JobId;
use spur_spank::SpankHost;

/// Typed launch errors so callers can distinguish prolog failure from other failures.
pub enum LaunchError {
    PrologFailed(anyhow::Error),
    Other(anyhow::Error),
}

impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PrologFailed(e) => write!(f, "prolog failed: {e}"),
            Self::Other(e) => write!(f, "{e}"),
        }
    }
}

impl From<anyhow::Error> for LaunchError {
    fn from(e: anyhow::Error) -> Self {
        Self::Other(e)
    }
}

use crate::container::ContainerConfig;

/// Cgroup root for slurmd-managed jobs.
const CGROUP_ROOT: &str = "/sys/fs/cgroup/spur";

pub struct ContainerLaunchConfig {
    pub config: ContainerConfig,
    pub rootfs: PathBuf,
}

/// Everything an agent needs to launch a job process on this node.
///
/// Groups the resolved execution parameters that come from multiple sources
/// (JobSpec, scheduler allocation, agent config) into a single value.
pub struct JobLaunchConfig {
    pub job_id: JobId,
    pub script: String,
    pub work_dir: String,
    pub environment: HashMap<String, String>,
    pub stdout_path: String,
    pub stderr_path: String,
    pub cpus: u32,
    pub memory_mb: u64,
    pub gpu_devices: Vec<u32>,
    pub cpu_ids: Vec<u32>,
    pub open_mode: Option<String>,
    pub uid: u32,
    pub gid: u32,
    pub container: Option<ContainerLaunchConfig>,
    pub prolog_script: Option<String>,
    pub partition: String,
    pub nodelist: String,
    /// Registry-based device injection plan for host (non-container) jobs.
    pub host_device_plan: Option<spur_devices::inject::HostInjectionPlan>,
}

/// A running job process — either a tokio-managed child or a raw-forked container.
pub enum RunningJob {
    /// Non-container jobs managed by tokio::process::Child.
    Managed {
        child: tokio::process::Child,
        cgroup_path: Option<PathBuf>,
    },
    /// Container jobs: raw fork with optional pidfd for PID-recycling safety.
    Forked {
        pid: i32,
        /// Holds a kernel reference preventing PID recycling. None on kernels < 5.3.
        _pidfd: Option<OwnedFd>,
        cgroup_path: Option<PathBuf>,
        reaped: bool,
    },
}

/// Split a finished process's wait status into (exit_code, signal).
/// Slurm parity: WIFEXITED -> (code, 0); WIFSIGNALED -> (0, sig).
pub fn decode_wait_status(status: nix::sys::wait::WaitStatus) -> (i32, i32) {
    match status {
        nix::sys::wait::WaitStatus::Exited(_, code) => (code, 0),
        nix::sys::wait::WaitStatus::Signaled(_, sig, _) => (0, sig as i32),
        _ => (-1, 0), // unreachable from try_wait (only Exited/Signaled reach here); -1 = shouldn't-happen sentinel
    }
}

fn pidfd_open(pid: i32) -> std::io::Result<OwnedFd> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as RawFd;
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

impl RunningJob {
    pub fn pid(&self) -> Option<u32> {
        match self {
            RunningJob::Managed { child, .. } => child.id(),
            RunningJob::Forked { pid, .. } => Some(*pid as u32),
        }
    }

    /// Non-blocking check for process exit. Returns (exit_code, signal) if done.
    pub fn try_wait(&mut self) -> anyhow::Result<Option<(i32, i32)>> {
        match self {
            RunningJob::Managed { child, .. } => match child.try_wait() {
                Ok(Some(status)) => {
                    use std::os::unix::process::ExitStatusExt;
                    Ok(Some((
                        status.code().unwrap_or(0),
                        status.signal().unwrap_or(0),
                    )))
                }
                Ok(None) => Ok(None),
                Err(e) => Err(e.into()),
            },
            RunningJob::Forked { pid, reaped, .. } => {
                if *reaped {
                    return Ok(None);
                }
                match nix::sys::wait::waitpid(
                    Pid::from_raw(*pid),
                    Some(nix::sys::wait::WaitPidFlag::WNOHANG),
                ) {
                    Ok(nix::sys::wait::WaitStatus::StillAlive) => Ok(None),
                    Ok(status @ nix::sys::wait::WaitStatus::Exited(_, _))
                    | Ok(status @ nix::sys::wait::WaitStatus::Signaled(_, _, _)) => {
                        *reaped = true;
                        Ok(Some(decode_wait_status(status)))
                    }
                    Ok(_) => Ok(None),
                    Err(e) => Err(e.into()),
                }
            }
        }
    }

    /// Send a signal to the running process.
    ///
    /// Managed jobs are spawned as their own process-group leader, so we signal
    /// the whole group (negative pid) to reach the batch shell and its children
    /// (e.g. an inner `sleep`), not just the tracked process.
    /// For container (Forked) jobs, signals the entire process subtree
    /// since the tracked PID is the intermediate parent and the actual
    /// workload runs as a grandchild inside a PID namespace.
    pub fn kill_signal(&self, sig: Signal) -> anyhow::Result<()> {
        match self {
            RunningJob::Managed { child, .. } => {
                if let Some(pid) = child.id() {
                    // Negative pid = the job's process group.
                    signal::kill(Pid::from_raw(-(pid as i32)), sig)?;
                }
                Ok(())
            }
            RunningJob::Forked { pid, reaped, .. } => {
                if *reaped {
                    return Ok(());
                }
                kill_process_tree(*pid, sig);
                Ok(())
            }
        }
    }

    pub fn take_cgroup(&mut self) -> Option<PathBuf> {
        match self {
            RunningJob::Managed { cgroup_path, .. } => cgroup_path.take(),
            RunningJob::Forked { cgroup_path, .. } => cgroup_path.take(),
        }
    }
}

/// Launch a job script on this node.
///
/// If `container` is `Some`, the job runs inside a container via explicit
/// `fork()` + `container_init()` (namespace, mounts, pivot_root, priv drop).
/// Otherwise, it uses the standard `tokio::Command` path with optional
/// `build_namespace_wrapper()` for non-container namespace isolation.
pub async fn launch_job(
    cfg: &JobLaunchConfig,
    spank: Option<&SpankHost>,
) -> Result<RunningJob, LaunchError> {
    // Run prolog before anything else
    if let Some(ref prolog) = cfg.prolog_script {
        let ctx = spur_core::hooks::HookContext {
            job_id: cfg.job_id,
            work_dir: cfg.work_dir.clone(),
            uid: cfg.uid,
            gid: cfg.gid,
            partition: cfg.partition.clone(),
            nodelist: cfg.nodelist.clone(),
            script_context: "prolog_slurmd".into(),
            gpu_devices: cfg.gpu_devices.clone(),
            cpus: cfg.cpus,
            memory_mb: cfg.memory_mb,
        };
        spur_core::hooks::run_hook(prolog, &ctx)
            .await
            .map_err(LaunchError::PrologFailed)?;
    }

    spawn_job_process(cfg, spank)
        .await
        .map_err(LaunchError::Other)
}

async fn spawn_job_process(
    cfg: &JobLaunchConfig,
    spank: Option<&SpankHost>,
) -> anyhow::Result<RunningJob> {
    let JobLaunchConfig {
        job_id,
        ref script,
        ref work_dir,
        ref environment,
        ref stdout_path,
        ref stderr_path,
        cpus,
        memory_mb,
        gpu_devices: _,
        ref cpu_ids,
        ref open_mode,
        uid,
        gid,
        ref container,
        ..
    } = *cfg;
    info!(job_id, work_dir, "launching job");

    // Invoke SPANK Init hook (after prolog, before process spawn)
    if let Some(spank) = spank {
        if let Err(e) = spank.invoke_hook(spur_spank::SpankHook::Init) {
            warn!(job_id, error = %e, "SPANK Init hook failed");
        }
        if let Err(e) = spank.invoke_hook(spur_spank::SpankHook::TaskInit) {
            warn!(job_id, error = %e, "SPANK TaskInit hook failed");
        }
    }

    // Set up cgroup for isolation
    let cgroup_path = setup_cgroup(job_id, cpus, memory_mb, cpu_ids)?;

    // Ensure work_dir exists on this node (the submitted path may only exist on the submitting
    // node). If creation fails (e.g. path is under another user's home), fall back to /tmp so
    // the job can still run; absolute output paths in the spec are unaffected.
    let effective_work_dir: String = if tokio::fs::create_dir_all(work_dir).await.is_ok() {
        work_dir.to_string()
    } else {
        warn!(
            job_id,
            work_dir, "work_dir unavailable on this node, using /tmp"
        );
        "/tmp".to_string()
    };
    let work_dir = effective_work_dir.as_str();

    // Wrap script with burst buffer stage-in/stage-out if configured
    let script = if let Ok(bb) = std::env::var("SPUR_BURST_BUFFER") {
        if !bb.is_empty() {
            wrap_with_burst_buffer(script, &bb)
        } else {
            script.to_string()
        }
    } else {
        script.to_string()
    };
    let script = script.as_str();

    // Write script to temp file
    let script_path = PathBuf::from(work_dir).join(format!(".spur_job_{}.sh", job_id));
    tokio::fs::write(&script_path, script)
        .await
        .context("failed to write job script")?;

    // Make executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(&script_path, perms)?;
    }

    // Resolve stdout/stderr paths
    let stdout_resolved = resolve_output_path(stdout_path, job_id, work_dir);
    let stderr_resolved = resolve_output_path(stderr_path, job_id, work_dir);

    // Ensure output directories exist
    if let Some(parent) = Path::new(&stdout_resolved).parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    if let Some(parent) = Path::new(&stderr_resolved).parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }

    let use_append = open_mode
        .as_deref()
        .map(|m| m.eq_ignore_ascii_case("append"))
        .unwrap_or(false);

    let stdout_file = if use_append {
        tokio::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&stdout_resolved)
            .await
            .context("failed to open stdout file in append mode")?
    } else {
        tokio::fs::File::create(&stdout_resolved)
            .await
            .context("failed to create stdout file")?
    };
    let stderr_file = if use_append {
        tokio::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&stderr_resolved)
            .await
            .context("failed to open stderr file in append mode")?
    } else {
        tokio::fs::File::create(&stderr_resolved)
            .await
            .context("failed to create stderr file")?
    };

    let mut env = environment.clone();

    // GPU isolation via registry-based device injection plan.
    if let Some(ref plan) = cfg.host_device_plan {
        for (key, value) in &plan.env {
            env.insert(key.clone(), value.clone());
        }
    }

    // Environment-based CPU/thread limiting — works even without cgroups.
    // Well-behaved applications (OpenMP, MKL, PyTorch, etc.) read these.
    env.insert("OMP_NUM_THREADS".into(), cpus.to_string());
    env.insert("MKL_NUM_THREADS".into(), cpus.to_string());
    env.insert("OPENBLAS_NUM_THREADS".into(), cpus.to_string());
    env.insert("VECLIB_MAXIMUM_THREADS".into(), cpus.to_string());
    env.insert("NUMEXPR_NUM_THREADS".into(), cpus.to_string());

    // Container jobs: use explicit fork() + container_init() instead of bash wrapper.
    if let Some(ctn) = container {
        return launch_container_job(
            cfg,
            ctn,
            &env,
            use_append,
            &stdout_resolved,
            &stderr_resolved,
        )
        .await;
    }

    // --- Non-container jobs: existing tokio::Command path ---

    // Issue #99: If root, wrap job with namespace isolation.
    let use_namespaces = nix::unistd::geteuid().is_root();
    let (launch_cmd, launch_args) = if use_namespaces {
        let wrapper_path = PathBuf::from(work_dir).join(format!(".spur_ns_{}.sh", job_id));
        let visible_devices = cfg
            .host_device_plan
            .as_ref()
            .map(|p| p.visible_devices.as_slice())
            .unwrap_or(&[]);
        let wrapper = build_namespace_wrapper(uid, gid, visible_devices, &script_path);
        tokio::fs::write(&wrapper_path, &wrapper).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&wrapper_path, std::fs::Permissions::from_mode(0o755))?;
        }
        debug!(job_id, "namespace isolation wrapper created");
        (
            "/usr/bin/unshare".to_string(),
            vec![
                "--pid".into(),
                "--mount".into(),
                "--fork".into(),
                "/bin/bash".into(),
                wrapper_path.to_string_lossy().to_string(),
            ],
        )
    } else {
        (
            "/bin/bash".to_string(),
            vec![script_path.to_string_lossy().to_string()],
        )
    };

    // Launch the process
    let mut cmd = Command::new(&launch_cmd);
    cmd.args(&launch_args)
        .current_dir(work_dir)
        .envs(&env)
        .stdout(stdout_file.into_std().await)
        .stderr(stderr_file.into_std().await)
        .stdin(Stdio::null())
        // Run the batch process in its own process group (pgid == its pid) so
        // signals can target the whole job tree without touching spurd's group.
        .process_group(0);

    // Reset signal dispositions to default before exec. spurd is launched in the
    // background (SIGINT/SIGQUIT/SIGHUP set to SIG_IGN), and a child inherits that
    // ignore mask — which would make a job's own `kill -INT $$` a no-op and break
    // Slurm-parity signal reporting (e.g. SIGINT -> RaisedSignal:2). The job must
    // start with default handlers.
    unsafe {
        cmd.pre_exec(|| {
            // Use sigaction (async-signal-safe) rather than signal() to reset
            // dispositions; pre_exec runs post-fork in a multi-threaded process.
            let dfl = SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty());
            for sig in [
                Signal::SIGINT,
                Signal::SIGQUIT,
                Signal::SIGHUP,
                Signal::SIGPIPE,
            ] {
                let _ = signal::sigaction(sig, &dfl);
            }
            Ok(())
        });
    }

    // Issue #99, #107: Run job as the submitting user (not root).
    // Must set supplementary groups (video, render) via initgroups()
    // so the process can access GPU device nodes.
    //
    // Issue #128: when use_namespaces is true, the wrapper handles the priv
    // drop *after* unshare runs (via setpriv). Dropping priv here would cause
    // unshare(2) to fail with EPERM since the unprivileged user lacks
    // CAP_SYS_ADMIN.
    if uid > 0 && nix::unistd::geteuid().is_root() && !use_namespaces {
        let target_uid = uid;
        let target_gid = gid;
        unsafe {
            cmd.pre_exec(move || {
                // Set supplementary groups from /etc/group for this user.
                // This is critical for GPU access — /dev/dri and /dev/kfd
                // are typically owned by root:video or root:render.
                let username = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(target_uid))
                    .ok()
                    .flatten()
                    .map(|u| u.name)
                    .unwrap_or_else(|| format!("{}", target_uid));
                let c_name = std::ffi::CString::new(username).unwrap_or_default();
                libc::initgroups(c_name.as_ptr(), target_gid);
                Ok(())
            });
        }
        cmd.uid(uid);
        cmd.gid(gid);
        debug!(
            job_id,
            uid, gid, "job will run as non-root user with supplementary groups"
        );
    }

    // Issue #99: Apply seccomp-BPF syscall filter (opt-in via SPUR_SECCOMP=1).
    let enable_seccomp = std::env::var("SPUR_SECCOMP")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);
    if enable_seccomp {
        unsafe {
            cmd.pre_exec(|| {
                if let Err(e) = crate::seccomp::apply_seccomp_filter() {
                    eprintln!("spur: seccomp filter not applied: {e}");
                }
                Ok(())
            });
        }
    }

    // Issue #99: Apply Landlock filesystem restrictions (opt-in via SPUR_LANDLOCK=1).
    let work_dir_for_landlock = work_dir.to_string();
    let enable_landlock = std::env::var("SPUR_LANDLOCK")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);
    if enable_landlock {
        unsafe {
            cmd.pre_exec(move || {
                if let Err(e) = crate::landlock::apply_landlock_rules(&work_dir_for_landlock) {
                    eprintln!("spur: landlock not applied: {e}");
                }
                Ok(())
            });
        }
    }

    let child = cmd.spawn().context("failed to spawn job process")?;

    // Move process into cgroup
    if let Some(ref cgroup) = cgroup_path {
        if let Some(pid) = child.id() {
            move_to_cgroup(cgroup, pid);
        }
    }

    debug!(
        job_id,
        pid = child.id(),
        script = %script_path.display(),
        "job process spawned"
    );

    Ok(RunningJob::Managed { child, cgroup_path })
}

/// Set up a cgroups v2 hierarchy for a job.
fn setup_cgroup(
    job_id: JobId,
    cpus: u32,
    memory_mb: u64,
    cpu_ids: &[u32],
) -> anyhow::Result<Option<PathBuf>> {
    let cgroup_path = PathBuf::from(CGROUP_ROOT).join(format!("job_{}", job_id));

    // Try to create cgroup — when running as root, failure is fatal.
    // Non-root is expected to fail (development/test environments).
    if let Err(e) = std::fs::create_dir_all(&cgroup_path) {
        if nix::unistd::geteuid().is_root() {
            anyhow::bail!("cgroup creation failed as root: {}", e);
        }
        warn!(
            job_id,
            error = %e,
            "cgroup creation failed (not root), running without isolation"
        );
        return Ok(None);
    }

    // Set CPU limit (cpu.max: quota period)
    // e.g., 4 CPUs → "400000 100000" (400ms out of 100ms period)
    let quota = cpus as u64 * 100_000;
    let cpu_max = format!("{} 100000", quota);
    if let Err(e) = std::fs::write(cgroup_path.join("cpu.max"), &cpu_max) {
        warn!(job_id, error = %e, "failed to set cpu.max");
    }

    // Set memory limit
    if memory_mb > 0 {
        let memory_bytes = memory_mb * 1024 * 1024;
        if let Err(e) = std::fs::write(cgroup_path.join("memory.max"), memory_bytes.to_string()) {
            warn!(job_id, error = %e, "failed to set memory.max");
        }
    }

    // OOM isolation: kill entire cgroup on OOM, not a random process
    if let Err(e) = std::fs::write(cgroup_path.join("memory.oom.group"), "1") {
        warn!(job_id, error = %e, "failed to set memory.oom.group");
    }

    // Fork bomb protection: limit total processes per job
    let max_pids = (cpus as u64 * 256).max(1024);
    if let Err(e) = std::fs::write(cgroup_path.join("pids.max"), max_pids.to_string()) {
        warn!(job_id, error = %e, "failed to set pids.max");
    }

    // Pin to specific CPU cores via cpuset
    if !cpu_ids.is_empty() {
        let cpuset_str: String = cpu_ids
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(",");
        if let Err(e) = std::fs::write(cgroup_path.join("cpuset.cpus"), &cpuset_str) {
            warn!(job_id, error = %e, "failed to set cpuset.cpus");
        } else {
            debug!(job_id, cpuset = %cpuset_str, "cpuset pinning configured");
        }
    }

    debug!(
        job_id,
        cpus,
        memory_mb,
        path = %cgroup_path.display(),
        "cgroup created"
    );

    Ok(Some(cgroup_path))
}

/// Move a process into a cgroup. Returns true if successful.
fn move_to_cgroup(cgroup_path: &Path, pid: u32) -> bool {
    let procs_file = cgroup_path.join("cgroup.procs");
    if let Err(e) = std::fs::write(&procs_file, pid.to_string()) {
        warn!(
            pid,
            error = %e,
            "failed to move process to cgroup — job runs without isolation"
        );
        false
    } else {
        true
    }
}

/// Clean up a job's cgroup.
pub fn cleanup_cgroup(cgroup_path: &Path) {
    // Kill any remaining processes
    if let Ok(pids) = std::fs::read_to_string(cgroup_path.join("cgroup.procs")) {
        for pid_str in pids.lines() {
            if let Ok(pid) = pid_str.trim().parse::<i32>() {
                let _ = signal::kill(Pid::from_raw(pid), Signal::SIGKILL);
            }
        }
    }

    // Remove cgroup directory
    if let Err(e) = std::fs::remove_dir(cgroup_path) {
        warn!(error = %e, path = %cgroup_path.display(), "failed to remove cgroup");
    }
}

/// Recursively signal a process and all its descendants (children first).
fn kill_process_tree(pid: i32, sig: Signal) {
    let children = get_child_pids(pid);
    for child in &children {
        kill_process_tree(*child, sig);
    }
    let _ = signal::kill(Pid::from_raw(pid), sig);
}

/// Read immediate child PIDs from /proc/<pid>/task/<pid>/children.
fn get_child_pids(pid: i32) -> Vec<i32> {
    let path = format!("/proc/{}/task/{}/children", pid, pid);
    std::fs::read_to_string(&path)
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect()
}

/// Resolve output path patterns (%j → job_id, etc.)
fn resolve_output_path(pattern: &str, job_id: JobId, work_dir: &str) -> String {
    let resolved = if pattern.is_empty() {
        format!("spur-{}.out", job_id)
    } else {
        pattern
            .replace("%j", &job_id.to_string())
            .replace("%J", &job_id.to_string())
    };

    if Path::new(&resolved).is_absolute() {
        resolved
    } else {
        PathBuf::from(work_dir)
            .join(resolved)
            .to_string_lossy()
            .into()
    }
}

/// Launch a containerized job via explicit fork() + container_init().
///
/// The child process does all container setup (namespaces, mounts, pivot_root,
/// priv drop) in Rust, then execs the job. No generated bash scripts, no
/// dependency on host binaries inside the container.
///
/// The parent tracks the child PID via a sync pipe and wraps waitpid in a
/// blocking tokio task so it doesn't stall the async runtime.
async fn launch_container_job(
    cfg: &JobLaunchConfig,
    ctn: &ContainerLaunchConfig,
    env: &HashMap<String, String>,
    use_append: bool,
    stdout_path: &str,
    stderr_path: &str,
) -> anyhow::Result<RunningJob> {
    let job_id = cfg.job_id;
    let cgroup_path = setup_cgroup(job_id, cfg.cpus, cfg.memory_mb, &cfg.cpu_ids)?;

    // Open stdout/stderr files before fork (child will dup2 these)
    let stdout_fd: std::fs::File = if use_append {
        std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(stdout_path)
            .context("open stdout for container job")?
    } else {
        std::fs::File::create(stdout_path).context("create stdout for container job")?
    };
    let stderr_fd: std::fs::File = if use_append {
        std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(stderr_path)
            .context("open stderr for container job")?
    } else {
        std::fs::File::create(stderr_path).context("create stderr for container job")?
    };

    // Sync pipe: child writes status, parent reads.
    // Convert OwnedFd to raw fds for manual lifecycle management across fork.
    let (pipe_r, pipe_w) = nix::unistd::pipe().context("create sync pipe")?;
    // Prevent read end from leaking into exec'd process
    nix::fcntl::fcntl(
        &pipe_r,
        nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
    )
    .ok();
    let ready_r = pipe_r.as_raw_fd();
    let ready_w = pipe_w.as_raw_fd();
    // Keep OwnedFd alive so the fds aren't closed prematurely
    let _pipe_r_owner = pipe_r;
    let _pipe_w_owner = pipe_w;

    // Snapshot everything the child needs (must not reference async state after fork)
    let config = &ctn.config;
    let rootfs = ctn.rootfs.clone();
    let env_snapshot = env.clone();
    let container_env = config.container_env.clone();
    let entrypoint = config.entrypoint.clone();

    match unsafe { nix::unistd::fork().context("fork for container job")? } {
        nix::unistd::ForkResult::Child => {
            // === CHILD PROCESS ===
            // CRITICAL: synchronous code only. Tokio runtime is broken after fork.
            drop(stdout_fd);
            drop(stderr_fd);
            unsafe {
                libc::close(ready_r);
            }

            // Reset signal handlers
            unsafe {
                libc::signal(libc::SIGCHLD, libc::SIG_DFL);
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            }

            // Redirect stdout/stderr
            let stdout_reopen = std::fs::File::options().append(true).open(stdout_path).ok();
            let stderr_reopen = std::fs::File::options().append(true).open(stderr_path).ok();
            if let Some(f) = stdout_reopen.as_ref() {
                unsafe { libc::dup2(f.as_raw_fd(), libc::STDOUT_FILENO) };
            }
            if let Some(f) = stderr_reopen.as_ref() {
                unsafe { libc::dup2(f.as_raw_fd(), libc::STDERR_FILENO) };
            }

            // Close inherited fds (gRPC sockets, other jobs' files)
            crate::container::close_inherited_fds(ready_w);

            // Run container init: namespaces, mounts, pivot_root, priv drop
            let hook_env = match crate::container::container_init(config, &rootfs) {
                Ok(env) => env,
                Err(e) => {
                    let msg = format!("E:{:#}", e);
                    unsafe {
                        libc::write(ready_w, msg.as_ptr() as *const _, msg.len());
                    }
                    std::process::exit(1);
                }
            };

            // Signal parent: setup complete
            unsafe {
                libc::write(ready_w, b"OK".as_ptr() as *const _, 2);
                libc::close(ready_w);
            }

            // Build final environment: base + container_env + hook environ.d
            let mut final_env = env_snapshot;
            for (k, v) in &container_env {
                final_env.insert(k.clone(), v.clone());
            }
            for (k, v) in hook_env {
                final_env.insert(k, v);
            }
            let c_env: Vec<CString> = final_env
                .iter()
                .filter_map(|(k, v)| CString::new(format!("{}={}", k, v)).ok())
                .collect();
            let c_env_refs: Vec<&std::ffi::CStr> = c_env.iter().map(|s| s.as_c_str()).collect();

            // Build exec args: with or without entrypoint
            let c_bash = CString::new("/bin/bash").unwrap();
            let exec_args: Vec<CString> = if let Some(ref ep) = entrypoint {
                let cmd = format!("{} && /bin/bash /tmp/spur_job_{}.sh", ep, job_id);
                vec![
                    c_bash.clone(),
                    CString::new("-c").unwrap(),
                    CString::new(cmd).unwrap(),
                ]
            } else {
                vec![
                    c_bash.clone(),
                    CString::new(format!("/tmp/spur_job_{}.sh", job_id)).unwrap(),
                ]
            };
            let exec_arg_refs: Vec<&std::ffi::CStr> =
                exec_args.iter().map(|s| s.as_c_str()).collect();

            let _ = nix::unistd::execve(&c_bash, &exec_arg_refs, &c_env_refs);
            eprintln!("spur: execve failed: {}", std::io::Error::last_os_error());
            std::process::exit(1);
        }

        nix::unistd::ForkResult::Parent { child } => {
            unsafe {
                libc::close(ready_w);
            }

            let child_pid = child.as_raw();

            if let Some(ref cgroup) = cgroup_path {
                let _ = std::fs::write(cgroup.join("cgroup.procs"), child_pid.to_string());
            }

            // pidfd prevents PID recycling; falls back gracefully on kernels < 5.3
            let pidfd = pidfd_open(child_pid).ok();
            if pidfd.is_none() {
                debug!("pidfd_open unavailable, falling back to raw PID tracking");
            }

            let mut buf = [0u8; 512];
            let n = unsafe { libc::read(ready_r, buf.as_mut_ptr() as *mut _, buf.len()) };
            let n = n.max(0) as usize;
            unsafe {
                libc::close(ready_r);
            }

            if n < 2 || &buf[..2] != b"OK" {
                let msg = String::from_utf8_lossy(&buf[..n]);
                bail!("container init failed for job {}: {}", job_id, msg);
            }

            info!(
                job_id,
                pid = child_pid,
                rootfs = %ctn.rootfs.display(),
                "containerized job launched (fork + pivot_root)"
            );

            Ok(RunningJob::Forked {
                pid: child_pid,
                _pidfd: pidfd,
                cgroup_path,
                reaped: false,
            })
        }
    }
}

/// Wrap a job script with burst buffer stage-in (before) and stage-out (after).
///
/// The `bb` string contains semicolon-separated directives:
///   - `stage_in:<cmd>` — run before the job
///   - `stage_out:<cmd>` — run after the job (best-effort, ignores failures)
///
/// Build the bash wrapper that runs inside the unshare PID/mount namespace.
///
/// The wrapper executes as root (the same uid as spurd), so it can perform
/// the proc/tmpfs/dri mounts that need CAP_SYS_ADMIN. Once isolation is in
/// place, it drops privilege via `setpriv --init-groups` and exec's the user
/// script.
///
/// Issue #128: previously the priv drop happened in `Command::pre_exec` before
/// exec'ing unshare, which made the unshare(2) syscall fail with EPERM and
/// the mounts silently no-op. Doing the drop inside the wrapper (after the
/// mounts) keeps the unshare and mounts privileged while still landing the
/// user payload as the unprivileged uid.
fn build_namespace_wrapper(
    uid: u32,
    gid: u32,
    visible_device_paths: &[String],
    script_path: &Path,
) -> String {
    let gpu_mounts = visible_device_paths
        .iter()
        .filter(|p| p.starts_with("/dev/dri/"))
        .map(|path| {
            let basename = path.rsplit('/').next().unwrap_or("");
            format!(
                "  if [ -e $SPUR_HOST_DRI/{b} ]; then\n    cp -a $SPUR_HOST_DRI/{b} /dev/dri/{b} 2>/dev/null || true\n  fi\n",
                b = basename,
            )
        })
        .collect::<Vec<_>>()
        .join("");

    let final_exec = if uid > 0 {
        format!(
            "exec setpriv --reuid={uid} --regid={gid} --init-groups -- /bin/bash {script}\n",
            uid = uid,
            gid = gid,
            script = script_path.display(),
        )
    } else {
        format!("exec /bin/bash {}\n", script_path.display())
    };

    format!(
        concat!(
            "#!/bin/bash\n",
            "# Namespace isolation wrapper — all mounts best-effort\n",
            "mount -t proc proc /proc 2>/dev/null || true\n",
            "mount -t tmpfs tmpfs /dev/shm 2>/dev/null || true\n",
            "# GPU device restriction: save original /dev/dri, replace with\n",
            "# tmpfs, then selectively copy only allocated devices back.\n",
            "SPUR_HOST_DRI=$(mktemp -d /tmp/.spur_dri_XXXXXX 2>/dev/null || echo /tmp/.spur_dri)\n",
            "if [ -d /dev/dri ] && cp -a /dev/dri/. $SPUR_HOST_DRI/ 2>/dev/null; then\n",
            "  mount -t tmpfs tmpfs /dev/dri 2>/dev/null || true\n",
            "{gpu_mounts}",
            "fi\n",
            "{final_exec}",
        ),
        gpu_mounts = gpu_mounts,
        final_exec = final_exec,
    )
}

fn wrap_with_burst_buffer(script: &str, bb: &str) -> String {
    let mut stage_in = Vec::new();
    let mut stage_out = Vec::new();

    for directive in bb.split(';') {
        let directive = directive.trim();
        if let Some(cmd) = directive.strip_prefix("stage_in:") {
            stage_in.push(cmd.trim().to_string());
        } else if let Some(cmd) = directive.strip_prefix("stage_out:") {
            stage_out.push(cmd.trim().to_string());
        }
    }

    if stage_in.is_empty() && stage_out.is_empty() {
        return script.to_string();
    }

    let mut wrapper = String::from("#!/bin/bash\n");

    // Stage-in commands (fail-fast)
    for cmd in &stage_in {
        wrapper.push_str(&format!("# Burst buffer stage-in\n{} || exit 1\n", cmd));
    }

    // The user script (inline)
    wrapper.push_str("# User script\n");
    // Remove shebang from user script if present to avoid nested shebangs
    let user_body = if script.starts_with("#!") {
        script.split_once('\n').map(|x| x.1).unwrap_or("")
    } else {
        script
    };
    wrapper.push_str(user_body);
    wrapper.push_str("\nSPUR_BB_EXIT=$?\n");

    // Stage-out commands (best-effort)
    for cmd in &stage_out {
        wrapper.push_str(&format!("# Burst buffer stage-out\n{} || true\n", cmd));
    }

    wrapper.push_str("exit $SPUR_BB_EXIT\n");
    wrapper
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_wait_status_splits_exit_and_signal() {
        use nix::sys::wait::WaitStatus;
        use nix::unistd::Pid;
        let p = Pid::from_raw(1);
        assert_eq!(decode_wait_status(WaitStatus::Exited(p, 7)), (7, 0));
        assert_eq!(
            decode_wait_status(WaitStatus::Signaled(
                p,
                nix::sys::signal::Signal::SIGKILL,
                false
            )),
            (0, 9)
        );
        assert_eq!(
            decode_wait_status(WaitStatus::Signaled(
                p,
                nix::sys::signal::Signal::SIGTERM,
                false
            )),
            (0, 15)
        );
        assert_eq!(decode_wait_status(WaitStatus::StillAlive), (-1, 0));
    }

    #[test]
    fn test_resolve_output_path() {
        assert_eq!(
            resolve_output_path("spur-%j.out", 42, "/home/user"),
            "/home/user/spur-42.out"
        );
        assert_eq!(
            resolve_output_path("/var/log/job-%j.log", 42, "/home/user"),
            "/var/log/job-42.log"
        );
        assert_eq!(resolve_output_path("", 42, "/tmp"), "/tmp/spur-42.out");
    }

    #[test]
    fn test_burst_buffer_wrap_stage_in_only() {
        let script = "#!/bin/bash\necho hello\n";
        let bb = "stage_in:cp /data/model.bin /tmp/";
        let wrapped = wrap_with_burst_buffer(script, bb);
        assert!(wrapped.contains("cp /data/model.bin /tmp/ || exit 1"));
        assert!(wrapped.contains("echo hello"));
        assert!(wrapped.contains("exit $SPUR_BB_EXIT"));
    }

    #[test]
    fn test_burst_buffer_wrap_stage_out_only() {
        let script = "#!/bin/bash\necho hello\n";
        let bb = "stage_out:cp /tmp/results /data/";
        let wrapped = wrap_with_burst_buffer(script, bb);
        assert!(wrapped.contains("cp /tmp/results /data/ || true"));
        assert!(wrapped.contains("echo hello"));
    }

    #[test]
    fn test_burst_buffer_wrap_both() {
        let script = "#!/bin/bash\necho hello\n";
        let bb = "stage_in:cp /data/in.bin /tmp/;stage_out:cp /tmp/out.bin /data/";
        let wrapped = wrap_with_burst_buffer(script, bb);
        assert!(wrapped.contains("cp /data/in.bin /tmp/ || exit 1"));
        assert!(wrapped.contains("cp /tmp/out.bin /data/ || true"));
        // Stage-in should come before user script, stage-out after
        let stage_in_pos = wrapped.find("stage-in").unwrap();
        let user_pos = wrapped.find("User script").unwrap();
        let stage_out_pos = wrapped.find("stage-out").unwrap();
        assert!(stage_in_pos < user_pos);
        assert!(user_pos < stage_out_pos);
    }

    #[test]
    fn test_burst_buffer_empty_passthrough() {
        let script = "#!/bin/bash\necho hello\n";
        let wrapped = wrap_with_burst_buffer(script, "");
        assert_eq!(wrapped, script);
    }

    /// Issue #128: when uid > 0, the wrapper must drop privilege via setpriv
    /// *after* the mounts (which need CAP_SYS_ADMIN). Dropping priv before
    /// unshare would cause unshare(2) to fail with EPERM.
    #[test]
    fn test_namespace_wrapper_drops_priv_via_setpriv() {
        let script = PathBuf::from("/work/.spur_job_42.sh");
        let wrapper = build_namespace_wrapper(1000, 1000, &[], &script);

        // setpriv must appear with both --reuid and --regid plus --init-groups
        // (so video/render supplementary groups are picked up for GPU access).
        assert!(
            wrapper.contains("setpriv --reuid=1000 --regid=1000 --init-groups"),
            "wrapper missing setpriv invocation: {wrapper}"
        );
        // The setpriv exec must be the *last* exec, after the mount commands.
        let mount_pos = wrapper.find("mount -t proc").expect("missing proc mount");
        let setpriv_pos = wrapper.find("setpriv").expect("missing setpriv");
        assert!(
            mount_pos < setpriv_pos,
            "mounts must run before priv drop:\n{wrapper}"
        );
        // No bare `exec /bin/bash` slip-through that would run as root.
        assert!(
            !wrapper.contains("exec /bin/bash /work"),
            "uid>0 wrapper must not exec bash directly as root:\n{wrapper}"
        );
    }

    /// When uid == 0 (root job), no priv drop is needed and the wrapper exec's
    /// bash directly.
    #[test]
    fn test_namespace_wrapper_root_no_setpriv() {
        let script = PathBuf::from("/work/.spur_job_7.sh");
        let wrapper = build_namespace_wrapper(0, 0, &[], &script);

        assert!(
            !wrapper.contains("setpriv"),
            "root job should not invoke setpriv:\n{wrapper}"
        );
        assert!(
            wrapper.contains("exec /bin/bash /work/.spur_job_7.sh"),
            "root wrapper should exec the job script directly:\n{wrapper}"
        );
    }

    /// GPU device restriction lines are emitted for each allocated DRI device.
    #[test]
    fn test_namespace_wrapper_gpu_mounts() {
        let script = PathBuf::from("/work/.spur_job_1.sh");
        let paths = vec!["/dev/dri/renderD128".into(), "/dev/dri/renderD130".into()];
        let wrapper = build_namespace_wrapper(1000, 1000, &paths, &script);

        assert!(wrapper.contains("renderD128"));
        assert!(wrapper.contains("renderD130"));
        assert!(!wrapper.contains("renderD129"));
        assert!(!wrapper.contains("renderD131"));
    }

    /// Non-DRI paths (e.g. /dev/nvidia*) are skipped — they can't be isolated
    /// via the /dev/dri tmpfs trick; env vars handle visibility instead.
    #[test]
    fn test_namespace_wrapper_ignores_non_dri_paths() {
        let script = PathBuf::from("/work/.spur_job_5.sh");
        let paths = vec![
            "/dev/nvidia0".into(),
            "/dev/nvidiactl".into(),
            "/dev/nvidia-uvm".into(),
            "/dev/dri/renderD128".into(),
        ];
        let wrapper = build_namespace_wrapper(1000, 1000, &paths, &script);

        assert!(wrapper.contains("renderD128"));
        assert!(!wrapper.contains("nvidia"));
    }
}
