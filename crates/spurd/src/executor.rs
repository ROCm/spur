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

use spur_core::config::MemlockLimit;
use spur_core::job::JobId;
use spur_spank::{SpankContext, SpankHandle, SpankHost};

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

/// Node-local spool root for spurd's per-job scratch (job script, namespace
/// wrapper). Deliberately off the user's work_dir so these root-side writes
/// never hit an NFS root_squash mount. Mirrors Slurm's SlurmdSpoolDir.
const SPOOL_ROOT: &str = "/var/spool/spur";

pub struct ContainerLaunchConfig {
    pub config: ContainerConfig,
    pub rootfs: PathBuf,
}

/// Everything an agent needs to launch a job process on this node.
///
/// Groups the resolved execution parameters that come from multiple sources
/// (JobSpec, scheduler allocation, agent config) into a single value.
/// How the job's I/O is connected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LaunchIo {
    /// Traditional file-based stdout/stderr capture.
    #[default]
    File,
    /// PTY-backed: stdout/stderr/stdin all go through a pseudo-terminal.
    /// The master fd is returned in `LaunchResult::pty_master`.
    Pty,
}

pub struct JobLaunchConfig {
    pub job_id: JobId,
    pub script: String,
    pub work_dir: String,
    pub environment: HashMap<String, String>,
    pub stdout_path: String,
    pub stderr_path: String,
    pub stdin_path: String,
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
    /// RLIMIT_MEMLOCK to apply before exec (while still privileged).
    pub memlock: MemlockLimit,
    /// I/O mode for the job.
    pub io_mode: LaunchIo,
}

pub struct LaunchResult {
    pub job: RunningJob,
    pub stdout_path: String,
    pub stderr_path: String,
    /// Master fd of the PTY (only set when `io_mode == LaunchIo::Pty`).
    pub pty_master: Option<OwnedFd>,
}

/// Owns the resolved fds for a job's stdio, built once and consumed by both
/// the container (raw fork) and non-container (tokio::Command) spawn paths.
enum JobIo {
    File {
        stdin: Option<OwnedFd>,
        stdout: OwnedFd,
        stderr: OwnedFd,
    },
    Pty {
        master: OwnedFd,
        slave: OwnedFd,
    },
}

/// `Copy` snapshot of raw fds from a `JobIo`, safe to move into a `pre_exec`
/// closure or use in a raw-fork child. The parent retains ownership of the
/// underlying `OwnedFd`s so they stay valid through the fork boundary.
#[derive(Clone, Copy)]
pub(crate) enum JobIoRaw {
    File {
        stdin: Option<RawFd>,
        stdout: RawFd,
        stderr: RawFd,
    },
    Pty {
        master: RawFd,
        slave: RawFd,
    },
}

impl JobIo {
    fn raw(&self) -> JobIoRaw {
        match self {
            JobIo::File {
                stdin,
                stdout,
                stderr,
            } => JobIoRaw::File {
                stdin: stdin.as_ref().map(|fd| fd.as_raw_fd()),
                stdout: stdout.as_raw_fd(),
                stderr: stderr.as_raw_fd(),
            },
            JobIo::Pty { master, slave } => JobIoRaw::Pty {
                master: master.as_raw_fd(),
                slave: slave.as_raw_fd(),
            },
        }
    }

    /// Parent-side: extract the PTY master fd, dropping everything else.
    fn into_master(self) -> Option<OwnedFd> {
        match self {
            JobIo::Pty { master, .. } => Some(master),
            JobIo::File { .. } => None,
        }
    }
}

