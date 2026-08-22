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

/// Typed launch errors so callers can distinguish a broken node from a job that
/// simply cannot run here.
pub enum LaunchError {
    PrologFailed(anyhow::Error),
    /// The node itself cannot host work: an I/O failure in spurd's own spool
    /// tree, so every subsequent job will fail identically.
    NodeFault(anyhow::Error),
    Other(anyhow::Error),
}

impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `{:#}` renders the whole cause chain. Plain `{}` prints only the
        // outermost context, which would reduce a drain reason to "create job
        // spool dir" and drop the errno an operator needs to act on.
        match self {
            Self::PrologFailed(e) => write!(f, "prolog failed: {e:#}"),
            Self::NodeFault(e) => write!(f, "launch failed: {e:#}"),
            Self::Other(e) => write!(f, "{e:#}"),
        }
    }
}

impl LaunchError {
    /// Reason for the agent to drain itself, or `None` when the controller owns
    /// the decision. A prolog failure drains too, but the controller does it,
    /// because only the controller can pair the drain with the hold that stops
    /// the job walking the cluster.
    pub fn drain_reason(&self) -> Option<String> {
        match self {
            Self::NodeFault(_) => Some(self.to_string()),
            Self::PrologFailed(_) | Self::Other(_) => None,
        }
    }
}

impl From<anyhow::Error> for LaunchError {
    fn from(e: anyhow::Error) -> Self {
        Self::Other(e)
    }
}

/// True when the error chain carries a real OS-level I/O failure that the node
/// itself is responsible for.
///
/// An exclusion list, mirroring Slurm's "all others drain the node" default: the
/// spool tree is root-owned and every path under it is built from the job id
/// alone, so a submission cannot steer the errno. Requiring a real
/// `raw_os_error` keeps a plain `anyhow!("...")` out, and `EDQUOT` stays
/// excluded as a property of a user on a shared filesystem, not of the node.
fn is_node_fault_io_error(err: &anyhow::Error) -> bool {
    err.chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(is_node_fault_errno)
}

fn is_node_fault_errno(err: &std::io::Error) -> bool {
    matches!(err.raw_os_error(), Some(errno) if errno != libc::EDQUOT)
}

/// True when `dir` lives in the spool tree spurd owns, as opposed to the
/// world-writable temp fallback [`create_job_spool_dir`] drops to on a non-root
/// dev run. Only the owned tree may condemn a node: `/tmp` exhaustion is
/// something any single job can cause, so draining on it would let one runaway
/// job take the cluster down node by node.
fn is_node_owned_spool(dir: &Path) -> bool {
    dir.starts_with(SPOOL_ROOT)
}

/// Classify a failed write to a job's spool directory. An I/O failure under the
/// node's own spool root condemns the node; anything else is just this job's
/// problem.
///
/// Only spool writes may reach this. Writes to the job's `work_dir` must not use
/// it: that path is user-controlled and frequently a shared mount, where one user
/// filling their quota would otherwise drain every node in turn.
fn classify_spool_error(dir: &Path, err: anyhow::Error) -> LaunchError {
    if is_node_owned_spool(dir) && is_node_fault_io_error(&err) {
        LaunchError::NodeFault(err)
    } else {
        LaunchError::Other(err)
    }
}

use crate::container::ContainerConfig;

/// Cgroup root for slurmd-managed jobs.
const CGROUP_ROOT: &str = "/sys/fs/cgroup/spur";

/// Node-local spool root for spurd's per-job scratch (job script, namespace
/// wrapper). Deliberately off the user's work_dir so these root-side writes
/// never hit an NFS root_squash mount. Mirrors Slurm's SlurmdSpoolDir.
const SPOOL_ROOT: &str = "/var/spool/spur";

/// Candidate spool bases, highest priority first. The user-controlled temp
/// fallback is only used when spurd is NOT root — root must never scan or write
/// a world-reachable base (a user could plant a manifest there and have root
/// trust it verbatim on restart).
fn spool_bases() -> Vec<PathBuf> {
    let mut bases = vec![PathBuf::from(SPOOL_ROOT)];
    if !nix::unistd::geteuid().is_root() {
        bases.push(std::env::temp_dir().join("spur"));
    }
    bases
}

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
    /// Disambiguates the cgroup path across a same-node redispatch of the
    /// same job_id, so displacing an old run can never SIGKILL the new one.
    pub run_attempt: u32,
    pub script: String,
    pub work_dir: String,
    /// Needed to expand `%x`/`%u`/`%N`/`%a`/`%A` in output paths as the controller does.
    pub name: String,
    pub user: String,
    pub node: String,
    pub array_job_id: Option<JobId>,
    pub array_task_id: Option<u32>,
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
    /// Direct multi-rank PMIx launch via a wrapper script (batch `--mpi=pmix`).
    pub pmix_multi_task: bool,
}

pub struct LaunchResult {
    pub job: RunningJob,
    pub stdout_path: String,
    pub stderr_path: String,
    /// Master fd of the PTY (only set when `io_mode == LaunchIo::Pty`).
    pub pty_master: Option<OwnedFd>,
    pub spool_dir: PathBuf,
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

    /// Wire stdin only (stdout/stderr stay as inherited pipe fds).
    ///
    /// Used for batch `--mpi=pmix` multi-rank wrappers: Open MPI's PMIx client
    /// initializes correctly when stdout is a pipe (srun parity) but falls back
    /// to singleton worlds when stdout is dup2'd to a regular file.
    ///
    /// # Safety
    /// Same constraints as [`Self::wire`].
    pub(crate) unsafe fn wire_stdin_only(self) -> std::io::Result<()> {
        match self {
            JobIoRaw::File { stdin, .. } => {
                if let Some(fd) = stdin {
                    crate::pty::checked_dup2(fd, libc::STDIN_FILENO)?;
                    if fd > 2 {
                        libc::close(fd);
                    }
                }
                Ok(())
            }
            JobIoRaw::Pty { .. } => self.wire(),
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
    /// A job re-adopted from a manifest after spurd restarted; the new process
    /// isn't spurd's child, so completion is detected via cgroup population
    /// rather than waitpid. See `reconcile_running_jobs`.
    Resumed {
        pid: i32,
        start_time: u64,
        cgroup_path: Option<PathBuf>,
        exit_status_path: PathBuf,
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
            RunningJob::Resumed { pid, .. } => Some(*pid as u32),
            RunningJob::AllocationOnly => None,
        }
    }

    /// Non-consuming peek at the cgroup path, for recording it in a job
    /// manifest without disturbing `take_cgroup`'s completion-time handoff.
    pub fn cgroup_path(&self) -> Option<&Path> {
        match self {
            RunningJob::Managed { cgroup_path, .. } => cgroup_path.as_deref(),
            RunningJob::Forked { cgroup_path, .. } => cgroup_path.as_deref(),
            RunningJob::Resumed { cgroup_path, .. } => cgroup_path.as_deref(),
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
            RunningJob::Resumed {
                pid,
                start_time,
                cgroup_path,
                exit_status_path,
            } => {
                // A populated cgroup is authoritative proof the job is alive; an
                // empty one, proof it exited. But an unreadable cgroup.events
                // (non-v2 host, or the process was never moved in) is NOT proof
                // of exit — falling straight to "done" there would report every
                // adopted job complete on the first post-restart tick and release
                // its GPUs while the real workload keeps running. Fall back to a
                // direct pid liveness check in that case.
                let done = match cgroup_path {
                    Some(cg) => match cgroup_liveness(cg) {
                        CgroupLiveness::Populated => false,
                        CgroupLiveness::Empty => true,
                        CgroupLiveness::Unknown => !proc_alive(*pid, *start_time),
                    },
                    None => !proc_alive(*pid, *start_time),
                };
                if !done {
                    return Ok(None);
                }
                Ok(Some(
                    read_exit_status(exit_status_path)
                        .map(decode_shell_exit)
                        .unwrap_or((-1, 0)),
                ))
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
            RunningJob::Resumed {
                pid, cgroup_path, ..
            } => {
                match cgroup_path {
                    Some(cg) => cgroup_signal_all(cg, sig),
                    None => kill_process_tree(*pid, sig),
                }
                Ok(())
            }
            RunningJob::AllocationOnly => Ok(()),
        }
    }

    pub fn take_cgroup(&mut self) -> Option<PathBuf> {
        match self {
            RunningJob::Managed { cgroup_path, .. } => cgroup_path.take(),
            RunningJob::Forked { cgroup_path, .. } => cgroup_path.take(),
            RunningJob::Resumed { cgroup_path, .. } => cgroup_path.take(),
            RunningJob::AllocationOnly => None,
        }
    }
}

pub(crate) const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// What the agent still owes a job whose process has ended. Persisted in the
/// manifest and cleared step by step as each obligation is discharged, so a
/// restart that interrupts teardown (e.g. a completion report that never
/// reached the controller) resumes from where it left off instead of re-running
/// an already-done, possibly non-idempotent step like the epilog. Absent the
/// ledger, a retried teardown would run the epilog (GPU reset, scratch purge,
/// SPANK JobEpilog) twice.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct PendingObligations {
    /// Run the epilog + SPANK TaskExit/JobEpilog hooks (and drain on failure).
    #[serde(default = "default_true")]
    pub epilog: bool,
    /// Report the completion to the controller.
    #[serde(default = "default_true")]
    pub report_completion: bool,
    /// Set when the epilog failed, so the still-owed completion report carries a
    /// drain request even if it lands on a later restart (the epilog itself is
    /// no longer owed by then, so the drain intent must be remembered here).
    #[serde(default)]
    pub drain: bool,
}

fn default_true() -> bool {
    true
}

impl Default for PendingObligations {
    fn default() -> Self {
        Self {
            epilog: true,
            report_completion: true,
            drain: false,
        }
    }
}

impl PendingObligations {
    /// True once nothing is owed, so the job's spool/manifest can be removed.
    pub fn all_discharged(&self) -> bool {
        !self.epilog && !self.report_completion
    }
}

/// The record spurd re-adopts a job from after a restart. Split into two
/// concerns: **identity** — how to find the process and confirm it's still the
/// same one — and **obligations** — what the agent still owes the job once its
/// process ends (resource release, epilog, completion report). Structuring the
/// record around "what must still happen" rather than only "what the process
/// looks like" keeps the restart teardown honest: every obligation has an
/// explicit home in the schema instead of being re-derived by the reconcile
/// code, which is where obligations were silently dropped before.
///
/// Written right after a successful spawn so a graceful restart doesn't lose
/// track of (or double-book resources for) a job that's still running, then
/// rewritten as obligations are discharged during teardown.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct JobManifest {
    pub schema_version: u32,
    pub job_id: JobId,
    pub run_attempt: u32,

    // --- Identity: locate and verify the process on restart. ---
    pub pid: i32,
    pub start_time: u64,
    pub cgroup_path: Option<PathBuf>,
    pub forked: bool,
    /// Namespace layout as it was at launch. Recorded rather than re-derived,
    /// because the formula depends on whether the agent was privileged, and a
    /// restarted agent may not match the one that launched the job — getting
    /// these wrong sends exec/attach into the wrong namespaces.
    #[serde(default)]
    pub has_pid_namespace: bool,
    #[serde(default)]
    pub has_user_namespace: bool,
    #[serde(default)]
    pub has_mount_namespace: bool,
    /// Host path to the job's exit-status sentinel (inside rootfs for containers).
    #[serde(default)]
    pub exit_status_path: Option<String>,
    /// Rootfs this job owns and must remove. None for non-container jobs, and
    /// for named containers, which outlive the job.
    #[serde(default)]
    pub rootfs: Option<crate::container::JobRootfs>,

    // --- Obligations: what the agent still owes this job. ---
    /// Remaining teardown steps; each cleared as it is discharged.
    #[serde(default)]
    pub pending: PendingObligations,
    /// The resolved (exit_code, signal), recorded once the process is first seen
    /// dead. A container's exit sentinel lives in the job rootfs, which is torn
    /// down as soon as the job is adopted dead, so the outcome must be captured
    /// here for a report that only lands on a later restart to still be accurate.
    #[serde(default)]
    pub exit: Option<(i32, i32)>,
    /// Resources held by the job, released back to the node on teardown.
    pub cpu_ids: Vec<u32>,
    pub gpu_devices: Vec<u32>,
    pub cpus: u32,
    pub memory_mb: u64,
    /// Context the epilog + completion report need.
    pub uid: u32,
    pub gid: u32,
    /// Owning username, restored on re-adopt so exec/attach access gating still
    /// recognizes the owner of a job that survived a restart.
    #[serde(default)]
    pub user: String,
    pub stdout_path: String,
    pub stderr_path: String,
    pub work_dir: String,
    pub partition: String,
    pub nodelist: String,
    pub mpi: String,
}

fn manifest_path(spool_dir: &Path) -> PathBuf {
    spool_dir.join("manifest.json")
}

pub(crate) fn exit_status_path(spool_dir: &Path) -> PathBuf {
    spool_dir.join("exit_status")
}

/// Write a job's manifest. Best-effort: a failure here just means the job
/// won't survive a spurd restart, same as not having this feature. Written via
/// a temp file + rename so a crash mid-write can't leave a torn manifest.
///
/// Mode 0600: the manifest carries the job's uid/gid/cgroup/resource ids, and a
/// co-located user could open it by its predictable path in the traversable spool dir.
pub fn write_job_manifest(spool_dir: &Path, manifest: &JobManifest) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = manifest_path(spool_dir);
    let tmp = path.with_extension("json.tmp");
    let bytes = match serde_json::to_vec(manifest) {
        Ok(b) => b,
        Err(e) => {
            warn!(job_id = manifest.job_id, error = %e, "failed to serialize job manifest");
            return;
        }
    };
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &path)
    };
    if let Err(e) = write() {
        warn!(job_id = manifest.job_id, error = %e, "failed to write job manifest");
        let _ = std::fs::remove_file(&tmp);
    }
}

/// `/proc/<pid>/stat` field 22 (starttime) — disambiguates a live process
/// from a different one that has since reused the same pid.
pub fn proc_start_time(pid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm (field 2) is parenthesized and may itself contain ')'; split on
    // the last one so the fixed-width fields after it parse safely.
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

fn proc_alive(pid: i32, start_time: u64) -> bool {
    proc_start_time(pid) == Some(start_time)
}

/// Liveness verdict from a cgroup's `cgroup.events`.
#[derive(Debug, PartialEq, Eq)]
enum CgroupLiveness {
    /// `populated 1`: at least one process is still in the cgroup.
    Populated,
    /// `populated 0`: the cgroup exists but is empty — everything has exited.
    Empty,
    /// `cgroup.events` is missing or unreadable. On cgroup-v2 this means the dir
    /// was already removed (empty); but a non-v2/hybrid host, or a job never
    /// moved into the cgroup, also lands here — so it is NOT proof of exit and
    /// the caller must fall back to a direct pid liveness check.
    Unknown,
}

/// Read a cgroup-v2 `cgroup.events` to decide whether it still holds processes.
fn cgroup_liveness(cgroup_path: &Path) -> CgroupLiveness {
    let Ok(events) = std::fs::read_to_string(cgroup_path.join("cgroup.events")) else {
        return CgroupLiveness::Unknown;
    };
    if events.lines().any(|line| line.trim() == "populated 1") {
        CgroupLiveness::Populated
    } else {
        CgroupLiveness::Empty
    }
}

/// Signal every process in a cgroup, regardless of whether spurd is their
/// parent. SIGKILL uses the atomic `cgroup.kill` (5.14+) when available;
/// other signals, and older kernels, signal each `cgroup.procs` pid directly.
fn cgroup_signal_all(cgroup_path: &Path, sig: Signal) {
    if sig == Signal::SIGKILL && std::fs::write(cgroup_path.join("cgroup.kill"), "1").is_ok() {
        return;
    }
    if let Ok(pids) = std::fs::read_to_string(cgroup_path.join("cgroup.procs")) {
        for pid_str in pids.lines() {
            if let Ok(pid) = pid_str.trim().parse::<i32>() {
                let _ = signal::kill(Pid::from_raw(pid), sig);
            }
        }
    }
}

/// Decode a shell `$?` value the way `decode_wait_status` decodes a real wait
/// status: 128+N means death by signal N.
fn decode_shell_exit(raw: i32) -> (i32, i32) {
    if (129..=192).contains(&raw) {
        (0, raw - 128)
    } else {
        (raw, 0)
    }
}

fn read_exit_status(path: &Path) -> Option<i32> {
    // The container sentinel lives in a job-writable rootfs, so it is fully
    // untrusted input on the restart path. O_NONBLOCK means opening a planted
    // FIFO can't block spurd startup forever (a blocking open of a writer-less
    // FIFO never returns); O_NOFOLLOW refuses a symlink; the regular-file +
    // single-link fstat check refuses a FIFO/device/hardlink; and the capped
    // read refuses an arbitrarily large file.
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let meta = f.metadata().ok()?;
    if !meta.file_type().is_file() || meta.nlink() != 1 {
        return None;
    }
    let mut buf = String::new();
    f.take(64).read_to_string(&mut buf).ok()?;
    buf.trim().parse().ok()
}

/// Wrap a job script so its exit code survives even if spurd isn't around to
/// `wait()` it. A top-level `trap ... EXIT` fires on any exit — normal end,
/// internal `exit N`, or an uncaught error under `set -e` — and re-exits with
/// the same code so the wrapper's own exit status (what the normal,
/// non-restarted path reports via `Child::try_wait`) is unchanged. Read back by
/// `decode_shell_exit` on resume.
///
/// The trap is prepended rather than wrapping the body in a subshell: bash
/// parses (and runs) a plain script incrementally, so a syntax error deep in the
/// script still runs the lines above it, exactly as an unwrapped job would. A
/// subshell (or brace group) is one compound command that must parse in full
/// before any of it runs, which would change failure semantics for every job on
/// the universal launch path to buy an exit code only on the rare restart path.
pub(crate) fn wrap_with_exit_sentinel(script: &str, exit_status_path: &Path) -> String {
    // Signals need their own traps. bash does run the EXIT trap when the shell
    // is terminated, but `$?` there holds the last *completed* command's status
    // rather than 128+N — so an EXIT trap alone records a job we stopped as
    // having finished successfully. Each handler clears the traps first so the
    // status is written exactly once even though `exit` re-enters EXIT.
    //
    // SIGKILL cannot be trapped; it leaves no sentinel, and the reader treats a
    // missing sentinel as a failure, which is the correct answer there.
    //
    // These stay top-level simple commands so bash's incremental execution
    // still runs them when a later line of the user script fails to parse.
    format!(
        "_spur_rc_file='{path}'\n\
         _spur_exit() {{ trap - EXIT HUP INT QUIT TERM; echo \"$1\" > \"$_spur_rc_file\"; exit \"$1\"; }}\n\
         trap '_spur_exit $?' EXIT\n\
         trap '_spur_exit 129' HUP\n\
         trap '_spur_exit 130' INT\n\
         trap '_spur_exit 131' QUIT\n\
         trap '_spur_exit 143' TERM\n\
         {script}\n",
        path = exit_status_path.display(),
        script = script,
    )
}

/// Outcome of checking one manifest found on startup.
pub enum ReconcileOutcome {
    Alive {
        job: RunningJob,
        manifest: JobManifest,
    },
    /// The process is gone; the controller must not be left waiting forever
    /// for a completion report that will never come. The manifest carries the
    /// resolved exit (in `manifest.exit`) and the still-owed obligations.
    Dead { manifest: JobManifest },
}

/// Find every job manifest left behind by a previous spurd process.
/// Warn when records left by a previous *unprivileged* agent exist but cannot
/// be trusted by this privileged one.
///
/// A root agent deliberately refuses to read the user-writable temp base — a
/// user could plant a manifest there and have root act on it verbatim. But
/// silently skipping it means that correcting a deployment from unprivileged to
/// privileged strands every job that was running under the old agent, with no
/// completion report, no epilog and no resources released. Say so loudly.
fn warn_on_unreadable_foreign_spool() {
    if !nix::unistd::geteuid().is_root() {
        return;
    }
    let foreign = std::env::temp_dir().join("spur");
    let Ok(entries) = std::fs::read_dir(&foreign) else {
        return;
    };
    let stranded: Vec<String> = entries
        .flatten()
        .filter(|e| manifest_path(&e.path()).exists())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    if !stranded.is_empty() {
        warn!(
            base = %foreign.display(),
            count = stranded.len(),
            dirs = ?stranded,
            "ignoring job records written by an unprivileged agent: this agent is privileged and \
             will not trust a user-writable location. Those jobs cannot be adopted, will never \
             report completion, and their resources stay charged until the controller reclaims \
             them. Clean the directory once the processes are gone."
        );
    }
}

pub fn scan_job_manifests() -> Vec<(PathBuf, JobManifest)> {
    warn_on_unreadable_foreign_spool();
    let mut found = Vec::new();
    for base in spool_bases() {
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        for entry in entries.flatten() {
            let spool_dir = entry.path();
            let Ok(bytes) = std::fs::read(manifest_path(&spool_dir)) else {
                continue;
            };
            match serde_json::from_slice::<JobManifest>(&bytes) {
                Ok(manifest) if manifest.schema_version == MANIFEST_SCHEMA_VERSION => {
                    found.push((spool_dir, manifest));
                }
                Ok(manifest) => warn!(
                    job_id = manifest.job_id,
                    version = manifest.schema_version,
                    "skipping job manifest with unknown schema version"
                ),
                Err(e) => warn!(dir = %spool_dir.display(), error = %e, "unreadable job manifest"),
            }
        }
    }
    dedup_manifests_by_job_id(found)
}

/// A job_id should only ever have one manifest (whichever `SPOOL_ROOT`/tmp
/// base `create_job_spool_dir` used), but a stale leftover in the other base
/// (e.g. from a run under a different privilege level) would otherwise make
/// `reconcile_running_jobs` double-restore its allocation. Keep only the
/// highest run_attempt per job_id.
fn dedup_manifests_by_job_id(found: Vec<(PathBuf, JobManifest)>) -> Vec<(PathBuf, JobManifest)> {
    let mut by_job: HashMap<JobId, (PathBuf, JobManifest)> = HashMap::new();
    for (dir, manifest) in found {
        match by_job.get(&manifest.job_id) {
            Some((_, existing)) if existing.run_attempt >= manifest.run_attempt => {
                warn!(
                    job_id = manifest.job_id,
                    dir = %dir.display(),
                    "ignoring duplicate/stale job manifest"
                );
            }
            _ => {
                by_job.insert(manifest.job_id, (dir, manifest));
            }
        }
    }
    by_job.into_values().collect()
}

/// Decide whether a manifested job's process survived the restart.
pub fn reconcile_manifest(spool_dir: &Path, mut manifest: JobManifest) -> ReconcileOutcome {
    let sentinel = manifest
        .exit_status_path
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| exit_status_path(spool_dir));
    // A recorded exit is final. Checking liveness first would let a recycled
    // PID resurrect an already-finished job and re-run its teardown, and no
    // liveness answer can be more authoritative than an outcome we already
    // observed and persisted.
    if manifest.exit.is_some() {
        return ReconcileOutcome::Dead { manifest };
    }
    if proc_alive(manifest.pid, manifest.start_time) {
        let job = RunningJob::Resumed {
            pid: manifest.pid,
            start_time: manifest.start_time,
            cgroup_path: manifest.cgroup_path.clone(),
            exit_status_path: sentinel,
        };
        return ReconcileOutcome::Alive { job, manifest };
    }
    // Resolve the exit only the first time the job is seen dead. On a retry
    // after a partial teardown the sentinel/cgroup may already be gone, so trust
    // the exit recorded in the manifest instead of re-reading and getting -1.
    if manifest.exit.is_none() {
        let (exit_code, mut signal) = read_exit_status(&sentinel)
            .map(decode_shell_exit)
            .unwrap_or((-1, 0));
        // An OOM kill while spurd was down writes no sentinel; recover it from
        // the cgroup so the outcome isn't a bare -1, matching the live monitor.
        if let Some(ref cgroup) = manifest.cgroup_path {
            if cgroup_oom_killed(cgroup) {
                signal |= spur_core::job::OOM_SIGNAL_FLAG;
            }
        }
        manifest.exit = Some((exit_code, signal));
    }
    ReconcileOutcome::Dead { manifest }
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

    spawn_job_process(cfg, spank).await
}