impl JobIoRaw {
    /// Wire this job's stdio into the current process.
    ///
    /// For File mode: dup2 stdin/stdout/stderr from the opened files.
    /// For PTY mode: setsid + TIOCSCTTY + dup2 slave + close master.
    ///
    /// # Safety
    /// Must only be called in a child process (post-fork or inside pre_exec).
    /// All operations are async-signal-safe.
    pub(crate) unsafe fn wire(self) -> std::io::Result<()> {
        match self {
            JobIoRaw::File {
                stdin,
                stdout,
                stderr,
            } => {
                if let Some(fd) = stdin {
                    crate::pty::checked_dup2(fd, libc::STDIN_FILENO)?;
                    if fd > 2 {
                        libc::close(fd);
                    }
                }
                crate::pty::checked_dup2(stdout, libc::STDOUT_FILENO)?;
                if stdout > 2 {
                    libc::close(stdout);
                }
                crate::pty::checked_dup2(stderr, libc::STDERR_FILENO)?;
                if stderr > 2 && stderr != stdout {
                    libc::close(stderr);
                }
                Ok(())
            }
            JobIoRaw::Pty { master, slave } => crate::pty::pty_pre_exec(slave, master),
        }
    }
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
    /// Allocation registered without a batch process (standalone srun).
    AllocationOnly,
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

/// Set RLIMIT_MEMLOCK in the current process. Best-effort: a non-root spurd
/// cannot raise the hard limit beyond what it inherited.
pub(crate) fn apply_memlock(limit: MemlockLimit) {
    let v = match limit {
        MemlockLimit::Inherit => return,
        MemlockLimit::Unlimited => libc::RLIM_INFINITY,
        MemlockLimit::Bytes(n) => n as libc::rlim_t,
    };
    let rl = libc::rlimit {
        rlim_cur: v,
        rlim_max: v,
    };
    if unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &rl) } == 0 {
        return;
    }
    // Non-root cannot raise hard limit. Fall back: raise soft to current hard.
    let mut current = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut current) } == 0 {
        let fallback = libc::rlimit {
            rlim_cur: current.rlim_max,
            rlim_max: current.rlim_max,
        };
        unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &fallback) };
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
            RunningJob::AllocationOnly => None,
        }
    }

    pub fn is_allocation_only(&self) -> bool {
        matches!(self, RunningJob::AllocationOnly)
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
            RunningJob::AllocationOnly => Ok(None),
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
            RunningJob::AllocationOnly => Ok(()),
        }
    }

    pub fn take_cgroup(&mut self) -> Option<PathBuf> {
        match self {
            RunningJob::Managed { cgroup_path, .. } => cgroup_path.take(),
            RunningJob::Forked { cgroup_path, .. } => cgroup_path.take(),
            RunningJob::AllocationOnly => None,
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
) -> Result<LaunchResult, LaunchError> {
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
) -> anyhow::Result<LaunchResult> {
    let JobLaunchConfig {
        job_id,
        ref script,
        ref work_dir,
        ref environment,
        ref stdout_path,
        ref stderr_path,
        ref stdin_path,
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

    // Set up cgroup for isolation
    let cgroup_path = setup_cgroup(job_id, cpus, memory_mb, cpu_ids)?;

    // Ensure work_dir exists on this node (the submitted path may only exist on the submitting
    // node). If creation fails (e.g. path is under another user's home), fall back to /tmp so
    // the job can still run; absolute output paths in the spec are unaffected.
    let effective_work_dir: String = if create_dir_as_user(Path::new(work_dir), uid, gid) {
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

    // Script + wrapper live in the node-local spool dir, not work_dir (see
    // SPOOL_ROOT), so root-side writes survive NFS root_squash work_dirs.
    let spool_dir = create_job_spool_dir(job_id, uid, gid).context("create job spool dir")?;
    let script_path = spool_dir.join("spur_job.sh");
    write_job_scratch(&script_path, script, uid, gid).context("failed to write job script")?;

    // Build resolved output paths (empty for PTY mode since output goes to the terminal).
    let (stdout_resolved, stderr_resolved) = if cfg.io_mode == LaunchIo::Pty {
        ("/dev/null".to_string(), "/dev/null".to_string())
    } else {
        (
            resolve_output_path(stdout_path, job_id, work_dir),
            resolve_output_path(stderr_path, job_id, work_dir),
        )
    };

    // Build JobIo: a single object owning the fds for either file or PTY mode.
    let job_io = match cfg.io_mode {
        LaunchIo::Pty => {
            let (master, slave) = crate::pty::openpty_with_winsize(None).context("PTY openpty")?;
            JobIo::Pty { master, slave }
        }
        LaunchIo::File => {
            let stdin_resolved = if stdin_path.is_empty() {
                None
            } else {
                let r = resolve_output_path(stdin_path, job_id, work_dir);
                if r == stdout_resolved || r == stderr_resolved {
                    anyhow::bail!(
                        "stdin path {} overlaps with an output path; this would truncate the input",
                        r
                    );
                }
                Some(r)
            };

            let use_append = open_mode
                .as_deref()
                .map(|m| m.eq_ignore_ascii_case("append"))
                .unwrap_or(false);

            let (out, err) =
                open_job_output(uid, gid, use_append, &stdout_resolved, &stderr_resolved)
                    .context("failed to open job output files")?;

            let stdin_fd = match stdin_resolved {
                None => None,
                Some(ref resolved) => {
                    if uid > 0 {
                        use std::os::unix::fs::MetadataExt;
                        let meta = std::fs::metadata(resolved)
                            .with_context(|| format!("stdin file not found: {}", resolved))?;
                        let (fuid, fgid, mode) = (meta.uid(), meta.gid(), meta.mode());
                        let readable = (fuid == uid && mode & 0o400 != 0)
                            || (fgid == gid && mode & 0o040 != 0)
                            || (mode & 0o004 != 0);
                        if !readable {
                            anyhow::bail!("stdin file {} is not readable by uid {}", resolved, uid);
                        }
                    }
                    let f = std::fs::File::open(resolved)
                        .with_context(|| format!("failed to open stdin file: {}", resolved))?;
                    Some(OwnedFd::from(f))
                }
            };

            JobIo::File {
                stdin: stdin_fd,
                stdout: OwnedFd::from(out),
                stderr: OwnedFd::from(err),
            }
        }
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

    // Run SPANK Init/TaskInit against a handle seeded with the assembled env,
    // then fold plugin edits back so both the container and command paths pick
    // them up. Hooks run in the spurd (root) process, not the forked task.
    if let Some(spank) = spank {
        let context = SpankContext {
            job_id,
            uid,
            gid,
            ..Default::default()
        };
        let mut handle = SpankHandle::new(context, env);
        for hook in [spur_spank::SpankHook::Init, spur_spank::SpankHook::TaskInit] {
            if let Err(e) = spank.invoke_hook(hook, &mut handle) {
                warn!(job_id, error = %e, "SPANK hook failed");
            }
        }
        env = handle.env;
    }

    // Container jobs: use explicit fork() + container_init() instead of bash wrapper.
    if let Some(ctn) = container {
        if !stdin_path.is_empty() && matches!(job_io, JobIo::File { .. }) {
            warn!(
                job_id,
                "stdin redirection is not supported for container jobs, ignoring"
            );
        }
        let (job, pty_master) = launch_container_job(cfg, ctn, &env, job_io).await?;
        return Ok(LaunchResult {
            job,
            stdout_path: stdout_resolved,
            stderr_path: stderr_resolved,
            pty_master,
        });
    }

    // --- Non-container jobs: existing tokio::Command path ---

    // Issue #99: If root, wrap job with namespace isolation.
    let use_namespaces = nix::unistd::geteuid().is_root();
    let (launch_cmd, launch_args) = if use_namespaces {
        let wrapper_path = spool_dir.join("spur_ns.sh");
        let visible_devices = cfg
            .host_device_plan
            .as_ref()
            .map(|p| p.visible_devices.as_slice())
            .unwrap_or(&[]);
        let wrapper = build_namespace_wrapper(uid, gid, visible_devices, &script_path);
        write_job_scratch(&wrapper_path, &wrapper, uid, gid)?;
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
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

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

    // RLIMIT_MEMLOCK: raise before privilege drop so RDMA/NCCL ibv_reg_mr works.
    let memlock = cfg.memlock;
    unsafe {
        cmd.pre_exec(move || {
            apply_memlock(memlock);
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

    // Wire job I/O (file dup2 or PTY setsid+TIOCSCTTY+dup2) in the child.
    let raw_io = job_io.raw();
    unsafe {
        cmd.pre_exec(move || raw_io.wire());
    }

    let child = cmd.spawn().context("failed to spawn job process")?;

    // Drop the slave fd immediately so the master gets EOF when the child exits.
    let pty_master = job_io.into_master();

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

    Ok(LaunchResult {
        job: RunningJob::Managed { child, cgroup_path },
        stdout_path: stdout_resolved,
        stderr_path: stderr_resolved,
        pty_master,
    })
}

/// Set up a cgroups v2 hierarchy for a job.
fn setup_cgroup(
    job_id: JobId,
    cpus: u32,
    memory_mb: u64,
    cpu_ids: &[u32],
) -> anyhow::Result<Option<PathBuf>> {
    let cgroup_root = PathBuf::from(CGROUP_ROOT);
    let cgroup_path = cgroup_root.join(format!("job_{}", job_id));

    // Delegate controllers to children: in cgroup-v2 a child only gets
    // memory.*/cpu.*/pids.* files if the parent lists them in subtree_control;
    // without this the per-job memory limit is never enforced. Root failure fatal.
    if let Err(e) = std::fs::create_dir_all(&cgroup_root) {
        if nix::unistd::geteuid().is_root() {
            anyhow::bail!("cgroup root creation failed as root: {}", e);
        }
        warn!(job_id, error = %e, "cgroup creation failed (not root), running without isolation");
        return Ok(None);
    }
    let subtree = cgroup_root.join("cgroup.subtree_control");
    for ctrl in ["+memory", "+cpu", "+pids", "+cpuset"] {
        if let Err(e) = std::fs::write(&subtree, ctrl) {
            warn!(job_id, controller = ctrl, error = %e, "failed to delegate cgroup controller");
        }
    }
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

/// Whether the job's cgroup recorded an OOM kill (cgroup-v2 `memory.events`).
/// False if the file is absent/unreadable. Call before `cleanup_cgroup`.
pub fn cgroup_oom_killed(cgroup_path: &Path) -> bool {
    let Ok(events) = std::fs::read_to_string(cgroup_path.join("memory.events")) else {
        return false;
    };
    events.lines().any(|line| {
        let mut it = line.split_whitespace();
        matches!((it.next(), it.next()), (Some("oom_kill"), Some(n)) if n != "0")
    })
}

/// Kill any leftover processes in the job's cgroup and remove the directory.
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

/// Whether output file/dir creation must be performed as the submitting user.
/// Only meaningful when spurd is root and the job targets a non-root user.
fn should_run_as_user(uid: u32) -> bool {
    uid > 0 && nix::unistd::geteuid().is_root()
}

/// A user's credentials with the supplementary group list already resolved.
struct UserCreds {
    uid: nix::unistd::Uid,
    gid: nix::unistd::Gid,
    groups: Vec<nix::unistd::Gid>,
}

/// Resolve the user's supplementary groups in the parent, before any fork:
/// `getpwuid_r`/`getgrouplist` allocate and lock, so they're unsafe between fork
/// and exec in a multithreaded process. Leaves the child only async-signal-safe
/// syscalls (`apply_user_creds`). Falls back to the primary gid if unresolved.
fn resolve_user_creds(uid: u32, gid: u32) -> UserCreds {
    let gid = nix::unistd::Gid::from_raw(gid);
    let groups = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
        .ok()
        .flatten()
        .and_then(|u| std::ffi::CString::new(u.name).ok())
        .and_then(|name| nix::unistd::getgrouplist(&name, gid).ok())
        .unwrap_or_else(|| vec![gid]);
    UserCreds {
        uid: nix::unistd::Uid::from_raw(uid),
        gid,
        groups,
    }
}

/// Drop to the submitting user in a forked child using pre-resolved credentials.
/// Groups before gid before uid, since each drop removes the privilege the prior
/// step needs. Only setgroups/setgid/setuid run here — all async-signal-safe.
fn apply_user_creds(creds: &UserCreds) -> nix::Result<()> {
    nix::unistd::setgroups(&creds.groups)?;
    nix::unistd::setgid(creds.gid)?;
    nix::unistd::setuid(creds.uid)?;
    Ok(())
}

/// Open a single output file, creating parent directories. Runs in whatever
/// credentials the caller holds — as the submitting user when invoked from the
/// forked helper.
fn open_output_file(path: &str, use_append: bool) -> std::io::Result<std::fs::File> {
    if let Some(parent) = Path::new(path).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true);
    if use_append {
        opts.append(true);
    } else {
        opts.truncate(true);
    }
    opts.open(path)
}

/// Send file descriptors to a peer over a Unix socket via SCM_RIGHTS.
fn send_fds(sock: RawFd, fds: &[RawFd]) -> nix::Result<()> {
    use nix::sys::socket::{sendmsg, ControlMessage, MsgFlags};
    let iov = [std::io::IoSlice::new(b"F")];
    let cmsgs = [ControlMessage::ScmRights(fds)];
    sendmsg::<()>(sock, &iov, &cmsgs, MsgFlags::empty(), None)?;
    Ok(())
}

/// Receive file descriptors sent via SCM_RIGHTS. Returns an empty vec if the
/// peer closed without sending (e.g. the helper failed before passing fds).
fn recv_fds(sock: RawFd) -> nix::Result<Vec<OwnedFd>> {
    use nix::sys::socket::{recvmsg, ControlMessageOwned, MsgFlags};
    let mut buf = [0u8; 8];
    let mut iov = [std::io::IoSliceMut::new(&mut buf)];
    let mut cmsg = nix::cmsg_space!([RawFd; 2]);
    let msg = recvmsg::<()>(sock, &mut iov, Some(&mut cmsg), MsgFlags::empty())?;
    let mut fds = Vec::new();
    for cmsg in msg.cmsgs()? {
        if let ControlMessageOwned::ScmRights(received) = cmsg {
            for fd in received {
                fds.push(unsafe { OwnedFd::from_raw_fd(fd) });
            }
        }
    }
    Ok(fds)
}

/// Open a job's stdout/stderr, creating parent directories.
///
/// When spurd is root and the job targets a non-root user, a forked child drops
/// to the user's credentials before touching the filesystem and passes the open
/// fds back over a socketpair. Resolving paths as the user (not root) is what
/// prevents a job from coercing root into creating, truncating, or owning files
/// outside the user's reach; it also makes the files user-owned without a chown.
/// Otherwise the files are opened in-process.
fn open_job_output(
    uid: u32,
    gid: u32,
    use_append: bool,
    stdout_path: &str,
    stderr_path: &str,
) -> anyhow::Result<(std::fs::File, std::fs::File)> {
    // When stderr follows stdout (same resolved path, e.g. `srun -o` with no
    // `-e`), stderr must share stdout's open file description via dup so both
    // streams advance a single shared write offset and interleave correctly.
    // Opening the path a second time would give stderr an independent offset,
    // and subsequent stdout writes would clobber whatever stderr wrote.
    let shared = stderr_path == stdout_path;

    if !should_run_as_user(uid) {
        let out = open_output_file(stdout_path, use_append).context("open stdout")?;
        let err = if shared {
            out.try_clone().context("clone stdout fd for stderr")?
        } else {
            open_output_file(stderr_path, use_append).context("open stderr")?
        };
        return Ok((out, err));
    }

    // Resolve credentials before the fork; see resolve_user_creds.
    let creds = resolve_user_creds(uid, gid);

    use nix::sys::socket::{socketpair, AddressFamily, SockFlag, SockType};
    let (parent_sock, child_sock) = socketpair(
        AddressFamily::Unix,
        SockType::Datagram,
        None,
        SockFlag::empty(),
    )
    .context("socketpair for output fd passing")?;

    match unsafe { nix::unistd::fork().context("fork for output open")? } {
        nix::unistd::ForkResult::Child => {
            // CRITICAL: post-fork, so synchronous + async-signal-safe only
            // (tokio is broken here). _exit skips atexit/stdio flushing that
            // could deadlock on a lock a sibling thread held at fork time.
            // Exit codes distinguish failure stages.
            drop(parent_sock);
            let code = 'open: {
                if apply_user_creds(&creds).is_err() {
                    break 'open 1;
                }
                let Ok(out) = open_output_file(stdout_path, use_append) else {
                    break 'open 2;
                };
                // Same fd (dup) when stderr follows stdout; SCM_RIGHTS preserves
                // the shared open file description, so both land one offset.
                let err = if shared {
                    match out.try_clone() {
                        Ok(f) => f,
                        Err(_) => break 'open 3,
                    }
                } else {
                    match open_output_file(stderr_path, use_append) {
                        Ok(f) => f,
                        Err(_) => break 'open 3,
                    }
                };
                if send_fds(child_sock.as_raw_fd(), &[out.as_raw_fd(), err.as_raw_fd()]).is_err() {
                    break 'open 4;
                }
                0
            };
            unsafe { libc::_exit(code) };
        }
        nix::unistd::ForkResult::Parent { child } => {
            drop(child_sock);
            // Reap first: the helper sends the fds before exiting, and a datagram
            // socket buffers them past the sender's lifetime, so we can wait for
            // the exit code and only then read. Recv-first would hang on the
            // failure path — a closed datagram peer yields no reliable EOF.
            let status = nix::sys::wait::waitpid(child, None);
            if !matches!(status, Ok(nix::sys::wait::WaitStatus::Exited(_, 0))) {
                bail!("output helper failed to open job output (status: {status:?})");
            }
            let fds =
                recv_fds(parent_sock.as_raw_fd()).context("receive output fds from helper")?;
            if fds.len() != 2 {
                bail!("output helper returned {} fds, expected 2", fds.len());
            }
            let mut it = fds.into_iter();
            let out = std::fs::File::from(it.next().unwrap());
            let err = std::fs::File::from(it.next().unwrap());
            Ok((out, err))
        }
    }
}

/// Create `dir` and any missing parents as the submitting user (forking to drop
/// privilege when spurd is root), so directory creation resolves symlinks and
/// permissions with the user's authority. Returns whether the tree now exists.
fn create_dir_as_user(dir: &Path, uid: u32, gid: u32) -> bool {
    if !should_run_as_user(uid) {
        return std::fs::create_dir_all(dir).is_ok();
    }
    // Resolve credentials before the fork; see resolve_user_creds.
    let creds = resolve_user_creds(uid, gid);
    match unsafe { nix::unistd::fork() } {
        Ok(nix::unistd::ForkResult::Child) => {
            // _exit skips atexit/stdio flushing, unsafe in a post-fork child.
            let ok = apply_user_creds(&creds).is_ok() && std::fs::create_dir_all(dir).is_ok();
            unsafe { libc::_exit(if ok { 0 } else { 1 }) };
        }
        Ok(nix::unistd::ForkResult::Parent { child }) => {
            matches!(
                nix::sys::wait::waitpid(child, None),
                Ok(nix::sys::wait::WaitStatus::Exited(_, 0))
            )
        }
        Err(_) => false,
    }
}

/// Create a node-local spool directory for a job's scratch files. Prefers
/// `SPOOL_ROOT`; falls back to a temp dir when it isn't writable (e.g. non-root
/// dev runs). When spurd is root and the job targets a user, the dir is handed
/// to that user so the job — which runs as the user — can traverse it.
fn create_job_spool_dir(job_id: JobId, uid: u32, gid: u32) -> anyhow::Result<PathBuf> {
    let mut last_err = None;
    for base in [PathBuf::from(SPOOL_ROOT), std::env::temp_dir().join("spur")] {
        let dir = base.join(format!("job{}", job_id));
        match std::fs::create_dir_all(&dir) {
            Ok(()) => {
                if should_run_as_user(uid) {
                    use nix::unistd::{Gid, Uid};
                    // Path-based chown is safe here: the spool tree is
                    // root-owned, not user-controlled, so no symlink TOCTOU.
                    let _ = nix::unistd::chown(
                        &dir,
                        Some(Uid::from_raw(uid)),
                        Some(Gid::from_raw(gid)),
                    );
                }
                return Ok(dir);
            }
            Err(e) => last_err = Some(e),
        }
    }
    bail!("failed to create job spool dir: {last_err:?}")
}

/// Private per-job directory for srun step scripts under the step work dir.
pub(crate) fn prepare_step_script_dir(
    work_dir: &str,
    job_id: JobId,
    uid: u32,
    gid: u32,
) -> anyhow::Result<PathBuf> {
    let dir = PathBuf::from(work_dir).join(format!(".spur_step_{job_id}"));
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        if should_run_as_user(uid) {
            use nix::unistd::{Gid, Uid};
            nix::unistd::chown(&dir, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid)))
                .with_context(|| format!("chown {}", dir.display()))?;
        }
    }
    Ok(dir)
}

/// Write a scratch file (job script, namespace wrapper) executable. When spurd
/// is root and the job targets a user, hand ownership to that user and keep the
/// file private (0700), so only the job and root can read it — matching Slurm's
/// batch script handling.
pub(crate) fn write_job_scratch(
    path: &Path,
    content: &str,
    uid: u32,
    gid: u32,
) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, content).with_context(|| format!("write {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    if should_run_as_user(uid) {
        use nix::unistd::{Gid, Uid};
        nix::unistd::chown(path, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid)))
            .with_context(|| format!("chown {}", path.display()))?;
    }
    Ok(())
}

/// Remove a job's spool directory (best-effort), mirroring Slurm purging its
/// batchdir after completion. Tries both candidate roots since the fallback
/// location isn't recorded.
pub fn cleanup_job_spool(job_id: JobId) {
    for base in [PathBuf::from(SPOOL_ROOT), std::env::temp_dir().join("spur")] {
        let _ = std::fs::remove_dir_all(base.join(format!("job{}", job_id)));
    }
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
    job_io: JobIo,
) -> anyhow::Result<(RunningJob, Option<OwnedFd>)> {
    let job_id = cfg.job_id;
    let cgroup_path = setup_cgroup(job_id, cfg.cpus, cfg.memory_mb, &cfg.cpu_ids)?;

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

    // Snapshot raw I/O fds before fork — the Copy JobIoRaw can be used
    // in the child without owning the fds (parent's OwnedFds keep them alive
    // across the fork boundary).
    let raw_io = job_io.raw();

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
            unsafe {
                libc::close(ready_r);
            }

            // Reset signal handlers
            unsafe {
                libc::signal(libc::SIGCHLD, libc::SIG_DFL);
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            }

            unsafe {
                if let Err(e) = raw_io.wire() {
                    let msg = format!("E:stdio wire failed: {:#}", e);
                    let _ = libc::write(ready_w, msg.as_ptr() as *const _, msg.len());
                    libc::_exit(1);
                }
            }

            crate::container::close_inherited_fds(ready_w);

            // RLIMIT_MEMLOCK: raise while still root, before container_init drops privileges.
            apply_memlock(cfg.memlock);

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

            // Pick a shell that exists in the container
            let shell = if Path::new("/bin/bash").exists() {
                "/bin/bash"
            } else {
                "/bin/sh"
            };
            let c_shell = CString::new(shell).unwrap();
            let exec_args: Vec<CString> = if let Some(ref ep) = entrypoint {
                let cmd = format!("{} && {} /tmp/spur_job_{}.sh", ep, shell, job_id);
                vec![
                    c_shell.clone(),
                    CString::new("-c").unwrap(),
                    CString::new(cmd).unwrap(),
                ]
            } else {
                vec![
                    c_shell.clone(),
                    CString::new(format!("/tmp/spur_job_{}.sh", job_id)).unwrap(),
                ]
            };
            let exec_arg_refs: Vec<&std::ffi::CStr> =
                exec_args.iter().map(|s| s.as_c_str()).collect();

            let _ = nix::unistd::execve(&c_shell, &exec_arg_refs, &c_env_refs);
            eprintln!("spur: execve failed: {}", std::io::Error::last_os_error());
            std::process::exit(1);
        }

        nix::unistd::ForkResult::Parent { child } => {
            unsafe {
                libc::close(ready_w);
            }

            // Drop the slave fd immediately so the master gets EOF when the child exits.
            let pty_master = job_io.into_master();

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

            Ok((
                RunningJob::Forked {
                    pid: child_pid,
                    _pidfd: pidfd,
                    cgroup_path,
                    reaped: false,
                },
                pty_master,
            ))
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

    // These exercise the in-process (non-fork) branch of the helpers: as a
    // non-root test runner, should_run_as_user() is false, so no privilege drop
    // or fork happens and behaviour is deterministic regardless of the test uid.

    #[test]
    fn create_dir_as_user_creates_full_tree() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b/c");
        let uid = nix::unistd::getuid().as_raw();
        let gid = nix::unistd::getgid().as_raw();
        assert!(create_dir_as_user(&nested, uid, gid));
        assert!(nested.is_dir());
        // Idempotent over an existing tree.
        assert!(create_dir_as_user(&nested, uid, gid));
    }

    #[test]
    fn open_job_output_creates_files_and_parent_dirs() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("sub/nested/job.out");
        let err = dir.path().join("sub/nested/job.err");
        let uid = nix::unistd::getuid().as_raw();
        let gid = nix::unistd::getgid().as_raw();
        let (mut of, mut ef) = open_job_output(
            uid,
            gid,
            false,
            out.to_str().unwrap(),
            err.to_str().unwrap(),
        )
        .unwrap();
        of.write_all(b"o").unwrap();
        ef.write_all(b"e").unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "o");
        assert_eq!(std::fs::read_to_string(&err).unwrap(), "e");
    }

    #[test]
    fn open_job_output_append_preserves_existing_content() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("a.out");
        let err = dir.path().join("a.err");
        let uid = nix::unistd::getuid().as_raw();
        let gid = nix::unistd::getgid().as_raw();
        let (op, ep) = (out.to_str().unwrap(), err.to_str().unwrap());

        let (mut of, _ef) = open_job_output(uid, gid, false, op, ep).unwrap();
        of.write_all(b"first\n").unwrap();
        drop(of);

        let (mut of, _ef) = open_job_output(uid, gid, true, op, ep).unwrap();
        of.write_all(b"second\n").unwrap();
        drop(of);

        assert_eq!(std::fs::read_to_string(&out).unwrap(), "first\nsecond\n");
    }

    #[test]
    fn open_job_output_truncate_replaces_existing_content() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("t.out");
        let err = dir.path().join("t.err");
        let uid = nix::unistd::getuid().as_raw();
        let gid = nix::unistd::getgid().as_raw();
        let (op, ep) = (out.to_str().unwrap(), err.to_str().unwrap());

        let (mut of, _ef) = open_job_output(uid, gid, false, op, ep).unwrap();
        of.write_all(b"old content").unwrap();
        drop(of);

        let (mut of, _ef) = open_job_output(uid, gid, false, op, ep).unwrap();
        of.write_all(b"new").unwrap();
        drop(of);

        assert_eq!(std::fs::read_to_string(&out).unwrap(), "new");
    }

    #[test]
    fn open_job_output_shared_path_shares_offset() {
        // `srun -o file` with no `-e` makes stderr follow stdout (same path).
        // stderr must share stdout's fd (dup) so the two streams advance one
        // offset and interleave; independent offsets would clobber each other.
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("job.out");
        let uid = nix::unistd::getuid().as_raw();
        let gid = nix::unistd::getgid().as_raw();
        let p = shared.to_str().unwrap();

        let (mut of, mut ef) = open_job_output(uid, gid, false, p, p).unwrap();
        // Interleave: an out write after an err write must not overwrite it.
        of.write_all(b"out1\n").unwrap();
        of.flush().unwrap();
        ef.write_all(b"err1\n").unwrap();
        ef.flush().unwrap();
        of.write_all(b"out2\n").unwrap();
        of.flush().unwrap();
        ef.write_all(b"err2\n").unwrap();
        ef.flush().unwrap();

        let contents = std::fs::read_to_string(&shared).unwrap();
        assert_eq!(
            contents, "out1\nerr1\nout2\nerr2\n",
            "streams clobbered: {contents:?}"
        );
    }

    #[test]
    fn write_job_scratch_is_executable_and_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spur_job.sh");
        let uid = nix::unistd::getuid().as_raw();
        let gid = nix::unistd::getgid().as_raw();
        write_job_scratch(&path, "#!/bin/bash\necho hi\n", uid, gid).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "#!/bin/bash\necho hi\n"
        );
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn job_spool_dir_round_trips_create_and_cleanup() {
        let uid = nix::unistd::getuid().as_raw();
        let gid = nix::unistd::getgid().as_raw();
        // A job id unlikely to collide with a real job on the test host; as a
        // non-root runner this resolves to the temp-dir fallback.
        let job_id: JobId = 987_654_321;
        let dir = create_job_spool_dir(job_id, uid, gid).unwrap();
        assert!(dir.is_dir());
        write_job_scratch(&dir.join("spur_job.sh"), "x", uid, gid).unwrap();
        cleanup_job_spool(job_id);
        assert!(!dir.exists());
    }

    // send_fds/recv_fds are process-agnostic: they pass fds over any Unix
    // socket. Exercising the SCM_RIGHTS round-trip over an in-process socketpair
    // covers the fd-passing logic without needing root or a fork.
    #[test]
    fn send_recv_fds_round_trips_an_open_file() {
        use nix::sys::socket::{socketpair, AddressFamily, SockFlag, SockType};
        use std::io::{Read, Seek, Write};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("passed.txt");
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        file.write_all(b"from-sender").unwrap();

        let (a, b) = socketpair(
            AddressFamily::Unix,
            SockType::Datagram,
            None,
            SockFlag::empty(),
        )
        .unwrap();

        send_fds(a.as_raw_fd(), &[file.as_raw_fd()]).unwrap();
        let received = recv_fds(b.as_raw_fd()).unwrap();
        assert_eq!(received.len(), 1);

        // The received fd refers to the same open file description: writes made
        // through it land in the same file the sender opened.
        let mut got = std::fs::File::from(received.into_iter().next().unwrap());
        got.write_all(b"-and-more").unwrap();
        got.flush().unwrap();

        let mut contents = String::new();
        file.rewind().unwrap();
        file.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "from-sender-and-more");
    }

    #[test]
    fn recv_fds_returns_empty_when_no_fds_sent() {
        use nix::sys::socket::{sendmsg, socketpair, AddressFamily, MsgFlags, SockFlag, SockType};

        let (a, b) = socketpair(
            AddressFamily::Unix,
            SockType::Datagram,
            None,
            SockFlag::empty(),
        )
        .unwrap();

        // A payload with no ancillary data — mirrors a helper that reported
        // success framing but attached no descriptors.
        let iov = [std::io::IoSlice::new(b"F")];
        sendmsg::<()>(a.as_raw_fd(), &iov, &[], MsgFlags::empty(), None).unwrap();

        let received = recv_fds(b.as_raw_fd()).unwrap();
        assert!(received.is_empty());
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
    fn cgroup_oom_killed_parses_memory_events() {
        let dir = tempfile::tempdir().unwrap();
        // Missing file (no cgroup isolation) -> not OOM.
        assert!(!cgroup_oom_killed(dir.path()));
        // oom_kill 0 -> not OOM.
        std::fs::write(
            dir.path().join("memory.events"),
            "low 0\nhigh 0\nmax 5\noom 0\noom_kill 0\n",
        )
        .unwrap();
        assert!(!cgroup_oom_killed(dir.path()));
        // oom_kill > 0 -> OOM.
        std::fs::write(
            dir.path().join("memory.events"),
            "low 0\nhigh 0\nmax 12\noom 1\noom_kill 1\n",
        )
        .unwrap();
        assert!(cgroup_oom_killed(dir.path()));
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

    #[test]
    fn test_burst_buffer_capacity_directive_ignored_by_wrapper() {
        // The controller consumes `capacity=NNN`; the agent's stage wrapper must
        // ignore it (it's not a stage_in/stage_out command) and only act on the
        // stage directive. The shared parser owns the capacity grammar.
        let script = "#!/bin/bash\necho run\n";
        let bb = "capacity=128;stage_in:cp /data /tmp";
        let wrapped = wrap_with_burst_buffer(script, bb);
        assert!(wrapped.contains("cp /data /tmp"));
        assert!(!wrapped.contains("capacity=128"));
        assert_eq!(spur_core::burst_buffer::parse_capacity_gb(bb), 128);
    }

    #[test]
    fn test_burst_buffer_capacity_only_is_passthrough() {
        // A BB spec with only a capacity reservation (no stage commands) leaves
        // the script unwrapped — there is nothing for the agent to run.
        let script = "#!/bin/bash\necho run\n";
        let wrapped = wrap_with_burst_buffer(script, "capacity=64");
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

    #[tokio::test]
    async fn jobio_wire_pty() {
        let (master, slave) = crate::pty::openpty_with_winsize(Some(&crate::pty::WindowSize {
            rows: 24,
            cols: 80,
            xpixel: 0,
            ypixel: 0,
        }))
        .expect("openpty");

        let job_io = JobIo::Pty { master, slave };
        let raw = job_io.raw();

        let mut cmd = Command::new("/bin/echo");
        cmd.arg("pty_test_output")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        unsafe {
            cmd.pre_exec(move || raw.wire());
        }

        let mut child = cmd.spawn().expect("spawn");
        let master_fd = job_io.into_master().expect("PTY must have master");

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let mut buf = [0u8; 256];
        let n = unsafe { libc::read(master_fd.as_raw_fd(), buf.as_mut_ptr() as *mut _, buf.len()) };
        assert!(n > 0, "expected output from PTY master");
        let output = String::from_utf8_lossy(&buf[..n as usize]);
        assert!(
            output.contains("pty_test_output"),
            "expected 'pty_test_output' in output, got: {output}"
        );

        let status = child.wait().await.expect("wait");
        assert!(status.success());
    }

    #[tokio::test]
    async fn jobio_wire_file() {
        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("stdout");
        let err_path = dir.path().join("stderr");

        let out_file = std::fs::File::create(&out_path).unwrap();
        let err_file = std::fs::File::create(&err_path).unwrap();

        let job_io = JobIo::File {
            stdin: None,
            stdout: OwnedFd::from(out_file),
            stderr: OwnedFd::from(err_file),
        };
        let raw = job_io.raw();

        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg("echo file_stdout; echo file_stderr >&2")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        unsafe {
            cmd.pre_exec(move || raw.wire());
        }

        let mut child = cmd.spawn().expect("spawn");
        assert!(job_io.into_master().is_none(), "File mode has no master");

        let status = child.wait().await.expect("wait");
        assert!(status.success());

        let stdout = std::fs::read_to_string(&out_path).unwrap();
        let stderr = std::fs::read_to_string(&err_path).unwrap();
        assert!(
            stdout.contains("file_stdout"),
            "expected 'file_stdout' in stdout, got: {stdout}"
        );
        assert!(
            stderr.contains("file_stderr"),
            "expected 'file_stderr' in stderr, got: {stderr}"
        );
    }

    #[test]
    fn wire_file_closes_originals_gt_2() {
        // After wire(), originals > 2 should be closed. Verify by checking that
        // a write to the original fd fails with EBADF.
        use std::os::fd::AsRawFd;
        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("out");
        let err_path = dir.path().join("err");

        let out_file = std::fs::File::create(&out_path).unwrap();
        let err_file = std::fs::File::create(&err_path).unwrap();
        let out_fd = out_file.as_raw_fd();
        let err_fd = err_file.as_raw_fd();

        // Both fds should be > 2 since 0/1/2 are taken.
        assert!(out_fd > 2);
        assert!(err_fd > 2);

        // Fork so we don't corrupt our own stdio.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");

        if pid == 0 {
            let raw = JobIoRaw::File {
                stdin: None,
                stdout: out_fd,
                stderr: err_fd,
            };
            let result = unsafe { raw.wire() };
            // Exit with code 0 on success, 1 on failure.
            std::process::exit(if result.is_ok() { 0 } else { 1 });
        }

        // Parent: wait for child.
        let mut status = 0i32;
        unsafe { libc::waitpid(pid, &mut status, 0) };
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
            "child exited with non-zero status"
        );
    }

    #[test]
    fn wire_file_bad_fd_returns_error() {
        let raw = JobIoRaw::File {
            stdin: None,
            stdout: -1,
            stderr: -1,
        };
        // Fork to avoid clobbering test process stdio.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");

        if pid == 0 {
            let result = unsafe { raw.wire() };
            std::process::exit(if result.is_err() { 0 } else { 1 });
        }

        let mut status = 0i32;
        unsafe { libc::waitpid(pid, &mut status, 0) };
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
            "wire() should have returned an error for bad fd"
        );
    }
}