async fn spawn_job_process(
    cfg: &JobLaunchConfig,
    spank: Option<&SpankHost>,
) -> Result<LaunchResult, LaunchError> {
    let JobLaunchConfig {
        job_id,
        run_attempt,
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
    let cgroup_path = setup_cgroup(job_id, run_attempt, cpus, memory_mb, cpu_ids)?;

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
    let spool_dir = create_job_spool_dir(job_id, run_attempt, uid, gid)?;
    let script_path = spool_dir.join("spur_job.sh");
    let wrapped_script = wrap_with_exit_sentinel(script, &exit_status_path(&spool_dir));
    write_job_scratch(&script_path, &wrapped_script, uid, gid)
        .context("failed to write job script")
        .map_err(|e| classify_spool_error(&spool_dir, e))?;
    // Pre-create the exit-status sentinel owned by the job, so the wrapped
    // script can truncate-write it without the spool dir being world-writable.
    write_job_scratch(&exit_status_path(&spool_dir), "", uid, gid)
        .context("failed to create exit-status sentinel")
        .map_err(|e| classify_spool_error(&spool_dir, e))?;

    // Build resolved output paths (empty for PTY mode since output goes to the terminal).
    let (stdout_resolved, stderr_resolved) = if cfg.io_mode == LaunchIo::Pty {
        ("/dev/null".to_string(), "/dev/null".to_string())
    } else {
        (
            resolve_output_path(cfg, work_dir, stdout_path),
            resolve_output_path(cfg, work_dir, stderr_path),
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
                let r = resolve_output_path(cfg, work_dir, stdin_path);
                if r == stdout_resolved || r == stderr_resolved {
                    return Err(anyhow::anyhow!(
                        "stdin path {} overlaps with an output path; this would truncate the input",
                        r
                    )
                    .into());
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
                            return Err(anyhow::anyhow!(
                                "stdin file {} is not readable by uid {}",
                                resolved,
                                uid
                            )
                            .into());
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

    if cfg.pmix_multi_task {
        crate::mpi_plugin::strip_launcher_mpi_env(&mut env);
    }

    // GPU isolation via registry-based device injection plan.
    if let Some(ref plan) = cfg.host_device_plan {
        for (key, value) in &plan.env {
            env.insert(key.clone(), value.clone());
        }
    }

    // Environment-based CPU/thread limiting — works even without cgroups.
    // Well-behaved applications (OpenMP, MKL, PyTorch, etc.) read these.
    if !cfg.pmix_multi_task {
        env.insert("OMP_NUM_THREADS".into(), cpus.to_string());
        env.insert("MKL_NUM_THREADS".into(), cpus.to_string());
        env.insert("OPENBLAS_NUM_THREADS".into(), cpus.to_string());
        env.insert("VECLIB_MAXIMUM_THREADS".into(), cpus.to_string());
        env.insert("NUMEXPR_NUM_THREADS".into(), cpus.to_string());
    }

    // Run SPANK Init/TaskInit against a handle seeded with the assembled env,
    // then fold plugin edits back so both the container and command paths pick
    // them up. Hooks run in the spurd (root) process, not the forked task.
    if let Some(spank) = spank {
        if !cfg.pmix_multi_task {
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
            spool_dir,
        });
    }

    // --- Non-container jobs: existing tokio::Command path ---

    // Issue #99: If root, wrap job with namespace isolation.
    // Batch `--mpi=pmix` multi-rank wrappers must stay in the host mount/PID
    // namespace so Open MPI's PMIx client can reach spurd's embedded server
    // (same as standalone `srun` via `run_command`, which never uses unshare).
    let use_namespaces = nix::unistd::geteuid().is_root() && !cfg.pmix_multi_task;
    let (launch_cmd, launch_args) = if use_namespaces {
        let wrapper_path = spool_dir.join("spur_ns.sh");
        let visible_devices = cfg
            .host_device_plan
            .as_ref()
            .map(|p| p.visible_devices.as_slice())
            .unwrap_or(&[]);
        let wrapper = build_namespace_wrapper(uid, gid, visible_devices, &script_path);
        write_job_scratch(&wrapper_path, &wrapper, uid, gid)
            .map_err(|e| classify_spool_error(&spool_dir, e))?;
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
    let piped_mpi_stdio = cfg.pmix_multi_task && cfg.io_mode == LaunchIo::File;
    let mut cmd = Command::new(&launch_cmd);
    cmd.args(&launch_args).current_dir(work_dir).envs(&env);
    if !cfg.pmix_multi_task {
        cmd.process_group(0);
    }
    if piped_mpi_stdio {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    }

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
    // Must set supplementary groups (video, render) so the process can
    // access GPU device nodes.
    //
    // Issue #128: when use_namespaces is true, the wrapper handles the priv
    // drop *after* unshare runs (via setpriv). Dropping priv here would cause
    // unshare(2) to fail with EPERM since the unprivileged user lacks
    // CAP_SYS_ADMIN.
    if !use_namespaces {
        if let Some(pd) = crate::privdrop::PrivDrop::resolve_if_needed(uid, gid) {
            unsafe {
                cmd.pre_exec(move || {
                    pd.apply()
                        .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
                    Ok(())
                });
            }
            debug!(
                job_id,
                uid, gid, "job will run as non-root user with supplementary groups"
            );
        }
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
    let wire_stdin_only = piped_mpi_stdio;
    unsafe {
        cmd.pre_exec(move || {
            if wire_stdin_only {
                raw_io.wire_stdin_only()
            } else {
                raw_io.wire()
            }
        });
    }

    let mut child = cmd.spawn().context("failed to spawn job process")?;

    if piped_mpi_stdio {
        let shared = stderr_resolved == stdout_resolved;
        let use_append = open_mode
            .as_deref()
            .map(|m| m.eq_ignore_ascii_case("append"))
            .unwrap_or(false);
        spawn_mpi_stdio_drains(
            child.stdout.take(),
            child.stderr.take(),
            MpiStdioDrainOpts {
                uid,
                gid,
                stdout_path: &stdout_resolved,
                stderr_path: &stderr_resolved,
                shared,
                use_append,
            },
        );
    }

    // Drop the slave fd immediately so the master gets EOF when the child exits.
    let pty_master = job_io.into_master();

    // Move process into cgroup. If the move fails the process runs outside the
    // cgroup, so the cgroup can't report its liveness on restart — drop the path
    // so reconcile falls back to a direct pid check instead of misreading an
    // empty cgroup as an exited job.
    let cgroup_path = match (cgroup_path, child.id()) {
        (Some(cgroup), Some(pid)) if move_to_cgroup(&cgroup, pid) => Some(cgroup),
        (Some(_), _) => None,
        (None, _) => None,
    };

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
        spool_dir,
    })
}

/// The cgroup path for one job run. Keyed by job_id *and* run_attempt so a
/// same-node redispatch never shares a cgroup with (and can't accidentally
/// cgroup-wide-kill) a still-finishing prior run.
fn cgroup_path_for(job_id: JobId, run_attempt: u32) -> PathBuf {
    PathBuf::from(CGROUP_ROOT).join(format!("job_{}_{}", job_id, run_attempt))
}

/// Whether `/sys/fs/cgroup` is a unified cgroup-v2 mount. A v1/hybrid host has a
/// tmpfs there instead, so a job dir created under it is an ordinary directory
/// with no `cgroup.events` — which the reconcile path can't tell apart from an
/// exited job. Detecting this up front lets `setup_cgroup` decline to create a
/// fake hierarchy and run without cgroup isolation instead.
fn cgroup_v2_available() -> bool {
    matches!(
        nix::sys::statfs::statfs(Path::new("/sys/fs/cgroup")),
        Ok(s) if s.filesystem_type() == nix::sys::statfs::CGROUP2_SUPER_MAGIC
    )
}

/// Set up a cgroups v2 hierarchy for a job.
fn setup_cgroup(
    job_id: JobId,
    run_attempt: u32,
    cpus: u32,
    memory_mb: u64,
    cpu_ids: &[u32],
) -> anyhow::Result<Option<PathBuf>> {
    // Without a real cgroup-v2 mount, create_dir_all would make a plain
    // directory that never gets a cgroup.events file — indistinguishable on
    // restart from an exited job. Run without isolation rather than lie.
    if !cgroup_v2_available() {
        warn!(job_id, "cgroup v2 not available, running without isolation");
        return Ok(None);
    }
    let cgroup_root = PathBuf::from(CGROUP_ROOT);
    let cgroup_path = cgroup_path_for(job_id, run_attempt);

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

/// Resolve and apply user credentials for container fork children.
/// Delegates to the centralized `PrivDrop` implementation.
fn resolve_user_creds(uid: u32, gid: u32) -> Option<crate::privdrop::PrivDrop> {
    crate::privdrop::PrivDrop::resolve_if_needed(uid, gid)
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

/// Copy a PMIx batch wrapper's piped stdout/stderr into the job output files.
///
/// Open MPI's PMIx client path matches standalone `srun` when stdio is a pipe;
/// dup2'ing stdout/stderr to regular files before `MPI_Init` yields singleton
/// worlds even with correct per-rank `PMIX_*` exports in the wrapper.
struct MpiStdioDrainOpts<'a> {
    uid: u32,
    gid: u32,
    stdout_path: &'a str,
    stderr_path: &'a str,
    shared: bool,
    use_append: bool,
}

fn spawn_mpi_stdio_drains(
    stdout_pipe: Option<impl tokio::io::AsyncRead + Unpin + Send + 'static>,
    stderr_pipe: Option<impl tokio::io::AsyncRead + Unpin + Send + 'static>,
    opts: MpiStdioDrainOpts<'_>,
) {
    let MpiStdioDrainOpts {
        uid,
        gid,
        stdout_path,
        stderr_path,
        shared,
        use_append,
    } = opts;
    let Ok((out, err)) = open_job_output(uid, gid, use_append, stdout_path, stderr_path) else {
        warn!(
            stdout = stdout_path,
            stderr = stderr_path,
            "failed to open PMIx batch output files for pipe drain"
        );
        return;
    };

    if shared {
        let sink = std::sync::Arc::new(tokio::sync::Mutex::new(tokio::fs::File::from_std(out)));
        if let Some(pipe) = stdout_pipe {
            let sink = sink.clone();
            tokio::spawn(async move {
                let mut file = sink.lock().await;
                let _ = tokio::io::copy(&mut tokio::io::BufReader::new(pipe), &mut *file).await;
            });
        }
        if let Some(pipe) = stderr_pipe {
            tokio::spawn(async move {
                let mut file = sink.lock().await;
                let _ = tokio::io::copy(&mut tokio::io::BufReader::new(pipe), &mut *file).await;
            });
        }
    } else {
        if let Some(pipe) = stdout_pipe {
            let mut file = tokio::fs::File::from_std(out);
            tokio::spawn(async move {
                let _ = tokio::io::copy(&mut tokio::io::BufReader::new(pipe), &mut file).await;
            });
        }
        if let Some(pipe) = stderr_pipe {
            let mut file = tokio::fs::File::from_std(err);
            tokio::spawn(async move {
                let _ = tokio::io::copy(&mut tokio::io::BufReader::new(pipe), &mut file).await;
            });
        }
    }
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
                if let Some(ref pd) = creds {
                    if pd.apply().is_err() {
                        break 'open 1;
                    }
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
    // Resolve credentials before the fork.
    let creds = resolve_user_creds(uid, gid);
    match unsafe { nix::unistd::fork() } {
        Ok(nix::unistd::ForkResult::Child) => {
            // _exit skips atexit/stdio flushing, unsafe in a post-fork child.
            let ok = creds.as_ref().map(|c| c.apply().is_ok()).unwrap_or(true)
                && std::fs::create_dir_all(dir).is_ok();
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
/// dev runs). Kept root-owned and 0o711 so a co-located user can't plant a
/// symlink over the root-authored manifest, which is trusted on restart. Keyed
/// by `(job_id, run_attempt)` so a same-node redispatch never reuses a prior
/// run's dir.
fn create_job_spool_dir(
    job_id: JobId,
    run_attempt: u32,
    uid: u32,
    _gid: u32,
) -> Result<PathBuf, LaunchError> {
    let mut failures = Vec::new();
    for base in spool_bases() {
        let dir = base.join(format!("job{}_{}", job_id, run_attempt));
        match std::fs::create_dir_all(&dir) {
            Ok(()) => {
                if should_run_as_user(uid) {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o711));
                }
                return Ok(dir);
            }
            Err(e) => failures.push((dir, e)),
        }
    }
    Err(spool_dir_error(failures))
}

/// Build the error for a spool dir that could not be created under any candidate
/// root. Prefers the owned root's failure over the temp fallback's, since that
/// is the one an operator configured and the only one whose failure condemns the
/// node.
///
/// The `io::Error` must stay a source rather than be formatted into the message:
/// [`is_node_fault_io_error`] detects the fault by walking the chain, so a
/// flattened errno would silently downgrade a node fault to a job failure.
fn spool_dir_error(mut failures: Vec<(PathBuf, std::io::Error)>) -> LaunchError {
    if failures.is_empty() {
        return LaunchError::Other(anyhow::anyhow!("no spool root candidates configured"));
    }
    let chosen = failures
        .iter()
        .position(|(dir, _)| is_node_owned_spool(dir))
        .unwrap_or(0);
    let (dir, err) = failures.swap_remove(chosen);
    let err = anyhow::Error::new(err).context(format!("create job spool dir {}", dir.display()));
    classify_spool_error(&dir, err)
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

/// Remove one run attempt's spool dir. Scoped to `run_attempt` so cleaning a
/// completed run never deletes a concurrent same-node redispatch's live spool.
pub fn cleanup_job_spool(job_id: JobId, run_attempt: u32) {
    let dir = format!("job{}_{}", job_id, run_attempt);
    for base in spool_bases() {
        let _ = std::fs::remove_dir_all(base.join(&dir));
    }
}

/// Mark a job's epilog obligation discharged in its on-disk manifest, recording
/// the resolved exit and any drain intent. Called on the live monitor path only
/// when a completion report fails: the job is already gone from `running`, so a
/// restart re-adopts it from this manifest — without this, the retry would
/// re-run the (possibly non-idempotent) epilog. No-op if the manifest is already
/// gone. Kept off the common success path so a normal completion pays nothing.
pub fn mark_manifest_epilog_discharged(
    job_id: JobId,
    run_attempt: u32,
    exit: (i32, i32),
    drain: bool,
) {
    let dir = format!("job{}_{}", job_id, run_attempt);
    for base in spool_bases() {
        let spool_dir = base.join(&dir);
        let Ok(bytes) = std::fs::read(manifest_path(&spool_dir)) else {
            continue;
        };
        if let Ok(mut manifest) = serde_json::from_slice::<JobManifest>(&bytes) {
            manifest.pending.epilog = false;
            manifest.pending.drain = drain;
            manifest.exit = Some(exit);
            write_job_manifest(&spool_dir, &manifest);
            return;
        }
    }
}

/// Resolve output path patterns (%j → job_id, etc.)
/// Resolve a pattern against the *effective* work_dir (may be the `/tmp`
/// fallback) via the shared resolver, so agent and controller paths match.
fn resolve_output_path(cfg: &JobLaunchConfig, work_dir: &str, pattern: &str) -> String {
    spur_core::job::resolve_output_pattern(
        pattern,
        &spur_core::job::OutputPathContext {
            job_id: cfg.job_id,
            name: &cfg.name,
            user: &cfg.user,
            work_dir,
            node: (!cfg.node.is_empty()).then_some(cfg.node.as_str()),
            array_job_id: cfg.array_job_id,
            array_task_id: cfg.array_task_id,
        },
    )
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
    let cgroup_path = setup_cgroup(
        job_id,
        cfg.run_attempt,
        cfg.cpus,
        cfg.memory_mb,
        &cfg.cpu_ids,
    )?;

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

            // If the move fails the process runs outside the cgroup, so the
            // cgroup can't report its liveness on restart — drop the path so
            // reconcile falls back to a direct pid check (see the Managed path).
            let cgroup_path = match cgroup_path {
                Some(cgroup) if move_to_cgroup(&cgroup, child_pid as u32) => Some(cgroup),
                _ => None,
            };

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

    fn sample_manifest(job_id: JobId, run_attempt: u32) -> JobManifest {
        JobManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            job_id,
            run_attempt,
            pid: 1,
            start_time: 0,
            cgroup_path: None,
            forked: false,
            has_pid_namespace: false,
            has_user_namespace: false,
            has_mount_namespace: false,
            cpu_ids: vec![0, 1],
            gpu_devices: vec![],
            cpus: 2,
            memory_mb: 1024,
            uid: nix::unistd::getuid().as_raw(),
            gid: nix::unistd::getgid().as_raw(),
            user: String::new(),
            stdout_path: "/tmp/out".into(),
            stderr_path: "/tmp/err".into(),
            work_dir: "/tmp".into(),
            partition: "default".into(),
            nodelist: "n1".into(),
            mpi: String::new(),
            rootfs: None,
            exit_status_path: None,
            pending: PendingObligations::default(),
            exit: None,
        }
    }

    /// Frozen at the pre-`rootfs` shape on purpose — never regenerate it. A
    /// manifest written by an older agent must still load, not strand the job.
    #[test]
    fn manifest_without_rootfs_field_still_loads() {
        let frozen = r#"{
            "schema_version": 1, "job_id": 7, "run_attempt": 2,
            "pid": 4242, "start_time": 99, "cgroup_path": null, "forked": true,
            "has_pid_namespace": true, "has_user_namespace": false,
            "has_mount_namespace": true, "exit_status_path": "/var/spool/spur/job7_2/rc",
            "rootfs_mode": "Extracted",
            "cpu_ids": [0, 1], "gpu_devices": [3], "cpus": 2, "memory_mb": 1024,
            "uid": 1000, "gid": 1000, "user": "alice",
            "stdout_path": "/tmp/o", "stderr_path": "/tmp/e", "work_dir": "/tmp",
            "partition": "default", "nodelist": "n1", "mpi": ""
        }"#;

        let m: JobManifest = serde_json::from_str(frozen).expect("old manifest must deserialize");

        assert_eq!(m.job_id, 7);
        assert_eq!(m.run_attempt, 2);
        assert!(m.rootfs.is_none(), "unknown rootfs must not be invented");
        assert!(m.has_mount_namespace);
    }

    /// The teardown path reads the rootfs back off disk, so it has to survive
    /// the real write, not just an in-memory round trip.
    #[test]
    fn recorded_rootfs_survives_the_manifest_write() {
        let spool = tempfile::tempdir().unwrap();
        let mut m = sample_manifest(11, 4);
        m.rootfs = Some(crate::container::JobRootfs {
            base_dir: PathBuf::from("/var/spool/spur/containers/job_11_4"),
            mode: crate::container::RootfsMode::Overlay,
        });

        write_job_manifest(spool.path(), &m);
        let raw = std::fs::read_to_string(spool.path().join("manifest.json")).unwrap();
        let decoded: JobManifest = serde_json::from_str(&raw).unwrap();

        assert_eq!(decoded.rootfs, m.rootfs);
    }

    #[test]
    fn decode_shell_exit_splits_normal_and_signal() {
        assert_eq!(decode_shell_exit(0), (0, 0));
        assert_eq!(decode_shell_exit(5), (5, 0));
        assert_eq!(decode_shell_exit(137), (0, 9)); // 128 + SIGKILL
        assert_eq!(decode_shell_exit(143), (0, 15)); // 128 + SIGTERM
    }

    #[test]
    fn proc_start_time_matches_a_live_process_and_disappears_once_reaped() {
        let mut child = std::process::Command::new("sleep")
            .arg("2")
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let start_time = proc_start_time(pid).expect("live process must have a start time");
        assert!(proc_alive(pid, start_time));
        assert!(!proc_alive(pid, start_time.wrapping_add(1)));

        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(proc_start_time(pid), None);
        assert!(!proc_alive(pid, start_time));
    }

    #[test]
    fn wrap_with_exit_sentinel_captures_normal_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let exit_path = dir.path().join("exit_status");
        let script = wrap_with_exit_sentinel("exit 5", &exit_path);
        let script_path = dir.path().join("script.sh");
        std::fs::write(&script_path, script).unwrap();

        let status = std::process::Command::new("bash")
            .arg(&script_path)
            .status()
            .unwrap();
        // The wrapper's own exit status must match the original script's, so
        // the normal (non-restarted) Managed path's Child::try_wait is
        // unaffected by wrapping.
        assert_eq!(status.code(), Some(5));
        assert_eq!(std::fs::read_to_string(&exit_path).unwrap().trim(), "5");
    }

    #[test]
    fn wrap_with_exit_sentinel_records_terminate_signal_not_a_stale_status() {
        // The shell itself being terminated is the `scancel` path. `true` leaves
        // $? == 0, so an EXIT-only trap would observe that 0 and record a
        // stopped job as a clean success — the defect this guards.
        let dir = tempfile::tempdir().unwrap();
        let exit_path = dir.path().join("exit_status");
        let script = wrap_with_exit_sentinel("true\nkill -TERM $$", &exit_path);
        let script_path = dir.path().join("script.sh");
        std::fs::write(&script_path, script).unwrap();

        let status = std::process::Command::new("bash")
            .arg(&script_path)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(143));
        assert_eq!(std::fs::read_to_string(&exit_path).unwrap().trim(), "143");
    }

    #[test]
    fn wrap_with_exit_sentinel_records_interrupt_signal() {
        let dir = tempfile::tempdir().unwrap();
        let exit_path = dir.path().join("exit_status");
        let script = wrap_with_exit_sentinel("true\nkill -INT $$", &exit_path);
        let script_path = dir.path().join("script.sh");
        std::fs::write(&script_path, script).unwrap();

        let status = std::process::Command::new("bash")
            .arg(&script_path)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(130));
        assert_eq!(std::fs::read_to_string(&exit_path).unwrap().trim(), "130");
    }

    #[test]
    fn wrap_with_exit_sentinel_captures_child_signal_death() {
        // A child command dying by signal makes bash exit 128+N; the EXIT trap
        // sees that in $? and records it, matching how bash already reports an
        // unwrapped script whose last command dies by signal.
        let dir = tempfile::tempdir().unwrap();
        let exit_path = dir.path().join("exit_status");
        let script = wrap_with_exit_sentinel("(kill -TERM $BASHPID; sleep 5)", &exit_path);
        let script_path = dir.path().join("script.sh");
        std::fs::write(&script_path, script).unwrap();

        let status = std::process::Command::new("bash")
            .arg(&script_path)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(143));
        assert_eq!(std::fs::read_to_string(&exit_path).unwrap().trim(), "143");
    }

    #[test]
    fn wrap_with_exit_sentinel_runs_lines_before_a_later_syntax_error() {
        // The universal-path guarantee: a syntax error deep in the script must
        // not prevent the lines above it from running (a subshell/brace-group
        // wrapper parses the whole body first and would run nothing).
        let dir = tempfile::tempdir().unwrap();
        let exit_path = dir.path().join("exit_status");
        let marker = dir.path().join("marker");
        let body = format!("echo ran > {}\nif [\n", marker.display());
        let script = wrap_with_exit_sentinel(&body, &exit_path);
        let script_path = dir.path().join("script.sh");
        std::fs::write(&script_path, script).unwrap();

        let status = std::process::Command::new("bash")
            .arg(&script_path)
            .status()
            .unwrap();
        assert!(!status.success(), "the syntax error still fails the job");
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap().trim(),
            "ran",
            "the line before the syntax error still ran"
        );
    }

    #[test]
    fn cgroup_path_for_disambiguates_run_attempts_of_same_job() {
        // A same-node redispatch must never land the new run in the old
        // run's cgroup, or displacing the old run (cgroup-wide SIGKILL for a
        // Resumed job) would kill the new run too.
        let a = cgroup_path_for(42, 1);
        let b = cgroup_path_for(42, 2);
        assert_ne!(a, b);
        assert_eq!(cgroup_path_for(42, 1), a, "must be deterministic");
    }

    #[test]
    fn cgroup_liveness_reflects_events_file() {
        let dir = tempfile::tempdir().unwrap();
        // Missing file is Unknown (not proof of exit): a non-v2 host or a
        // process never moved in also lands here, so the caller must fall back
        // to a direct pid check rather than assume the job finished.
        assert_eq!(cgroup_liveness(dir.path()), CgroupLiveness::Unknown);
        std::fs::write(dir.path().join("cgroup.events"), "populated 1\nfrozen 0\n").unwrap();
        assert_eq!(cgroup_liveness(dir.path()), CgroupLiveness::Populated);
        std::fs::write(dir.path().join("cgroup.events"), "populated 0\nfrozen 0\n").unwrap();
        assert_eq!(cgroup_liveness(dir.path()), CgroupLiveness::Empty);
    }

    #[test]
    fn resumed_without_cgroup_events_falls_back_to_pid_liveness() {
        // The Issue-1 regression guard: a Resumed job whose cgroup dir has no
        // cgroup.events (non-v2 host, or process never moved in) must NOT be
        // reported complete while its pid is still alive.
        let dir = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let start_time = proc_start_time(pid).unwrap();
        let mut job = RunningJob::Resumed {
            pid,
            start_time,
            cgroup_path: Some(dir.path().to_path_buf()),
            exit_status_path: dir.path().join("exit_status"),
        };
        // Alive pid + unreadable cgroup.events => not done (falls back to pid).
        assert_eq!(job.try_wait().unwrap(), None);
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn cgroup_signal_all_uses_procs_list_for_non_kill_signals() {
        // Non-SIGKILL signals (graceful_cancel's initial SIGTERM,
        // suspend/resume) always go through the per-pid cgroup.procs path —
        // only SIGKILL ever attempts the atomic cgroup.kill fast path.
        let dir = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        std::fs::write(dir.path().join("cgroup.procs"), child.id().to_string()).unwrap();

        cgroup_signal_all(dir.path(), Signal::SIGTERM);

        use std::os::unix::process::ExitStatusExt;
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(15));
    }

    #[test]
    fn cgroup_signal_all_falls_back_when_cgroup_kill_unwritable() {
        // A plain tempdir would let `cgroup.kill` be created as an ordinary
        // (inert) file, masking the fallback. Making the dir read-only
        // forces the write to fail, like a kernel without cgroup.kill (<5.14)
        // would, so this only exercises the intended fallback as non-root.
        if nix::unistd::geteuid().is_root() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        std::fs::write(dir.path().join("cgroup.procs"), child.id().to_string()).unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();

        cgroup_signal_all(dir.path(), Signal::SIGKILL);

        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        use std::os::unix::process::ExitStatusExt;
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(9));
    }

    #[test]
    fn resumed_try_wait_uses_cgroup_population_when_available() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cgroup.events"), "populated 1\nfrozen 0\n").unwrap();
        let exit_path = dir.path().join("exit_status");
        let mut job = RunningJob::Resumed {
            pid: 1,
            start_time: 0,
            cgroup_path: Some(dir.path().to_path_buf()),
            exit_status_path: exit_path.clone(),
        };
        assert_eq!(job.try_wait().unwrap(), None);

        std::fs::write(dir.path().join("cgroup.events"), "populated 0\nfrozen 0\n").unwrap();
        std::fs::write(&exit_path, "3\n").unwrap();
        assert_eq!(job.try_wait().unwrap(), Some((3, 0)));
    }

    #[test]
    fn resumed_try_wait_falls_back_to_proc_liveness_without_cgroup() {
        let mut child = std::process::Command::new("sleep")
            .arg("2")
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let start_time = proc_start_time(pid).unwrap();
        let mut job = RunningJob::Resumed {
            pid,
            start_time,
            cgroup_path: None,
            exit_status_path: PathBuf::from("/nonexistent/exit_status"),
        };
        assert_eq!(job.try_wait().unwrap(), None);

        child.kill().unwrap();
        child.wait().unwrap();
        // No sentinel file for this fallback path -> best-effort -1.
        assert_eq!(job.try_wait().unwrap(), Some((-1, 0)));
    }

    // A resumed container (forked) job reports its real exit code from the
    // in-rootfs sentinel, not an approximate -1.
    #[test]
    fn resumed_try_wait_reads_real_exit_code_from_sentinel() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cgroup.events"), "populated 0\nfrozen 0\n").unwrap();
        let exit_path = dir.path().join("spur_exit_status");
        std::fs::write(&exit_path, "7\n").unwrap();
        let mut job = RunningJob::Resumed {
            pid: 1,
            start_time: 0,
            cgroup_path: Some(dir.path().to_path_buf()),
            exit_status_path: exit_path,
        };
        assert_eq!(job.try_wait().unwrap(), Some((7, 0)));
    }

    // A job-planted symlink at the sentinel path must not be followed by root.
    #[test]
    fn read_exit_status_refuses_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real");
        std::fs::write(&target, "7\n").unwrap();
        let link = dir.path().join("spur_exit_status");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(read_exit_status(&link), None, "symlinked sentinel refused");
        assert_eq!(
            read_exit_status(&target),
            Some(7),
            "a real file still reads"
        );
    }

    // A job-planted FIFO at the sentinel path must not block spurd startup: a
    // blocking read-only open of a writer-less FIFO never returns, and this runs
    // before the gRPC server starts, so it would brick the agent.
    #[test]
    fn read_exit_status_refuses_fifo() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spur_exit_status");
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        // No writer is ever opened; if the guard regressed this call would hang.
        assert_eq!(read_exit_status(&path), None, "FIFO sentinel refused");
    }

    // A job under a writable rootfs can bloat the sentinel; the read is capped.
    #[test]
    fn read_exit_status_reads_only_the_capped_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spur_exit_status");
        // Interior space past the 64-byte cap: unbounded read fails to parse, the
        // capped read sees only the leading "7".
        let body = format!("7{}8", " ".repeat(100));
        std::fs::write(&path, body).unwrap();
        assert_eq!(read_exit_status(&path), Some(7));
    }

    #[test]
    fn write_job_manifest_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        write_job_manifest(dir.path(), &sample_manifest(1, 1));
        let mode = std::fs::metadata(manifest_path(dir.path()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "manifest must not be world/group readable"
        );
    }

    #[test]
    fn reconcile_trusts_a_recorded_exit_over_a_live_pid() {
        // A record whose outcome was already resolved must stay dead even if its
        // PID now belongs to something else, or teardown runs twice.
        let dir = tempfile::tempdir().unwrap();
        let mut m = JobManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            job_id: 7,
            run_attempt: 1,
            // Our own PID, so the liveness check would say "alive" if consulted.
            pid: std::process::id() as i32,
            start_time: proc_start_time(std::process::id() as i32).unwrap(),
            cgroup_path: None,
            forked: false,
            has_pid_namespace: false,
            has_user_namespace: false,
            has_mount_namespace: false,
            exit_status_path: None,
            rootfs: None,
            pending: PendingObligations::default(),
            exit: Some((3, 0)),
            cpu_ids: vec![],
            gpu_devices: vec![],
            cpus: 1,
            memory_mb: 0,
            uid: 0,
            gid: 0,
            user: String::new(),
            stdout_path: String::new(),
            stderr_path: String::new(),
            work_dir: String::new(),
            partition: String::new(),
            nodelist: String::new(),
            mpi: String::new(),
        };
        match reconcile_manifest(dir.path(), m.clone()) {
            ReconcileOutcome::Dead { manifest } => assert_eq!(manifest.exit, Some((3, 0))),
            ReconcileOutcome::Alive { .. } => panic!("a recorded exit must not be re-litigated"),
        }
        // Without a recorded exit the same record is correctly seen as alive.
        m.exit = None;
        assert!(matches!(
            reconcile_manifest(dir.path(), m),
            ReconcileOutcome::Alive { .. }
        ));
    }

    #[test]
    fn manifest_namespace_layout_survives_a_round_trip_and_defaults_safely() {
        // The layout must come from the record, not from the reading agent's own
        // privilege — a privileged agent adopting a job launched by an
        // unprivileged one (or vice versa) would otherwise send exec/attach into
        // the wrong namespaces.
        let m = JobManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            job_id: 7,
            run_attempt: 1,
            pid: 1234,
            start_time: 99,
            cgroup_path: None,
            forked: false,
            has_pid_namespace: true,
            has_user_namespace: false,
            has_mount_namespace: true,
            exit_status_path: None,
            rootfs: None,
            pending: PendingObligations::default(),
            exit: None,
            cpu_ids: vec![],
            gpu_devices: vec![],
            cpus: 1,
            memory_mb: 0,
            uid: 1000,
            gid: 1000,
            user: "vm".into(),
            stdout_path: String::new(),
            stderr_path: String::new(),
            work_dir: String::new(),
            partition: String::new(),
            nodelist: String::new(),
            mpi: String::new(),
        };
        let back: JobManifest = serde_json::from_slice(&serde_json::to_vec(&m).unwrap()).unwrap();
        assert!(back.has_pid_namespace);
        assert!(!back.has_user_namespace);
        assert!(back.has_mount_namespace);

        // A record written before these fields existed must still load, and must
        // default to the conservative "no namespaces" answer rather than failing.
        let mut value: serde_json::Value = serde_json::to_value(&m).unwrap();
        let obj = value.as_object_mut().unwrap();
        obj.remove("has_pid_namespace");
        obj.remove("has_user_namespace");
        obj.remove("has_mount_namespace");
        let older: JobManifest = serde_json::from_value(value).unwrap();
        assert!(!older.has_pid_namespace);
        assert!(!older.has_user_namespace);
        assert!(!older.has_mount_namespace);
    }

    #[test]
    fn manifest_round_trip_scan_and_reconcile_alive() {
        // Serialize against other tests that scan the shared spool-root
        // manifest tree — see MANIFEST_SCAN_TEST_LOCK.
        let _guard = crate::MANIFEST_SCAN_TEST_LOCK.blocking_lock();
        let job_id: JobId = 987_654_324;
        let uid = nix::unistd::getuid().as_raw();
        let gid = nix::unistd::getgid().as_raw();
        let spool_dir = create_job_spool_dir(job_id, 1, uid, gid)
            .unwrap_or_else(|e| panic!("create spool dir: {e}"));

        let mut child = std::process::Command::new("sleep")
            .arg("2")
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let start_time = proc_start_time(pid).unwrap();

        write_job_manifest(
            &spool_dir,
            &JobManifest {
                schema_version: MANIFEST_SCHEMA_VERSION,
                job_id,
                run_attempt: 1,
                pid,
                start_time,
                cgroup_path: None,
                forked: false,
                has_pid_namespace: false,
                has_user_namespace: false,
                has_mount_namespace: false,
                cpu_ids: vec![0, 1],
                gpu_devices: vec![],
                cpus: 2,
                memory_mb: 1024,
                uid,
                gid,
                user: String::new(),
                stdout_path: "/tmp/out".into(),
                stderr_path: "/tmp/err".into(),
                work_dir: "/tmp".into(),
                partition: "default".into(),
                nodelist: "n1".into(),
                mpi: String::new(),
                rootfs: None,
                exit_status_path: None,
                pending: PendingObligations::default(),
                exit: None,
            },
        );

        let (found_dir, found_manifest) = scan_job_manifests()
            .into_iter()
            .find(|(_, m)| m.job_id == job_id)
            .expect("manifest not found by scan");
        assert_eq!(found_dir, spool_dir);
        assert_eq!(found_manifest.cpu_ids, vec![0, 1]);

        match reconcile_manifest(&found_dir, found_manifest) {
            ReconcileOutcome::Alive { job, manifest } => {
                assert_eq!(manifest.job_id, job_id);
                assert!(matches!(job, RunningJob::Resumed { pid: p, .. } if p == pid));
            }
            ReconcileOutcome::Dead { .. } => panic!("expected Alive outcome for a live process"),
        }

        child.kill().unwrap();
        let _ = child.wait();
        cleanup_job_spool(job_id, 1);
    }

    #[test]
    fn reconcile_manifest_dead_reports_exit_status_from_sentinel() {
        let _guard = crate::MANIFEST_SCAN_TEST_LOCK.blocking_lock();
        let job_id: JobId = 987_654_325;
        let uid = nix::unistd::getuid().as_raw();
        let gid = nix::unistd::getgid().as_raw();
        let spool_dir = create_job_spool_dir(job_id, 1, uid, gid)
            .unwrap_or_else(|e| panic!("create spool dir: {e}"));

        // Spawn and fully reap so the pid is verifiably gone before we
        // reconcile, mirroring a job that finished while spurd was down.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        child.wait().unwrap();
        std::fs::write(exit_status_path(&spool_dir), "7\n").unwrap();

        let manifest = JobManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            job_id,
            run_attempt: 1,
            pid,
            start_time: 0,
            cgroup_path: None,
            forked: false,
            has_pid_namespace: false,
            has_user_namespace: false,
            has_mount_namespace: false,
            cpu_ids: vec![],
            gpu_devices: vec![],
            cpus: 1,
            memory_mb: 0,
            uid,
            gid,
            user: String::new(),
            stdout_path: String::new(),
            stderr_path: String::new(),
            work_dir: "/tmp".into(),
            partition: "default".into(),
            nodelist: "n1".into(),
            mpi: String::new(),
            rootfs: None,
            exit_status_path: None,
            pending: PendingObligations::default(),
            exit: None,
        };

        match reconcile_manifest(&spool_dir, manifest) {
            ReconcileOutcome::Dead { manifest } => {
                assert_eq!(manifest.exit, Some((7, 0)))
            }
            ReconcileOutcome::Alive { .. } => panic!("gone process must not be Alive"),
        }
        cleanup_job_spool(job_id, 1);
    }

    // A job OOM-killed while spurd was down writes no sentinel; the OOM flag is
    // recovered from the cgroup so the outcome isn't a bare -1.
    #[test]
    fn reconcile_manifest_dead_recovers_oom_from_cgroup() {
        let _guard = crate::MANIFEST_SCAN_TEST_LOCK.blocking_lock();
        let job_id: JobId = 987_654_332;
        let uid = nix::unistd::getuid().as_raw();
        let gid = nix::unistd::getgid().as_raw();
        let spool_dir = create_job_spool_dir(job_id, 1, uid, gid)
            .unwrap_or_else(|e| panic!("create spool dir: {e}"));
        let cgroup = tempfile::tempdir().unwrap();
        std::fs::write(cgroup.path().join("memory.events"), "oom_kill 1\n").unwrap();

        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        child.wait().unwrap();

        let manifest = JobManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            job_id,
            run_attempt: 1,
            pid,
            start_time: 0,
            cgroup_path: Some(cgroup.path().to_path_buf()),
            forked: false,
            has_pid_namespace: false,
            has_user_namespace: false,
            has_mount_namespace: false,
            cpu_ids: vec![],
            gpu_devices: vec![],
            cpus: 1,
            memory_mb: 0,
            uid,
            gid,
            user: String::new(),
            stdout_path: String::new(),
            stderr_path: String::new(),
            work_dir: "/tmp".into(),
            partition: "default".into(),
            nodelist: "n1".into(),
            mpi: String::new(),
            rootfs: None,
            exit_status_path: None,
            pending: PendingObligations::default(),
            exit: None,
        };

        match reconcile_manifest(&spool_dir, manifest) {
            ReconcileOutcome::Dead { manifest } => {
                let (_, signal) = manifest.exit.expect("dead outcome records the exit");
                assert_ne!(signal & spur_core::job::OOM_SIGNAL_FLAG, 0, "OOM flag set");
            }
            ReconcileOutcome::Alive { .. } => panic!("gone process must not be Alive"),
        }
        cleanup_job_spool(job_id, 1);
    }

    // Distinct run_attempts get distinct spool dirs, and cleaning one attempt
    // must leave a concurrent redispatch's attempt untouched.
    #[test]
    fn spool_dir_cleanup_is_scoped_to_one_run_attempt() {
        let _guard = crate::MANIFEST_SCAN_TEST_LOCK.blocking_lock();
        let job_id: JobId = 987_654_330;
        let uid = nix::unistd::getuid().as_raw();
        let gid = nix::unistd::getgid().as_raw();

        let d1 = create_job_spool_dir(job_id, 1, uid, gid)
            .unwrap_or_else(|e| panic!("create spool dir: {e}"));
        let d2 = create_job_spool_dir(job_id, 2, uid, gid)
            .unwrap_or_else(|e| panic!("create spool dir: {e}"));
        assert_ne!(d1, d2, "distinct run_attempts must get distinct dirs");
        assert!(d1.is_dir() && d2.is_dir());

        cleanup_job_spool(job_id, 1);
        assert!(!d1.exists(), "the cleaned attempt is removed");
        assert!(d2.exists(), "a different attempt's spool must survive");

        cleanup_job_spool(job_id, 2);
        assert!(!d2.exists());
    }

    // write_job_manifest replaces atomically: a leftover temp file is never
    // picked up as a live manifest by the scanner.
    #[test]
    fn manifest_write_leaves_no_temp_file() {
        let _guard = crate::MANIFEST_SCAN_TEST_LOCK.blocking_lock();
        let job_id: JobId = 987_654_331;
        let uid = nix::unistd::getuid().as_raw();
        let gid = nix::unistd::getgid().as_raw();
        let spool_dir = create_job_spool_dir(job_id, 1, uid, gid)
            .unwrap_or_else(|e| panic!("create spool dir: {e}"));

        write_job_manifest(
            &spool_dir,
            &JobManifest {
                schema_version: MANIFEST_SCHEMA_VERSION,
                job_id,
                run_attempt: 1,
                pid: 1,
                start_time: 0,
                cgroup_path: None,
                forked: false,
                has_pid_namespace: false,
                has_user_namespace: false,
                has_mount_namespace: false,
                cpu_ids: vec![],
                gpu_devices: vec![],
                cpus: 1,
                memory_mb: 0,
                uid,
                gid,
                user: String::new(),
                stdout_path: String::new(),
                stderr_path: String::new(),
                work_dir: "/tmp".into(),
                partition: "default".into(),
                nodelist: "n1".into(),
                mpi: String::new(),
                rootfs: None,
                exit_status_path: None,
                pending: PendingObligations::default(),
                exit: None,
            },
        );

        assert!(manifest_path(&spool_dir).is_file());
        assert!(!spool_dir.join("manifest.json.tmp").exists());
        cleanup_job_spool(job_id, 1);
    }

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

    // ── launch error classification / node drain ─────────────────

    fn disk_full_error(context: &str) -> anyhow::Error {
        // Same shape the production paths produce: an io::Error from the
        // filesystem, wrapped by the call site's .context().
        anyhow::Error::new(std::io::Error::from_raw_os_error(libc::ENOSPC))
            .context(context.to_owned())
    }

    fn owned_spool() -> PathBuf {
        PathBuf::from(SPOOL_ROOT).join("job1")
    }

    fn fallback_spool() -> PathBuf {
        std::env::temp_dir().join("spur").join("job1")
    }

    #[test]
    fn spool_disk_exhaustion_is_a_node_fault_and_drains() {
        // create_job_spool_dir / write_job_scratch target SPOOL_ROOT, which
        // spurd owns, so a full filesystem there condemns the node.
        let err = classify_spool_error(&owned_spool(), disk_full_error("create job spool dir"));
        assert!(matches!(err, LaunchError::NodeFault(_)));
        let reason = err.drain_reason().expect("node fault must drain");
        assert!(reason.contains("No space left on device"), "{reason}");
    }

    #[test]
    fn a_full_temp_fallback_spool_does_not_drain() {
        // The fallback root is world-writable, so any single job can fill it.
        // Draining on that would let one runaway job walk the cluster, taking
        // out every node the scheduler retries it on.
        let err = classify_spool_error(&fallback_spool(), disk_full_error("write job script"));
        assert!(matches!(err, LaunchError::Other(_)));
        assert!(
            err.drain_reason().is_none(),
            "a full world-writable /tmp must never drain the node"
        );
    }

    #[test]
    fn exhausted_spool_roots_stay_classifiable_as_a_node_fault() {
        // Every candidate root failing to mkdir is what an exhausted rootfs
        // looks like, since SPOOL_ROOT and the temp fallback usually share a
        // filesystem. Formatting the errno into the message here would hide it
        // from classification, so the node would keep accepting jobs it cannot
        // launch — the retry storm this whole path exists to stop.
        let err = spool_dir_error(vec![
            (
                owned_spool(),
                std::io::Error::from_raw_os_error(libc::ENOSPC),
            ),
            (
                fallback_spool(),
                std::io::Error::from_raw_os_error(libc::ENOSPC),
            ),
        ]);
        assert!(matches!(err, LaunchError::NodeFault(_)));
        let reason = err.drain_reason().expect("node fault must drain");
        assert!(
            reason.contains(&owned_spool().display().to_string()),
            "the configured spool root must be the one named, got: {reason}"
        );
    }

    #[test]
    fn an_errno_rendered_into_the_message_is_not_recoverable() {
        // Why spool_dir_error keeps the io::Error as a source. Classification
        // walks the chain, so an errno turned into text is gone for good; this
        // is how the all-roots-failed path used to lose node faults.
        let flattened = anyhow::anyhow!(
            "failed to create job spool dir: {:?}",
            std::io::Error::from_raw_os_error(libc::ENOSPC)
        );
        assert!(
            !is_node_fault_io_error(&flattened),
            "an errno in the message text must not be mistaken for a real source"
        );
    }

    #[test]
    fn a_failure_confined_to_the_fallback_root_does_not_drain() {
        // Only the world-writable fallback failed. The node's own spool is
        // fine, so this is a job failure, not grounds for taking the node out
        // of service. This is the path check doing the work, not the errno.
        let err = spool_dir_error(vec![(
            fallback_spool(),
            std::io::Error::from_raw_os_error(libc::ENOSPC),
        )]);
        assert!(matches!(err, LaunchError::Other(_)));
        assert!(err.drain_reason().is_none());
    }

    #[test]
    fn an_error_with_no_io_source_never_drains() {
        // Everything under the owned root drains except EDQUOT, so the errno
        // check is what keeps a plain anyhow error out. Without it a container
        // or config problem would start condemning nodes.
        let err =
            classify_spool_error(&owned_spool(), anyhow::anyhow!("spool root not configured"));
        assert!(matches!(err, LaunchError::Other(_)));
        assert!(err.drain_reason().is_none());
    }

    #[test]
    fn a_permission_failure_on_the_owned_spool_root_is_a_node_fault() {
        // The spool tree is root-owned and every path under it is built by
        // spurd from the job id, so a submission cannot steer the errno. EACCES
        // there means the node is misconfigured or its filesystem is broken,
        // and leaving it eligible just feeds it more jobs to fail.
        let err = spool_dir_error(vec![(
            owned_spool(),
            std::io::Error::from_raw_os_error(libc::EACCES),
        )]);
        assert!(matches!(err, LaunchError::NodeFault(_)));
        assert!(err.drain_reason().is_some());
    }

    #[test]
    fn a_hardware_io_error_on_the_owned_spool_root_is_a_node_fault() {
        let err = classify_spool_error(
            &owned_spool(),
            anyhow::Error::new(std::io::Error::from_raw_os_error(libc::EIO))
                .context("write job script"),
        );
        assert!(matches!(err, LaunchError::NodeFault(_)));
    }

    #[test]
    fn write_job_scratch_keeps_the_errno_downcastable() {
        // The whole classification scheme rests on write_job_scratch leaving a
        // real io::Error in the chain. If it ever formatted the errno into its
        // message instead, every classification test above would still pass
        // while production silently stopped draining broken nodes.
        let err = write_job_scratch(
            Path::new("/nonexistent-spur-audit-dir/job.sh"),
            "#!/bin/sh\n",
            0,
            0,
        )
        .expect_err("writing under a nonexistent parent must fail");
        assert!(
            err.chain()
                .any(|c| c.downcast_ref::<std::io::Error>().is_some()),
            "the io::Error must survive as a source, not be flattened into text"
        );
    }

    #[test]
    fn read_only_spool_is_a_node_fault() {
        let err = classify_spool_error(
            &owned_spool(),
            anyhow::Error::new(std::io::Error::from_raw_os_error(libc::EROFS))
                .context("write job script"),
        );
        assert!(matches!(err, LaunchError::NodeFault(_)));
    }

    #[test]
    fn output_file_disk_exhaustion_does_not_drain() {
        // open_job_output writes to paths resolved against the job's work_dir,
        // which is user-controlled and frequently a shared mount. Its errors
        // reach the caller through `?`, i.e. From<anyhow::Error>, so they must
        // classify as Other: draining here would take a healthy node offline,
        // and the scheduler would then repeat it on every remaining node.
        let err: LaunchError = disk_full_error("failed to open job output files").into();
        assert!(matches!(err, LaunchError::Other(_)));
        assert!(
            err.drain_reason().is_none(),
            "a full user filesystem must never drain the node"
        );
    }

    #[test]
    fn user_quota_exhaustion_is_not_a_node_fault() {
        // EDQUOT is a property of a user on a shared filesystem, not of the
        // node, and no quota applies to the root-owned spool tree.
        let err = classify_spool_error(
            &owned_spool(),
            anyhow::Error::new(std::io::Error::from_raw_os_error(libc::EDQUOT))
                .context("write job script"),
        );
        assert!(matches!(err, LaunchError::Other(_)));
        assert!(err.drain_reason().is_none());
    }

    #[test]
    fn a_spool_failure_with_no_errno_does_not_drain() {
        let err =
            classify_spool_error(&owned_spool(), anyhow::anyhow!("container image not found"));
        assert!(matches!(err, LaunchError::Other(_)));
        assert!(err.drain_reason().is_none());
    }

    #[test]
    fn the_agent_does_not_self_drain_on_a_prolog_failure() {
        // The drain still happens, but the controller issues it, so it can pair
        // it with the hold. An agent-side drain would retry the job elsewhere
        // and walk a job-caused failure across the cluster.
        let err = LaunchError::PrologFailed(anyhow::anyhow!("exit status 1"));
        assert!(err.drain_reason().is_none());
        assert_eq!(
            err.to_string(),
            "prolog failed: exit status 1",
            "this text reaches the controller as the launch error and becomes \
             the drain reason there, so it must not be double-prefixed"
        );
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
        // LaunchError has no Debug impl on purpose (it must not be convertible
        // back into an anyhow::Error), so report it through Display.
        let dir = create_job_spool_dir(job_id, 1, uid, gid)
            .unwrap_or_else(|e| panic!("create spool dir: {e}"));

        assert!(dir.is_dir());
        write_job_scratch(&dir.join("spur_job.sh"), "x", uid, gid).unwrap();
        cleanup_job_spool(job_id, 1);
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

    fn launch_cfg_for_paths(job_id: JobId, name: &str, user: &str, node: &str) -> JobLaunchConfig {
        JobLaunchConfig {
            job_id,
            run_attempt: 0,
            script: String::new(),
            work_dir: String::new(),
            name: name.to_string(),
            user: user.to_string(),
            node: node.to_string(),
            array_job_id: None,
            array_task_id: None,
            environment: HashMap::new(),
            stdout_path: String::new(),
            stderr_path: String::new(),
            stdin_path: String::new(),
            cpus: 1,
            memory_mb: 0,
            gpu_devices: Vec::new(),
            cpu_ids: Vec::new(),
            open_mode: None,
            uid: 0,
            gid: 0,
            container: None,
            prolog_script: None,
            partition: String::new(),
            nodelist: String::new(),
            host_device_plan: None,
            memlock: MemlockLimit::Unlimited,
            io_mode: LaunchIo::File,
            pmix_multi_task: false,
        }
    }

    #[test]
    fn test_resolve_output_path() {
        let cfg = launch_cfg_for_paths(42, "train", "alice", "node7");
        assert_eq!(
            resolve_output_path(&cfg, "/home/user", "spur-%j.out"),
            "/home/user/spur-42.out"
        );
        assert_eq!(
            resolve_output_path(&cfg, "/home/user", "/var/log/job-%j.log"),
            "/var/log/job-42.log"
        );
        assert_eq!(resolve_output_path(&cfg, "/tmp", ""), "/tmp/spur-42.out");
        // Same codes as the controller (%x/%u/%N), so reported/computed never diverge.
        assert_eq!(
            resolve_output_path(&cfg, "/tmp", "out-%x-%u-%N.log"),
            "/tmp/out-train-alice-node7.log"
        );
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
