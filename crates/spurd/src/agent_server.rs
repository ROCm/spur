// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! gRPC server implementing the SlurmAgent service.
//! Receives job launch/cancel requests from spurctld.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tonic::{Request, Response, Status};
use tracing::{debug, error, info, warn};

use tokio_stream::wrappers::ReceiverStream;

use spur_proto::proto::slurm_agent_server::SlurmAgent;
use spur_proto::proto::*;

use spur_sched::cons_tres::{AllocError, AllocationResult, NodeAllocation, ReleaseWarrant};

use spur_spank::{SpankContext, SpankHandle, SpankHook, SpankHost};

use spur_core::config::{CgroupConfig, HooksConfig, MpiConfig};
use spur_core::job::RunKey;
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

type TerminalExit = std::pin::Pin<Box<dyn std::future::Future<Output = i32> + Send>>;

/// A shell running under its own supervisor, from the agent's side: the master
/// it bridges, the pid its signals target, and where its exit status arrives.
struct SupervisedTerminal {
    master: std::os::fd::OwnedFd,
    pid: i32,
    exit: TerminalExit,
}

/// What a terminal inherits from the job it opens into. Copied out under one
/// lock so the launch below never holds the running-job table.
struct TerminalJobFacts {
    run_attempt: u32,
    cpus: u32,
    memory_mb: u64,
    gpu_devices: Vec<u32>,
    partition: String,
    nodelist: String,
}

struct StepdLaunchOptions {
    step_id: spur_core::step::StepId,
    allocation_only: bool,
    container_rootfs_mode: Option<crate::container::RootfsMode>,
    hooks: HooksConfig,
    plugstack_path: String,
    pmix: Option<crate::stepd::StepdPmix>,
}

/// `spurstepd` is expected to be installed alongside `spurd`; the bare-name
/// fallback resolves via `execv` (CWD-relative only, no $PATH search).
fn resolve_stepd_executable() -> std::path::PathBuf {
    let co_located = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|dir| dir.join("spurstepd")));
    match co_located {
        Some(path) if path.exists() => path,
        _ => std::path::PathBuf::from("spurstepd"),
    }
}

/// Double-fork+setsid detach (Slurm's `slurmstepd` spawn pattern): the
/// grandchild reports its real pid over a pipe before exec, since
/// `Command::spawn`'s return is just the immediately-exiting intermediate
/// child that lets the grandchild reparent to init.
fn spawn_stepd_process(
    executable: &std::path::Path,
    args: &[std::path::PathBuf],
) -> std::io::Result<u32> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;

    let c_path =
        std::ffi::CString::new(executable.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
    let mut c_args = vec![c_path.clone()];
    for arg in args {
        c_args.push(
            std::ffi::CString::new(arg.as_os_str().as_bytes()).map_err(std::io::Error::other)?,
        );
    }
    let c_arg_refs: Vec<&std::ffi::CStr> = c_args.iter().map(|s| s.as_c_str()).collect();

    let (pipe_r, pipe_w) = nix::unistd::pipe()?;
    let ready_r = pipe_r.as_raw_fd();
    let ready_w = pipe_w.as_raw_fd();

    match unsafe { nix::unistd::fork() }? {
        // === intermediate child: synchronous code only, tokio runtime is
        // broken here after fork. ===
        nix::unistd::ForkResult::Child => {
            unsafe { libc::close(ready_r) };
            match unsafe { nix::unistd::fork() } {
                Ok(nix::unistd::ForkResult::Child) => {
                    // === grandchild: becomes spurstepd ===
                    if nix::unistd::setsid().is_err() {
                        unsafe { libc::_exit(1) };
                    }
                    let pid = std::process::id().to_ne_bytes();
                    unsafe {
                        libc::write(ready_w, pid.as_ptr().cast(), pid.len());
                        // FD_CLOEXEC, not an immediate close: a successful
                        // execv closes this for us (EOF, no error byte);
                        // only a failed execv falls through to report one.
                        libc::fcntl(ready_w, libc::F_SETFD, libc::FD_CLOEXEC);
                    }
                    let _ = nix::unistd::execv(&c_path, &c_arg_refs);
                    unsafe {
                        libc::write(ready_w, [1u8].as_ptr().cast(), 1);
                        libc::close(ready_w);
                    }
                    unsafe { libc::_exit(127) };
                }
                // Intermediate child (or a failed second fork, folded into
                // the same arm): exit so the grandchild orphans to init.
                _ => unsafe { libc::_exit(0) },
            }
        }
        nix::unistd::ForkResult::Parent { child } => {
            drop(pipe_w);
            // Reap the near-instantly-exiting intermediate child so it
            // doesn't accumulate as a zombie under spurd.
            let _ = nix::sys::wait::waitpid(child, None);
            let mut buf = [0u8; 4];
            let mut read = 0;
            while read < buf.len() {
                match nix::unistd::read(&pipe_r, &mut buf[read..]) {
                    Ok(0) => break,
                    Ok(n) => read += n,
                    Err(nix::errno::Errno::EINTR) => continue,
                    Err(error) => return Err(std::io::Error::from(error)),
                }
            }
            if read != buf.len() {
                return Err(std::io::Error::other(
                    "spurstepd exited before reporting its pid",
                ));
            }
            let pid = u32::from_ne_bytes(buf);
            if execv_reported_failure(&pipe_r) {
                return Err(std::io::Error::other(format!(
                    "spurstepd (pid {pid}) failed to exec {}",
                    executable.display()
                )));
            }
            Ok(pid)
        }
    }
}

/// After the pid handoff, briefly polls the same pipe for the grandchild's
/// exec-failure byte (written only if `execv` returned) instead of waiting
/// out the full readiness timeout — a successful exec closes the write end
/// via FD_CLOEXEC with no byte sent, so this returns quickly either way.
fn execv_reported_failure(pipe_r: &std::os::fd::OwnedFd) -> bool {
    use std::os::fd::AsRawFd;
    let mut pfd = libc::pollfd {
        fd: pipe_r.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    if unsafe { libc::poll(&mut pfd, 1, 200) } <= 0 {
        return false;
    }
    let mut byte = [0u8; 1];
    matches!(nix::unistd::read(pipe_r, &mut byte), Ok(1))
}

async fn launch_stepd(
    config: &executor::JobLaunchConfig,
    run_attempt: u32,
    controller_addr: &str,
    reporting_node: &str,
    state_dir: &std::path::Path,
    options: StepdLaunchOptions,
) -> Result<(executor::LaunchResult, crate::stepd::StepdDescriptor), executor::LaunchError> {
    let mut launch_spec = crate::stepd::StepdLaunchSpec::try_from(config)
        .map_err(|error| executor::LaunchError::Other(anyhow::anyhow!(error)))?;
    launch_spec.controller_addr = controller_addr.into();
    launch_spec.reporting_node = reporting_node.into();
    launch_spec.run_attempt = run_attempt;
    launch_spec.allocation_only = options.allocation_only;
    launch_spec.step_id = options.step_id;
    launch_spec.container_rootfs_mode = options.container_rootfs_mode;
    launch_spec.hooks = options.hooks;
    launch_spec.plugstack_path = options.plugstack_path;
    launch_spec.pmix = options.pmix;
    let store = crate::stepd::StepdStore::new(state_dir);
    let session_dir = store
        .prepare_session_dir(config.job_id, run_attempt, launch_spec.step_id)
        .map_err(|error| {
            executor::LaunchError::Other(
                anyhow::Error::from(error).context("prepare stepd directory"),
            )
        })?;
    let mut descriptor = crate::stepd::StepdDescriptor::new(
        config.job_id,
        run_attempt,
        launch_spec.step_id,
        0,
        0,
        session_dir.join("runtime.sock"),
        std::path::PathBuf::new(),
    );
    launch_spec.capability = descriptor.capability.clone();
    descriptor.owner = config.user.clone();
    descriptor.uid = config.uid;
    descriptor.gid = config.gid;
    descriptor.work_dir = config.work_dir.clone();
    let namespaces = launch_namespaces(config, nix::unistd::geteuid().is_root());
    descriptor.has_pid_namespace = namespaces.pid;
    descriptor.has_user_namespace = namespaces.user;
    descriptor.has_mount_namespace = namespaces.mount;
    // Also on the spec: the supervisor republishes the descriptor from it, and
    // would otherwise overwrite these with defaults it cannot derive.
    launch_spec.has_pid_namespace = namespaces.pid;
    launch_spec.has_user_namespace = namespaces.user;
    launch_spec.has_mount_namespace = namespaces.mount;
    descriptor.resources = launch_spec.resources.clone();
    let launch_path = session_dir.join("launch.json");
    let launch_json = serde_json::to_vec(&launch_spec)
        .map_err(|error| executor::LaunchError::Other(anyhow::anyhow!(error)))?;
    crate::stepd::write_private(&launch_path, &launch_json).map_err(|error| {
        executor::LaunchError::Other(
            anyhow::Error::from(error).context("write runtime launch specification"),
        )
    })?;
    let executable = resolve_stepd_executable();
    info!(job_id = config.job_id, run_attempt, state_dir = %state_dir.display(), executable = %executable.display(), "starting stepd process");
    let spawn_args = vec![
        state_dir.to_path_buf(),
        std::path::PathBuf::from(config.job_id.to_string()),
        std::path::PathBuf::from(run_attempt.to_string()),
        launch_path,
    ];
    let spawn_executable = executable.clone();
    let pid =
        tokio::task::spawn_blocking(move || spawn_stepd_process(&spawn_executable, &spawn_args))
            .await
            .map_err(|error| executor::LaunchError::Other(error.into()))?
            .map_err(|error| {
                cleanup_unstarted_stepd(&store, config.job_id, run_attempt, launch_spec.step_id);
                executor::LaunchError::Other(
                    anyhow::Error::from(error).context("spawn stepd process"),
                )
            })?;
    descriptor.pid = pid;
    descriptor.process_start_ticks = crate::stepd::process_start_ticks(pid).unwrap_or(0);
    if let Err(error) = wait_for_stepd(&descriptor).await {
        if let Err(stop_error) = stop_stepd_process(&descriptor).await {
            warn!(
                job_id = config.job_id,
                run_attempt,
                %stop_error,
                "failed to stop stepd after readiness failure"
            );
        }
        cleanup_unstarted_stepd(&store, config.job_id, run_attempt, launch_spec.step_id);
        return Err(executor::LaunchError::Other(
            anyhow::Error::from(error).context("wait for stepd socket"),
        ));
    }
    // Readiness confirmed the subprocess is up and has published its real
    // pid/start-ticks; track those instead of the pid:0 placeholder so a
    // later liveness check can tell this session apart from a dead one.
    match store.load_descriptor(&session_dir) {
        Ok(published) => descriptor = published,
        Err(error) => warn!(job_id = config.job_id, run_attempt, %error,
            "failed to reload runtime descriptor; liveness checks will skip this session"),
    }
    // scontrol reports these back, so resolve the patterns the same way the
    // unsupervised launch does rather than echoing the raw request.
    let work_dir = descriptor.work_dir.clone();
    Ok((
        executor::LaunchResult {
            job: executor::RunningJob::AllocationOnly,
            stdout_path: executor::resolve_output_path(config, &work_dir, &config.stdout_path),
            stderr_path: executor::resolve_output_path(config, &work_dir, &config.stderr_path),
            pty_master: None,
            // Not created until the job is released; steps read it live.
            cgroup_path: None,
        },
        descriptor,
    ))
}

/// Signals a stepd process directly by pid — there's no systemd unit to stop
/// it through. A stale pid (already exited, or reused by an unrelated
/// process) is confirmed via `stepd_liveness` before signaling, same check
/// the crash watchdog uses.
async fn stop_stepd_process(descriptor: &crate::stepd::StepdDescriptor) -> std::io::Result<()> {
    match crate::stepd::stepd_liveness(descriptor)? {
        crate::stepd::StepdLiveness::Stale => Ok(()),
        crate::stepd::StepdLiveness::Live => {
            match nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(descriptor.pid as i32),
                nix::sys::signal::Signal::SIGTERM,
            ) {
                Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
                Err(error) => Err(std::io::Error::from(error)),
            }
        }
    }
}

/// The supervisor publishes its cgroup only once the launch returns, so a
/// descriptor read before that carries none — derive it from the job identity.
fn effective_cgroup_path(descriptor: &crate::stepd::StepdDescriptor) -> std::path::PathBuf {
    if descriptor.cgroup_path.as_os_str().is_empty() {
        executor::reapable_cgroup_path(
            descriptor.job_id,
            descriptor.run_attempt,
            descriptor.step_id,
        )
    } else {
        descriptor.cgroup_path.clone()
    }
}

/// Whether a torn-down stepd is confirmed dead. A recorded cgroup path is
/// authoritative — a successful stop signal only reaches the supervisor, which
/// by design ignores a raw SIGTERM for its job's cgroup.
async fn runtime_teardown_confirmed(
    descriptor: &crate::stepd::StepdDescriptor,
    stop_result: &std::io::Result<()>,
) -> bool {
    // A derived path is still worth reaping — the supervisor may have created
    // the cgroup before dying — but its absence proves nothing, since it may
    // never have existed, so the workload itself has to answer for its death.
    if descriptor.cgroup_path.as_os_str().is_empty() {
        runtime_cgroup_reaped(&effective_cgroup_path(descriptor));
        return stop_result.is_ok() && workload_confirmed_gone(descriptor).await;
    }
    // The retrying rmdir only succeeds once the cgroup is empty, so its
    // absence is what confirms the job's processes are actually gone.
    runtime_cgroup_reaped(&descriptor.cgroup_path)
}

/// With no cgroup to watch empty, killing the supervisor says nothing about the
/// workload it left behind — it is a separate process the signal never reached.
/// Escalate to the recorded workload and answer for that instead.
async fn workload_confirmed_gone(descriptor: &crate::stepd::StepdDescriptor) -> bool {
    // The agent's own copy was written before the launch returned, so it names
    // no workload at all; only what the supervisor republished does.
    let freshest = crate::stepd::freshest_session(descriptor);
    match crate::stepd::workload_liveness(&freshest) {
        crate::stepd::WorkloadLiveness::Gone => return true,
        // Holding is all an undetermined reading licenses; a root SIGKILL on a
        // pid whose identity it never confirmed is not.
        crate::stepd::WorkloadLiveness::Unknown => {
            warn!(
                job_id = descriptor.job_id,
                step_id = descriptor.step_id,
                "could not prove this workload is gone, nor that its pid is still its own; it \
                 stays held"
            );
            return false;
        }
        crate::stepd::WorkloadLiveness::Live => {}
    }
    let Ok(session) = freshest else { return false };
    let alive =
        || crate::stepd::process_is_live(session.workload_pid, session.workload_start_ticks);
    crate::executor::kill_process_tree(
        session.workload_pid as i32,
        nix::sys::signal::Signal::SIGKILL,
    );
    for _ in 0..WORKLOAD_DEATH_POLLS {
        if !alive() {
            return true;
        }
        tokio::time::sleep(WORKLOAD_DEATH_POLL_INTERVAL).await;
    }
    warn!(
        job_id = descriptor.job_id,
        step_id = descriptor.step_id,
        pid = session.workload_pid,
        "workload outlived its supervisor and did not die on SIGKILL"
    );
    false
}

/// Reap a supervisor's cgroup and report whether it is confirmed gone.
fn runtime_cgroup_reaped(cgroup_path: &std::path::Path) -> bool {
    crate::executor::cleanup_cgroup(cgroup_path);
    !cgroup_path.exists()
}

/// The run a loose `(job, attempt)` pair names. Attempt 0 names no run, so a
/// caller that cannot name one is told rather than left keying on a phantom.
fn named_run(job_id: u32, run_attempt: u32) -> Option<RunKey> {
    let key = RunKey::new(job_id, run_attempt);
    if key.is_none() {
        warn!(job_id, "a command named run attempt 0, which is no run");
    }
    key
}

#[cfg(test)]
fn key(job_id: u32, run_attempt: u32) -> RunKey {
    RunKey::new(job_id, run_attempt).expect("attempts start at 1")
}

/// Supervisors are per (job, step): a job may hold several at once.
pub(crate) type StepdKey = (u32, spur_core::step::StepId);
pub(crate) type StepdMap = HashMap<StepdKey, crate::stepd::StepdDescriptor>;

/// An interactive allocation owns the job's extern step, not its batch script.
/// Fencing and the tracked-attempt check must agree with what launch records.
fn launch_step_id(pty: bool) -> spur_core::step::StepId {
    if pty {
        spur_core::step::STEP_EXTERN
    } else {
        spur_core::step::STEP_BATCH
    }
}

/// Read a cgroup's member pids.
fn cgroup_member_pids(cgroup_path: &std::path::Path) -> Vec<i32> {
    std::fs::read_to_string(cgroup_path.join("cgroup.procs"))
        .map(|procs| {
            procs
                .split_whitespace()
                .filter_map(|pid| pid.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The process that owns a supervised job's namespaces: the one in the job's
/// cgroup whose parent is outside it, since the supervisor never joins.
fn cgroup_root_pid(cgroup_path: &std::path::Path) -> Option<i32> {
    let members = cgroup_member_pids(cgroup_path);
    let inside: std::collections::HashSet<i32> = members.iter().copied().collect();
    members
        .iter()
        .find(|pid| parent_pid(**pid).is_some_and(|parent| !inside.contains(&parent)))
        .copied()
        .or_else(|| members.iter().copied().min())
}

/// A process's parent, read from the stat field after the (possibly
/// space-containing) comm, which is why this splits on the closing paren.
fn parent_pid(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn stepd_key(descriptor: &crate::stepd::StepdDescriptor) -> StepdKey {
    (descriptor.job_id, descriptor.step_id)
}

/// One descriptor per adopted job, ranked rather than taken first: directory
/// order is arbitrary and only batch and allocation ones record the cores.
fn replayable_allocations(
    descriptors: &[crate::stepd::StepdDescriptor],
) -> Vec<&crate::stepd::StepdDescriptor> {
    let mut best: std::collections::HashMap<u32, &crate::stepd::StepdDescriptor> =
        std::collections::HashMap::new();
    for descriptor in descriptors {
        let rank =
            |d: &crate::stepd::StepdDescriptor| (d.run_attempt, !d.resources.cpu_ids.is_empty());
        match best.get(&descriptor.job_id) {
            Some(current) if rank(current) >= rank(descriptor) => {}
            _ => {
                best.insert(descriptor.job_id, descriptor);
            }
        }
    }
    let mut chosen: Vec<&crate::stepd::StepdDescriptor> = best.into_values().collect();
    chosen.sort_by_key(|d| (d.resources.cpu_ids.is_empty(), d.job_id));
    chosen
}

/// A descriptor with no recorded cores — a step, or one written before they
/// were persisted; the lowest free cores keep occupancy right if not identity.
fn fallback_cpu_ids(allocation: &NodeAllocation, cpus: u32) -> Vec<u32> {
    allocation
        .allocated_cpus
        .iter()
        .enumerate()
        .filter(|(_, &taken)| !taken)
        .map(|(index, _)| index as u32)
        .take(cpus as usize)
        .collect()
}

/// What one admitted run's evidence said at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptedRunOutcome {
    pub job_id: u32,
    pub run_attempt: u32,
    pub disposition: crate::admission::RunDisposition,
}

/// Every supervisor a job currently holds. Job-level operations (signal,
/// cancel, teardown) act on all of its steps, not just the batch one.
fn stepds_for_job(sessions: &StepdMap, job_id: u32) -> Vec<crate::stepd::StepdDescriptor> {
    sessions
        .iter()
        .filter(|((tracked, _), _)| *tracked == job_id)
        .map(|(_, descriptor)| descriptor.clone())
        .collect()
}

/// Which attempt's cgroup a cancel reaps directly, if any. Reaping kills every
/// process in the tree, so a supervised teardown must serve its grace period first.
fn cgroup_to_reap_on_cancel(doomed_attempt: Option<u32>, supervised: bool) -> Option<u32> {
    doomed_attempt.filter(|_| !supervised)
}

/// PMIx namespaces are keyed by job, not by attempt, so tearing one down on
/// behalf of a doomed attempt would strand a sibling still running under it.
fn pmix_teardown_is_due(doomed_attempt: Option<u32>, tracked_attempt: Option<u32>) -> bool {
    tracked_attempt.is_none_or(|tracked| doomed_attempt == Some(tracked))
}

/// The job's supervisors for one epoch. Attempt 0 means "whichever attempt this
/// node runs", which is how the controller asks without tracking the epoch.
fn stepds_for_attempt(
    sessions: &StepdMap,
    job_id: u32,
    run_attempt: u32,
) -> Vec<crate::stepd::StepdDescriptor> {
    stepds_for_job(sessions, job_id)
        .into_iter()
        .filter(|descriptor| run_attempt == 0 || descriptor.run_attempt == run_attempt)
        .collect()
}

/// Whether a supervisor holds the job's own processes, so the agent has no
/// child handle of its own. A step supervisor speaks only for that step.
fn owns_job_processes(descriptors: &[crate::stepd::StepdDescriptor]) -> bool {
    descriptors
        .iter()
        .any(|descriptor| descriptor.step_id == spur_core::step::STEP_BATCH)
}

/// Whether a supervisor is running the job's teardown, so the agent must not
/// retire its tracking underneath one and strand its completion.
fn supervisor_owns_teardown(descriptors: &[crate::stepd::StepdDescriptor]) -> bool {
    descriptors
        .iter()
        .any(|descriptor| descriptor.step_id != spur_core::step::STEP_EXTERN)
}

async fn fence_displaced_stepd(
    stepds: &Arc<Mutex<StepdMap>>,
    job_id: u32,
    step_id: spur_core::step::StepId,
    run_attempt: u32,
) -> std::io::Result<()> {
    let displaced = stepds.lock().await.get(&(job_id, step_id)).cloned();
    let Some(displaced) = displaced else {
        return Ok(());
    };
    displaced_runtime_attempt(&displaced, run_attempt)?;
    let stop_result = stop_stepd_process(&displaced).await;
    if !runtime_teardown_confirmed(&displaced, &stop_result).await {
        return match stop_result {
            Err(error) => Err(error),
            Ok(()) => Err(std::io::Error::other(
                "could not confirm the displaced stepd's cgroup is empty",
            )),
        };
    }
    // Only remove the entry we just fenced: a concurrent claim (a newer
    // attempt racing this one) may have already replaced it while the stop
    // was in flight, and that entry must not be dropped.
    let mut sessions = stepds.lock().await;
    if sessions
        .get(&(job_id, step_id))
        .is_some_and(|current| stepd_is_current(current, &displaced))
    {
        sessions.remove(&(job_id, step_id));
        discard_superseded_session(&displaced, run_attempt);
    }
    Ok(())
}

/// Drop a superseded attempt's session directory. The agent stopped this
/// supervisor itself, so nothing will ever finalize the session.
fn discard_superseded_session(displaced: &crate::stepd::StepdDescriptor, claiming_attempt: u32) {
    // A relaunch of the same attempt reuses this very directory.
    if displaced.run_attempt >= claiming_attempt {
        return;
    }
    let Some(session_dir) = displaced.socket_path.parent() else {
        return;
    };
    if let Err(error) = std::fs::remove_dir_all(session_dir) {
        if error.kind() != std::io::ErrorKind::NotFound {
            warn!(
                job_id = displaced.job_id,
                run_attempt = displaced.run_attempt,
                %error,
                "failed to discard a superseded runtime session"
            );
        }
    }
}

fn displaced_runtime_attempt(
    displaced: &crate::stepd::StepdDescriptor,
    run_attempt: u32,
) -> std::io::Result<u32> {
    if displaced.run_attempt > run_attempt {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "runtime attempt {} is still tracked for job {}",
                displaced.run_attempt, displaced.job_id
            ),
        ));
    }
    Ok(displaced.run_attempt)
}

/// Atomically claims this job's `stepds` slot for `descriptor`,
/// refusing to clobber an already-tracked strictly-newer attempt. Two
/// concurrent LaunchJob calls for the same job (e.g. a re-dispatch racing the
/// tail of a slow, now-superseded launch) can both pass fencing before either
/// is tracked; without this check whichever finishes last would silently
/// overwrite a newer, already-tracked session.
async fn claim_stepd_slot(
    stepds: &Arc<Mutex<StepdMap>>,
    descriptor: crate::stepd::StepdDescriptor,
) -> Result<(), crate::stepd::StepdDescriptor> {
    let mut sessions = stepds.lock().await;
    if sessions
        .get(&stepd_key(&descriptor))
        .is_some_and(|existing| existing.run_attempt > descriptor.run_attempt)
    {
        return Err(descriptor);
    }
    sessions.insert(stepd_key(&descriptor), descriptor);
    Ok(())
}

/// True when `job_id`'s stepd is already tracked under the exact
/// same `run_attempt` — a retried LaunchJob for an attempt already alive on
/// this node, not a genuine new dispatch.
async fn runtime_attempt_already_tracked(
    stepds: &Arc<Mutex<StepdMap>>,
    job_id: u32,
    step_id: spur_core::step::StepId,
    run_attempt: u32,
) -> bool {
    stepds
        .lock()
        .await
        .get(&(job_id, step_id))
        .is_some_and(|existing| existing.run_attempt == run_attempt)
}

fn stepd_is_current(
    current: &crate::stepd::StepdDescriptor,
    expected: &crate::stepd::StepdDescriptor,
) -> bool {
    current == expected
}

fn unreported_durable_exit(
    store: &crate::stepd::StepdStore,
    job_id: u32,
    run_attempt: u32,
    step_id: spur_core::step::StepId,
) -> bool {
    store
        .discover_unacknowledged_completions()
        .map(|pending| {
            pending.iter().any(|completion| {
                completion.job_id == job_id
                    && completion.run_attempt == run_attempt
                    && completion.step_id == step_id
            })
        })
        .unwrap_or(false)
}

fn cleanup_unstarted_stepd(
    store: &crate::stepd::StepdStore,
    job_id: u32,
    run_attempt: u32,
    step_id: spur_core::step::StepId,
) {
    // A supervisor we never reached may still have run the job to completion.
    // Deleting its recorded exit would turn that into a phantom launch failure.
    if matches!(
        store.observed_exit(job_id, run_attempt, step_id),
        Ok(Some(_))
    ) {
        warn!(
            job_id,
            run_attempt, "keeping stepd state; it recorded an exit before readiness completed"
        );
        return;
    }
    let session_dir = store.session_dir(job_id, run_attempt, step_id);
    if let Err(error) = std::fs::remove_dir_all(&session_dir) {
        if error.kind() != std::io::ErrorKind::NotFound {
            warn!(path = %session_dir.display(), %error, "failed to remove unstarted stepd state");
        }
    }
}

async fn wait_for_stepd(descriptor: &crate::stepd::StepdDescriptor) -> std::io::Result<()> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match crate::stepd::query_state(descriptor, uuid::Uuid::new_v4().to_string()).await {
            Ok(_) => return Ok(()),
            Err(error) if tokio::time::Instant::now() >= deadline => return Err(error),
            Err(_) => {}
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
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

pub struct TrackedJob {
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
    /// The job's cgroup, owned here so every launch path into the job can reach
    /// it. `None` when cgroup enforcement is off or no cgroup was created.
    cgroup_path: Option<std::path::PathBuf>,
}

impl TrackedJob {
    /// Take the cgroup path, leaving `None`. Whichever teardown path reaches
    /// the job first takes it, so the removal it authorizes happens once.
    fn take_cgroup(&mut self) -> Option<std::path::PathBuf> {
        self.cgroup_path.take()
    }
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
        if let Err(e) = mpi_host.stop_pmix_job(job_id) {
            warn!(job_id, error = %e, "PMIx teardown on job completion failed");
        }
    }
}

/// Release what a finished run owned, once the monitor has dropped it from
/// `running`. Skipped if the id is tracked again: it is all keyed by job id.
async fn teardown_completed_job(
    completed: &CompletedJob,
    lifecycle: &crate::job_lifecycle::JobLifecycle,
    running: &RunningJobs,
    allocation: &Arc<Mutex<NodeAllocation>>,
    mpi_host: &MpiPluginHost,
    admissions: &crate::admission::AdmissionStore,
    epilog: crate::admission::EpilogOwed,
) {
    let job_id = completed.job_id;
    // Held across the check below and every release under it, so a re-dispatch either
    // lands before this and is seen, or waits and finds nothing of its own removed.
    let _lifecycle = lifecycle.acquire(job_id).await;
    // Taken and released before `allocation`, the order commit_job uses.
    if running.lock().await.contains_key(&job_id) {
        warn!(
            job_id,
            "job id re-dispatched during teardown; leaving the new run's state alone"
        );
        return;
    }

    crate::container::cleanup_rootfs(
        &crate::container::job_rootfs_base(job_id),
        &completed.rootfs_mode,
    );
    crate::executor::cleanup_job_spool(job_id);
    if let Some(ref cgroup) = completed.cgroup {
        crate::executor::cleanup_cgroup(cgroup);
    }
    // The node above whatever was recorded: an allocation's is a step's leaf,
    // and nothing else removes the job's own. Derived from identity because the
    // recorded path is a leaf in one shape and absent in another.
    crate::executor::cleanup_cgroup(&crate::executor::expected_cgroup_path(
        job_id,
        completed.run_attempt,
    ));
    // The slice is NOT freed here. Cleanup finishing is not the controller
    // having committed the completion, and only that frees it.
    let _ = allocation;
    // Marked, not removed: the record is the evidence a later reconcile reads,
    // and the sweep collects it once nothing is still owed.
    if let Some(run) = named_run(job_id, completed.run_attempt) {
        if let Err(error) = admissions.mark_run_cleaned(run, epilog) {
            warn!(job_id, %error, "failed to settle the admission record after teardown");
        }
    }
    cleanup_completed_job_mpi(job_id, &completed.mpi, mpi_host).await;
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

/// What to do with an allocation once its cgroup setup has been attempted.
enum AllocationCgroup {
    /// Enforcement is in place, or off by config; record the allocation.
    Record(Option<std::path::PathBuf>),
    /// Enforcement failed but is not required; record it unenforced.
    Degraded(String),
    /// Enforcement is required and failed; refuse the registration.
    Refuse(String),
}

/// Fail closed: an allocation whose cgroup could not be created is refused when
/// `[cgroup] required` is set, rather than recorded as one nothing can enforce.
fn allocation_cgroup(
    setup: anyhow::Result<Option<std::path::PathBuf>>,
    required: bool,
) -> AllocationCgroup {
    match setup {
        Ok(path) => AllocationCgroup::Record(path),
        Err(e) if required => AllocationCgroup::Refuse(format!("{e:#}")),
        Err(e) => AllocationCgroup::Degraded(format!("{e:#}")),
    }
}

/// Whether a spawned child missed the job's cgroup, and so runs outside its
/// limits and device filter. Only `required` makes that fatal.
fn escaped_job_cgroup(required: bool, cgroup: Option<&std::path::Path>, pid: Option<u32>) -> bool {
    if !required {
        return false;
    }
    let (Some(cgroup), Some(pid)) = (cgroup, pid) else {
        return false;
    };
    // A step that exits between the spawn and this probe leaves `cgroup.procs`
    // too, and has escaped nothing — the normal wait path collects it.
    !executor::cgroup_has_pid(cgroup, pid) && pid_is_running(pid)
}

/// Whether `pid` is still executing. A signal-0 probe cannot tell: an
/// exited-but-unreaped child is a valid signal target, so `/proc` decides.
fn pid_is_running(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    stat_shows_running(&stat)
}

/// Whether a `/proc/<pid>/stat` line describes a still-executing process.
/// Read from the last ')': `comm` may itself contain spaces and ')'.
fn stat_shows_running(stat: &str) -> bool {
    let state = stat
        .rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().next());
    !matches!(state, None | Some("Z" | "X"))
}

/// Drop a job from the running set, handing back the cgroup it still owns.
///
/// The single exit from `running` bar the monitor loop, which takes the cgroup
/// itself to read `memory.events` first. Taking it here keeps the release
/// single; the caller drops the guard once the `running` lock is gone, since
/// removal SIGKILLs the cgroup's stragglers and then blocks retrying rmdir.
fn remove_tracked_job(
    running: &mut HashMap<u32, TrackedJob>,
    job_id: u32,
) -> (Option<TrackedJob>, executor::CgroupGuard) {
    let Some(mut tracked) = running.remove(&job_id) else {
        return (None, executor::CgroupGuard::new(None));
    };
    let cgroup = executor::CgroupGuard::new(tracked.take_cgroup());
    (Some(tracked), cgroup)
}

/// Roll back a registration that cannot be enforced. Takes the held `running`
/// map so the reservation is released under that lock, the order that keeps a
/// reconcile pass from seeing the allocation committed with no tracked job.
/// The refused job's cgroup comes back with the status for the caller to drop.
fn refuse_allocation(
    job_id: u32,
    reservation: LaunchReservationGuard,
    running: &mut HashMap<u32, TrackedJob>,
    reason: &str,
) -> (Status, executor::CgroupGuard) {
    error!(
        job_id,
        reason, "refusing an allocation that cannot be enforced"
    );
    let (_, cgroup) = remove_tracked_job(running, job_id);
    drop(reservation);
    let status = Status::failed_precondition(format!(
        "[cgroup] required but the allocation's cgroup could not be created: {reason}"
    ));
    (status, cgroup)
}

/// Job ids this node holds, shared with the reporter so heartbeats carry them.
pub(crate) type RunningJobs = Arc<Mutex<HashMap<u32, TrackedJob>>>;

#[derive(Clone)]
pub struct StepdRecoveryCleanup {
    running: RunningJobs,
    allocation: Arc<Mutex<NodeAllocation>>,
    stepds: Arc<Mutex<StepdMap>>,
}

#[derive(Clone)]
pub struct CompletionListenerContext {
    running: RunningJobs,
    step_completions: crate::step_completion::StepCompletions,
    allocation: Arc<Mutex<NodeAllocation>>,
    stepds: Arc<Mutex<StepdMap>>,
    stepds_store: crate::stepd::StepdStore,
    admissions: crate::admission::AdmissionStore,
    controller_addr: String,
    hostname: String,
}

impl StepdRecoveryCleanup {
    pub async fn reject(&self, descriptor: &crate::stepd::StepdDescriptor) {
        self.finish_rejection(descriptor, stop_stepd_process(descriptor).await)
            .await;
    }

    async fn finish_rejection(
        &self,
        descriptor: &crate::stepd::StepdDescriptor,
        stop_result: std::io::Result<()>,
    ) {
        if let Err(error) = &stop_result {
            warn!(
                job_id = descriptor.job_id,
                run_attempt = descriptor.run_attempt,
                %error,
                "failed to stop controller-rejected stepd"
            );
        }
        if !runtime_teardown_confirmed(descriptor, &stop_result).await {
            // Can't confirm the old attempt is actually gone — releasing
            // tracking now would let a new attempt double-book resources
            // it's still using.
            return;
        }
        self.release_tracking(descriptor).await;
        cleanup_stepd_files(descriptor);
    }

    async fn release_tracking(&self, descriptor: &crate::stepd::StepdDescriptor) {
        release_stepd_tracking(
            &self.running,
            &self.allocation,
            &self.stepds,
            descriptor,
            "controller-rejected",
        )
        .await;
    }
}

async fn release_stepd_tracking(
    running: &RunningJobs,
    allocation: &Arc<Mutex<NodeAllocation>>,
    stepds: &Arc<Mutex<StepdMap>>,
    descriptor: &crate::stepd::StepdDescriptor,
    reason: &'static str,
) -> bool {
    // The allocation outlives the individual steps drawing on it, so only the
    // job's last supervisor releases it. Held across the release so a step
    // claimed meanwhile cannot have its allocation torn down underneath it.
    let mut sessions = stepds.lock().await;
    let removed_runtime = sessions
        .get(&stepd_key(descriptor))
        .is_some_and(|current| current == descriptor);
    if removed_runtime {
        sessions.remove(&stepd_key(descriptor));
    }
    // Scoped to this attempt: a leftover session from a superseded one has no
    // claim on the allocation, and would otherwise strand it for good.
    let was_last_step = !sessions.values().any(|other| {
        other.job_id == descriptor.job_id && other.run_attempt == descriptor.run_attempt
    });

    // Tracking stops, but the slice is not freed: until the controller commits
    // a completion, this node is the only thing that knows the work stopped.
    let _ = allocation;
    let removed_tracked = was_last_step && {
        let mut jobs = running.lock().await;
        if jobs
            .get(&descriptor.job_id)
            .is_some_and(|current| current.run_attempt == descriptor.run_attempt)
        {
            jobs.remove(&descriptor.job_id);
            true
        } else {
            false
        }
    };
    drop(sessions);

    // The job node is the agent's to remove: it creates it, and the steps that
    // live in it only ever own their own leaf. Once the last of them is gone
    // nothing else will, so it would sit there empty for the node's lifetime.
    // Derived from the job's identity rather than by walking up from a leaf,
    // which is unreliable when the descriptor has no path yet and one level too
    // far when it does not.
    if was_last_step {
        crate::executor::cleanup_cgroup(&executor::expected_cgroup_path(
            descriptor.job_id,
            descriptor.run_attempt,
        ));
    }

    // Recorded even when a sibling keeps the job alive: this step's session is
    // pruned only once its own release is durable.
    if let Err(error) = crate::stepd::record_resources_released(descriptor) {
        warn!(
            job_id = descriptor.job_id,
            run_attempt = descriptor.run_attempt,
            step_id = descriptor.step_id,
            %error,
            "failed to record runtime resource release after {reason}"
        );
    }
    removed_runtime || removed_tracked
}

fn cleanup_stepd_files(descriptor: &crate::stepd::StepdDescriptor) {
    let Some(session_dir) = descriptor.socket_path.parent() else {
        return;
    };
    if let Err(error) = std::fs::remove_dir_all(session_dir) {
        if error.kind() != std::io::ErrorKind::NotFound {
            warn!(path = %session_dir.display(), %error, "failed to remove rejected stepd state");
        }
    }
}

/// Tear down a supervisor whose launch is being abandoned: stop the process,
/// reap its cgroup, and drop the session it left on disk.
async fn discard_stepd_session(descriptor: &crate::stepd::StepdDescriptor) {
    let job_id = descriptor.job_id;
    let run_attempt = descriptor.run_attempt;
    if let Err(error) = stop_stepd_process(descriptor).await {
        warn!(job_id, run_attempt, %error, "failed to stop abandoned stepd");
    }
    if !runtime_cgroup_reaped(&effective_cgroup_path(descriptor)) {
        warn!(
            job_id,
            run_attempt, "could not confirm the abandoned stepd's cgroup is empty"
        );
    }
    cleanup_stepd_files(descriptor);
}

/// Build an empty running-jobs map to share between the reporter and the agent.
pub fn new_running_jobs() -> RunningJobs {
    Arc::new(Mutex::new(HashMap::new()))
}

pub async fn recover_stepds(
    running: &RunningJobs,
    descriptors: Vec<crate::stepd::StepdDescriptor>,
) {
    let mut jobs = running.lock().await;
    for descriptor in descriptors {
        let cgroup_path = (!descriptor.cgroup_path.as_os_str().is_empty())
            .then(|| descriptor.cgroup_path.clone());
        let tracked = jobs.entry(descriptor.job_id).or_insert_with(|| TrackedJob {
            job: executor::RunningJob::AllocationOnly,
            cgroup_path,
            rootfs_mode: crate::container::RootfsMode::Extracted,
            stdout_path: String::new(),
            stderr_path: String::new(),
            has_pid_namespace: descriptor.has_pid_namespace,
            has_user_namespace: descriptor.has_user_namespace,
            has_mount_namespace: descriptor.has_mount_namespace,
            _pty_master: None,
            work_dir: descriptor.work_dir.clone(),
            uid: descriptor.uid,
            gid: descriptor.gid,
            user: descriptor.owner.clone(),
            partition: descriptor.resources.partition.clone(),
            gpu_devices: descriptor.resources.gpu_devices.clone(),
            cpus: descriptor.resources.cpus,
            memory_mb: descriptor.resources.memory_mb,
            nodelist: descriptor.resources.nodelist.clone(),
            mpi: descriptor.resources.mpi.clone(),
            run_attempt: descriptor.run_attempt,
        });
        // Sessions arrive in directory order, so only take these from the job's
        // own: a step's spool file and rootfs are the step's, not the job's.
        if spur_core::step::is_user_step(descriptor.step_id) {
            continue;
        }
        if !descriptor.stdout_path.is_empty() {
            tracked.stdout_path = descriptor.stdout_path.clone();
        }
        if !descriptor.stderr_path.is_empty() {
            tracked.stderr_path = descriptor.stderr_path.clone();
        }
        if let Some(mode) = descriptor.container_rootfs_mode.clone() {
            tracked.rootfs_mode = mode;
        }
    }
}

/// A recovered session whose supervisor has finished. Claiming first keeps a
/// completion push or the crash watchdog from reporting the same exit twice;
/// `None` means one of them got there first and this poll owes nothing.
#[allow(clippy::too_many_arguments)]
async fn settle_recovered_stepd(
    running: &RunningJobs,
    allocation: &Arc<Mutex<NodeAllocation>>,
    stepds: &Arc<Mutex<StepdMap>>,
    completions: &crate::step_completion::StepCompletions,
    store: &crate::stepd::StepdStore,
    descriptor: &crate::stepd::StepdDescriptor,
    exit_code: i32,
    signal: i32,
) -> Option<crate::stepd::PendingStepdCompletion> {
    if !claim_stepd(stepds, descriptor).await {
        return None;
    }
    release_stepd_tracking(
        running,
        allocation,
        stepds,
        descriptor,
        "runtime completion",
    )
    .await;
    // The controller report this poll goes on to make does not reach a client
    // that reattached to this step after the agent restarted.
    completions
        .complete(
            descriptor.job_id,
            descriptor.run_attempt,
            descriptor.step_id,
            crate::step_completion::StepOutcome { exit_code, signal },
        )
        .await;
    let epilog_failed = store
        .epilog_failed(
            descriptor.job_id,
            descriptor.run_attempt,
            descriptor.step_id,
        )
        .unwrap_or(false);
    Some(crate::stepd::PendingStepdCompletion {
        job_id: descriptor.job_id,
        run_attempt: descriptor.run_attempt,
        step_id: descriptor.step_id,
        exit_code,
        signal,
        epilog_failed,
    })
}

/// What the recovery monitor needs to settle an adopted supervisor.
pub(crate) struct RecoveryContext {
    pub running: RunningJobs,
    pub allocation: Arc<Mutex<NodeAllocation>>,
    pub stepds: Arc<Mutex<StepdMap>>,
    pub completions: crate::step_completion::StepCompletions,
    pub store: crate::stepd::StepdStore,
    pub admissions: crate::admission::AdmissionStore,
    pub controller_addr: String,
}

pub(crate) fn monitor_recovered_stepds(
    context: RecoveryContext,
    descriptors: Vec<crate::stepd::StepdDescriptor>,
) {
    let RecoveryContext {
        running,
        allocation,
        stepds,
        completions,
        store,
        admissions,
        controller_addr,
    } = context;
    tokio::spawn(async move {
        let mut pending: StepdMap = descriptors
            .into_iter()
            .map(|descriptor| (stepd_key(&descriptor), descriptor))
            .collect();
        let mut completed = HashMap::new();
        // Attempt count and first-failure instant per outstanding report. The
        // durable driver is the record; this only paces and surfaces the retry.
        let mut attempts: HashMap<(u32, spur_core::step::StepId), (u32, std::time::Instant)> =
            HashMap::new();
        let mut hooking_since: HashMap<(u32, spur_core::step::StepId), std::time::Instant> =
            HashMap::new();
        let hostname = hostname::get()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|_| "localhost".into());
        let instance_id = uuid::Uuid::new_v4().to_string();
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        while !pending.is_empty() {
            interval.tick().await;
            let mut newly_completed = Vec::new();
            let mut released = Vec::new();
            for (key, descriptor) in &pending {
                if completed.contains_key(key) {
                    continue;
                }
                let job_id = key.0;
                let tracked = running
                    .lock()
                    .await
                    .get(&job_id)
                    .is_some_and(|job| job.run_attempt == descriptor.run_attempt);
                let inactive =
                    match crate::stepd::query_state(descriptor, instance_id.clone()).await {
                        Ok(snapshot) => !snapshot.active,
                        Err(error) => {
                            // A transient IO/socket error is not evidence the
                            // session is gone — treat it as still-unknown and let
                            // the next tick retry, rather than risk classifying a
                            // live session as exited off a single failed probe.
                            tracing::debug!(
                                job_id,
                                run_attempt = descriptor.run_attempt,
                                %error,
                                "failed to query recovered stepd; will retry"
                            );
                            false
                        }
                    };
                let progress = if inactive {
                    match recovered_session_progress(&store, descriptor) {
                        Ok(progress) => progress,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            RecoveredSession::Unfinished
                        }
                        Err(error) => {
                            warn!(
                                job_id,
                                run_attempt = descriptor.run_attempt,
                                %error,
                                "failed to read durable recovered runtime completion"
                            );
                            RecoveredSession::Unfinished
                        }
                    }
                } else {
                    RecoveredSession::Unfinished
                };
                match progress {
                    RecoveredSession::Ended { exit_code, signal } => {
                        match settle_recovered_stepd(
                            &running,
                            &allocation,
                            &stepds,
                            &completions,
                            &store,
                            descriptor,
                            exit_code,
                            signal,
                        )
                        .await
                        {
                            Some(completion) => newly_completed.push((*key, completion)),
                            None => released.push(*key),
                        }
                    }
                    // Held pending rather than released: the report is what frees
                    // the slice, and the hook is still standing on it.
                    RecoveredSession::Hooking => {
                        if epilog_hold_is_worth_saying(&mut hooking_since, *key) {
                            warn!(
                                job_id,
                                run_attempt = descriptor.run_attempt,
                                step_id = descriptor.step_id,
                                "an epilog is still running; holding this run's completion report"
                            );
                        }
                    }
                    RecoveredSession::Unfinished if !tracked => released.push(*key),
                    RecoveredSession::Unfinished => {}
                }
            }
            for key in released {
                pending.remove(&key);
                hooking_since.remove(&key);
            }
            for (key, completion) in newly_completed {
                completed.insert(key, completion);
            }
            let mut acknowledged = Vec::new();
            for completion in completed.values() {
                let key = (completion.job_id, completion.step_id);
                if report_completion(
                    &controller_addr,
                    CompletionReport {
                        job_id: completion.job_id,
                        exit_code: completion.exit_code,
                        signal: completion.signal,
                        run_attempt: completion.run_attempt,
                        reporting_node: &hostname,
                        drain: completion.epilog_failed.then_some(&DrainRequest {
                            reason: "epilog script failed".into(),
                        }),
                        step_id: Some(completion.step_id),
                        payload: PayloadEvidence::Supervised(&store),
                    },
                )
                .await
                .settled()
                {
                    if let Err(error) = store.acknowledge_completion(completion) {
                        warn!(
                            job_id = completion.job_id,
                            run_attempt = completion.run_attempt,
                            %error,
                            "failed to acknowledge recovered runtime completion"
                        );
                    } else {
                        // Only now may the slice go: the controller has it.
                        if let Some(run) = named_run(completion.job_id, completion.run_attempt) {
                            record_run_epilog(
                                &admissions,
                                run,
                                completion.step_id,
                                epilog_outcome(completion.epilog_failed),
                            );
                            settle_acknowledged_completion(
                                &allocation,
                                &admissions,
                                run,
                                completion.step_id,
                            )
                            .await;
                        }
                        acknowledged.push(key);
                        attempts.remove(&key);
                    }
                } else {
                    // Nothing ends the obligation but an acknowledgement: this
                    // report is the only thing that will ever free the slice.
                    let attempt = attempts
                        .entry(key)
                        .or_insert((0u32, std::time::Instant::now()));
                    attempt.0 = attempt.0.saturating_add(1);
                    if attempt.1.elapsed() >= COMPLETION_STUCK_AFTER {
                        error!(
                            job_id = completion.job_id,
                            run_attempt = completion.run_attempt,
                            attempts = attempt.0,
                            held_for_secs = attempt.1.elapsed().as_secs(),
                            "a completion report the controller will not accept is holding this \
                             job's resources"
                        );
                        attempt.1 = std::time::Instant::now();
                    }
                    tokio::time::sleep(completion_backoff(attempt.0, completion.job_id)).await;
                }
            }
            for key in acknowledged {
                completed.remove(&key);
                pending.remove(&key);
                hooking_since.remove(&key);
            }
        }
    });
}

const REPARENTED_EXIT_POLL: std::time::Duration = std::time::Duration::from_millis(500);
const WORKLOAD_DEATH_POLLS: u32 = 20;
const WORKLOAD_DEATH_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);
const WORKLOAD_PID_POLLS: u32 = 40;
const WORKLOAD_PID_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(25);

/// A supervisor that dies before pushing completion leaves no other record, so
/// its tracked pid and start ticks are re-checked on this interval.
const RUNTIME_LIVENESS_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);

pub(crate) fn monitor_stepd_liveness(context: CompletionListenerContext) {
    tokio::spawn(async move {
        let stepds = context.stepds.clone();
        let mut interval = tokio::time::interval(RUNTIME_LIVENESS_CHECK_INTERVAL);
        loop {
            interval.tick().await;
            let tracked: Vec<_> = stepds.lock().await.values().cloned().collect();
            for descriptor in tracked {
                if descriptor.pid == 0 {
                    continue;
                }
                match crate::stepd::stepd_liveness(&descriptor) {
                    Ok(crate::stepd::StepdLiveness::Live) => {}
                    Ok(crate::stepd::StepdLiveness::Stale) => {
                        fence_dead_stepd(&context, descriptor).await;
                    }
                    Err(error) => {
                        warn!(job_id = descriptor.job_id, run_attempt = descriptor.run_attempt, %error,
                            "failed to check stepd liveness");
                    }
                }
            }
        }
    });
}

/// Atomically removes `descriptor`'s tracking entry if it's still the
/// current one, so a completion push and the crash watchdog can never both
/// proceed to report or record the same session's exit.
async fn claim_stepd(
    stepds: &Arc<Mutex<StepdMap>>,
    descriptor: &crate::stepd::StepdDescriptor,
) -> bool {
    let mut sessions = stepds.lock().await;
    if sessions
        .get(&stepd_key(descriptor))
        .is_some_and(|current| current == descriptor)
    {
        sessions.remove(&stepd_key(descriptor));
        true
    } else {
        false
    }
}

async fn fence_dead_stepd(
    context: &CompletionListenerContext,
    descriptor: crate::stepd::StepdDescriptor,
) {
    let CompletionListenerContext {
        running,
        step_completions: completions,
        allocation,
        stepds,
        stepds_store: store,
        admissions,
        controller_addr,
        hostname,
    } = context;
    if !claim_stepd(stepds, &descriptor).await {
        return;
    }
    warn!(
        job_id = descriptor.job_id,
        run_attempt = descriptor.run_attempt,
        "stepd process is gone without reporting completion; fencing"
    );
    let obligations = store.obligations(
        descriptor.job_id,
        descriptor.run_attempt,
        descriptor.step_id,
    );
    // A supervisor that recorded its workload's exit before dying already knows the
    // real outcome; only invent one when the ledger has nothing.
    let recorded_exit = store
        .observed_exit(
            descriptor.job_id,
            descriptor.run_attempt,
            descriptor.step_id,
        )
        .ok()
        .flatten();
    let (exit_code, signal) =
        recorded_exit.unwrap_or((0, nix::sys::signal::Signal::SIGKILL as i32));
    // Strictly above both the synthetic exit below, which would read as a hook
    // not reached, and the teardown that retires the ledger holding this answer.
    let recorded_epilog = match store.epilog_result(
        descriptor.job_id,
        descriptor.run_attempt,
        descriptor.step_id,
    ) {
        Ok(Some(failed)) => epilog_outcome(failed),
        Ok(None) => crate::admission::HookState::Unknown,
        Err(error) => {
            warn!(job_id = descriptor.job_id, run_attempt = descriptor.run_attempt, %error,
                "could not read how a dead stepd's epilog ended");
            crate::admission::HookState::Unknown
        }
    };
    if recorded_exit.is_none() {
        if let Some(reason) = store.recorded_failure(
            descriptor.job_id,
            descriptor.run_attempt,
            descriptor.step_id,
        ) {
            warn!(job_id = descriptor.job_id, run_attempt = descriptor.run_attempt, %reason,
                "stepd recorded why its launch failed before dying");
        }
        if let Err(error) =
            obligations.append(&crate::stepd::StepdObligation::ExitObserved { exit_code, signal })
        {
            warn!(job_id = descriptor.job_id, run_attempt = descriptor.run_attempt, %error,
                "failed to record synthetic exit for a dead stepd");
        }
    }
    // A crashed supervisor's job can outlive it. Teardown's confirmation falls
    // back to the recorded workload; a cgroup-only reap would do nothing here.
    if !runtime_teardown_confirmed(&descriptor, &Ok(())).await {
        warn!(
            job_id = descriptor.job_id,
            run_attempt = descriptor.run_attempt,
            "could not confirm a crashed stepd's workload is gone; its slice stays held"
        );
    }
    // The supervisor cannot be recovered, but its death still has to reach the
    // controller: unreported, the job holds its allocation in Running forever.
    let reported = report_completion(
        controller_addr,
        CompletionReport {
            job_id: descriptor.job_id,
            exit_code,
            signal,
            run_attempt: descriptor.run_attempt,
            reporting_node: hostname,
            drain: None,
            step_id: Some(descriptor.step_id),
            payload: PayloadEvidence::Supervised(store),
        },
    )
    .await
    .settled();
    if reported {
        if let Err(error) =
            obligations.append(&crate::stepd::StepdObligation::CompletionAcknowledged)
        {
            warn!(job_id = descriptor.job_id, run_attempt = descriptor.run_attempt, %error,
                "failed to record the acknowledged completion of a dead stepd");
        }
    } else {
        warn!(
            job_id = descriptor.job_id,
            run_attempt = descriptor.run_attempt,
            "could not report a dead stepd's completion; the retry loop will replay it"
        );
    }

    release_stepd_tracking(running, allocation, stepds, &descriptor, "stepd crash").await;
    if reported {
        if let Some(run) = named_run(descriptor.job_id, descriptor.run_attempt) {
            record_run_epilog(admissions, run, descriptor.step_id, recorded_epilog);
            settle_acknowledged_completion(allocation, admissions, run, descriptor.step_id).await;
        }
    }

    // The supervisor is gone, so no completion push is coming; without this the
    // RPC that launched this step stays parked for the agent's lifetime.
    if completions
        .complete(
            descriptor.job_id,
            descriptor.run_attempt,
            descriptor.step_id,
            crate::step_completion::StepOutcome { exit_code, signal },
        )
        .await
    {
        warn!(
            job_id = descriptor.job_id,
            run_attempt = descriptor.run_attempt,
            step_id = descriptor.step_id,
            "reported a fenced step's exit to its parked caller"
        );
    }
}

/// Accept stepd completion pushes for the daemon's life; spurd,
/// not the subprocess, owns forwarding to the controller and local cleanup.
const COMPLETION_NOTIFICATION_READ_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(10);
const COMPLETION_ACCEPT_ERROR_BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);

pub async fn serve_completion_notifications(
    listener: tokio::net::UnixListener,
    context: CompletionListenerContext,
) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let context = context.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_completion_notification(stream, &context).await {
                        warn!(%error, "failed to handle stepd completion notification");
                    }
                });
            }
            Err(error) => {
                warn!(%error, "completion notification listener accept failed");
                tokio::time::sleep(COMPLETION_ACCEPT_ERROR_BACKOFF).await;
            }
        }
    }
}

fn capability_matches(capability: &str, expected: &str) -> bool {
    !expected.is_empty()
        && capability.len() == expected.len()
        && bool::from(subtle::ConstantTimeEq::ct_eq(
            capability.as_bytes(),
            expected.as_bytes(),
        ))
}

async fn handle_completion_notification(
    stream: tokio::net::UnixStream,
    context: &CompletionListenerContext,
) -> std::io::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(reader);
    let mut line = String::new();
    tokio::time::timeout(
        COMPLETION_NOTIFICATION_READ_TIMEOUT,
        crate::stepd::read_line_bounded(&mut reader, &mut line),
    )
    .await
    .map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::TimedOut, "notification read timeout")
    })??;
    let notification: crate::stepd::AgentNotification =
        serde_json::from_str(&line).map_err(std::io::Error::other)?;
    let (job_id, run_attempt, step_id, exit_code, signal, epilog_failed, capability) =
        match notification {
            crate::stepd::AgentNotification::StepdCompleted {
                job_id,
                run_attempt,
                step_id,
                exit_code,
                signal,
                epilog_failed,
                capability,
            } => (
                job_id,
                run_attempt,
                step_id,
                exit_code,
                signal,
                epilog_failed,
                capability,
            ),
        };

    let descriptor = context
        .stepds
        .lock()
        .await
        .get(&(job_id, step_id))
        .filter(|descriptor| descriptor.run_attempt == run_attempt)
        .cloned();
    let descriptor = match descriptor {
        Some(descriptor) if capability_matches(&capability, &descriptor.capability) => {
            Some(descriptor)
        }
        Some(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "stepd completion capability mismatch",
            ));
        }
        None => None,
    };

    let response = match descriptor {
        Some(descriptor) if !claim_stepd(&context.stepds, &descriptor).await => {
            // Already claimed elsewhere (the watchdog, or a concurrent duplicate push).
            crate::stepd::AgentNotificationResponse::Acknowledged
        }
        Some(descriptor) => {
            let reported = report_completion(
                &context.controller_addr,
                CompletionReport {
                    job_id,
                    exit_code,
                    signal,
                    run_attempt,
                    reporting_node: &context.hostname,
                    drain: epilog_failed.then_some(&DrainRequest {
                        reason: "epilog script failed".into(),
                    }),
                    step_id: Some(step_id),
                    payload: PayloadEvidence::Supervised(&context.stepds_store),
                },
            )
            .await
            .settled();
            release_stepd_tracking(
                &context.running,
                &context.allocation,
                &context.stepds,
                &descriptor,
                "runtime completion",
            )
            .await;
            // Recorded whatever the controller said: the record must describe
            // the hook that ran, not the delivery of the report about it.
            let admissions = context.admissions.clone();
            if let Some(run) = named_run(job_id, run_attempt) {
                record_run_epilog(&admissions, run, step_id, epilog_outcome(epilog_failed));
                // The acknowledgement, and only it, unlocks the slice. Without
                // this the exit path holds and nothing ever frees it again.
                if reported {
                    settle_acknowledged_completion(&context.allocation, &admissions, run, step_id)
                        .await;
                }
            }
            // A user step's exit ends the RPC that launched it, not the job, so
            // the controller report above was a no-op and this is the delivery.
            context
                .step_completions
                .complete(
                    job_id,
                    run_attempt,
                    step_id,
                    crate::step_completion::StepOutcome { exit_code, signal },
                )
                .await;
            if reported {
                crate::stepd::AgentNotificationResponse::Acknowledged
            } else {
                crate::stepd::AgentNotificationResponse::Deferred
            }
        }
        // Usually a duplicate retry after a lost ack — but a session that
        // finished pre-claim lands here too, so defer while its exit is unreported.
        None if unreported_durable_exit(&context.stepds_store, job_id, run_attempt, step_id) => {
            crate::stepd::AgentNotificationResponse::Deferred
        }
        None => crate::stepd::AgentNotificationResponse::Acknowledged,
    };

    let payload = serde_json::to_vec(&response).map_err(std::io::Error::other)?;
    writer.write_all(&payload).await?;
    writer.write_all(b"\n").await
}

fn durable_runtime_exit(
    store: &crate::stepd::StepdStore,
    descriptor: &crate::stepd::StepdDescriptor,
) -> std::io::Result<Option<(i32, i32)>> {
    store.observed_exit(
        descriptor.job_id,
        descriptor.run_attempt,
        descriptor.step_id,
    )
}

/// How far a recovered supervisor's own ledger says its session has got. The
/// exit is recorded before the epilog runs, so an exit alone is not the end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveredSession {
    Unfinished,
    /// Exited, with the hook that follows it yet to say how it ended.
    Hooking,
    Ended {
        exit_code: i32,
        signal: i32,
    },
}

fn recovered_session_progress(
    store: &crate::stepd::StepdStore,
    descriptor: &crate::stepd::StepdDescriptor,
) -> std::io::Result<RecoveredSession> {
    let Some((exit_code, signal)) = durable_runtime_exit(store, descriptor)? else {
        return Ok(RecoveredSession::Unfinished);
    };
    // Only the hook's own record proves it ended: a hook can outlive the process
    // that started it, so nothing about the supervisor answers for it.
    match store.epilog_result(
        descriptor.job_id,
        descriptor.run_attempt,
        descriptor.step_id,
    )? {
        Some(_) => Ok(RecoveredSession::Ended { exit_code, signal }),
        None => Ok(RecoveredSession::Hooking),
    }
}

/// One runtime session: a step of one attempt of one job. Two sessions of the
/// same attempt are distinct, so dedup keyed on the job alone over-matches.
pub type SessionIdentity = (u32, u32, spur_core::step::StepId);

pub async fn replay_unacknowledged_stepd_completions(
    store: &crate::stepd::StepdStore,
    admissions: &crate::admission::AdmissionStore,
    allocation: &Arc<Mutex<NodeAllocation>>,
    controller_addr: &str,
    reporting_node: &str,
) -> anyhow::Result<Vec<SessionIdentity>> {
    let mut reconciled = Vec::new();
    for completion in store.discover_unacknowledged_completions()? {
        if report_completion(
            controller_addr,
            CompletionReport {
                job_id: completion.job_id,
                exit_code: completion.exit_code,
                signal: completion.signal,
                run_attempt: completion.run_attempt,
                reporting_node,
                drain: completion.epilog_failed.then_some(&DrainRequest {
                    reason: "epilog script failed".into(),
                }),
                step_id: Some(completion.step_id),
                payload: PayloadEvidence::Supervised(store),
            },
        )
        .await
        .settled()
        {
            store.acknowledge_completion(&completion)?;
            if let Some(run) = named_run(completion.job_id, completion.run_attempt) {
                record_run_epilog(
                    admissions,
                    run,
                    completion.step_id,
                    epilog_outcome(completion.epilog_failed),
                );
                // A late delivery frees the slice here, like every other settled
                // report: leaving it to a later sweep frees it with nothing said.
                settle_acknowledged_completion(allocation, admissions, run, completion.step_id)
                    .await;
            }
            reconciled.push((
                completion.job_id,
                completion.run_attempt,
                completion.step_id,
            ));
        }
    }
    // After the sessions above, which carry the real exit status: a run only
    // reaches the pass below once nothing is left that could report one.
    report_runs_nothing_can_speak_for(
        store,
        admissions,
        allocation,
        controller_addr,
        reporting_node,
    )
    .await;
    Ok(reconciled)
}

/// What a run's recorded supervisors can be *proven* to be. "No evidence" is its
/// own answer, so silence can never be spent as proof of death.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Liveness {
    /// Every recorded supervisor read as stale, and there was one to read.
    Gone,
    Live,
    /// Nothing was recorded, or a read could not be completed.
    CannotTell,
}

/// Whether the reads prove anything. No reads at all proves nothing, and neither
/// does one that could not be completed.
fn liveness_of(reads: &[std::io::Result<crate::stepd::StepdLiveness>]) -> Liveness {
    if reads
        .iter()
        .any(|read| matches!(read, Ok(crate::stepd::StepdLiveness::Live)))
    {
        return Liveness::Live;
    }
    if !reads.is_empty()
        && reads
            .iter()
            .all(|read| matches!(read, Ok(crate::stepd::StepdLiveness::Stale)))
    {
        return Liveness::Gone;
    }
    Liveness::CannotTell
}

fn recorded_supervisor_liveness(admitted: &crate::admission::AdmittedRun) -> Liveness {
    let reads: Vec<std::io::Result<crate::stepd::StepdLiveness>> = admitted
        .recorded_supervisors()
        .map(crate::stepd::supervisor_liveness)
        .collect();
    for (supervisor, read) in admitted.recorded_supervisors().zip(reads.iter()) {
        if let Err(error) = read {
            warn!(
                job_id = admitted.run.job_id,
                run_attempt = admitted.run.run_attempt,
                pid = supervisor.pid,
                %error,
                "could not tell whether a supervisor is alive; holding its slice"
            );
        }
    }
    liveness_of(&reads)
}

/// Whether some participant left an exit behind. One exists only on disk, so a
/// session still holding it is what will report this run, with the real status.
fn an_exit_is_still_on_disk(
    store: &crate::stepd::StepdStore,
    admitted: &crate::admission::AdmittedRun,
) -> bool {
    admitted.participants.iter().any(|participant| {
        !matches!(
            store.observed_exit(
                admitted.run.job_id,
                admitted.run.run_attempt,
                participant.step_id,
            ),
            Ok(None)
        )
    })
}

/// Runs no session is left to speak for; without this nothing ever asks, and the
/// record charges the node's cores for good.
fn runs_nothing_can_speak_for(
    store: &crate::stepd::StepdStore,
    admissions: &crate::admission::AdmissionStore,
) -> Vec<crate::admission::AdmittedRun> {
    let Ok(loaded) = admissions.load_all() else {
        return Vec::new();
    };
    loaded
        .runs
        .into_iter()
        .filter(|admitted| {
            !admitted.run.controller_ack.is_given()
                && admitted.owes_a_report()
                && recorded_supervisor_liveness(admitted) == Liveness::Gone
                && !an_exit_is_still_on_disk(store, admitted)
        })
        .collect()
}

/// Report the loss of a run nothing is left to speak for, so its slice is freed
/// on the controller's acknowledgement like every other completion.
async fn report_runs_nothing_can_speak_for(
    store: &crate::stepd::StepdStore,
    admissions: &crate::admission::AdmissionStore,
    allocation: &Arc<Mutex<NodeAllocation>>,
    controller_addr: &str,
    reporting_node: &str,
) {
    for admitted in runs_nothing_can_speak_for(store, admissions) {
        let (job_id, run_attempt) = (admitted.run.job_id, admitted.run.run_attempt);
        let (Some(run), Some(step_id)) = (admitted.run.key(), admitted.lifecycle_step()) else {
            continue;
        };
        let acknowledged = report_completion(
            controller_addr,
            CompletionReport {
                job_id,
                exit_code: 0,
                signal: nix::sys::signal::Signal::SIGKILL as i32,
                run_attempt,
                reporting_node,
                drain: None,
                step_id: Some(step_id),
                payload: PayloadEvidence::Supervised(store),
            },
        )
        .await
        .settled();
        if !acknowledged {
            debug!(
                job_id,
                run_attempt, "a lost run's completion is not acknowledged yet; holding its slice"
            );
            continue;
        }
        settle_acknowledged_completion(allocation, admissions, run, step_id).await;
        // The run's completion is committed, so no sibling's report will ever be
        // answered, and one left owed keeps the record forever.
        if let Err(error) = admissions.discharge_owed_reports(run) {
            warn!(job_id, run_attempt, %error, "failed to discharge a lost run's owed reports");
        }
        warn!(
            job_id,
            run_attempt,
            "no supervisor was left for this run; reported its loss and freed its slice"
        );
    }
}

pub fn retry_unacknowledged_stepd_completions(
    store: crate::stepd::StepdStore,
    admissions: crate::admission::AdmissionStore,
    allocation: Arc<Mutex<NodeAllocation>>,
    controller_addr: String,
    reporting_node: String,
) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            match replay_unacknowledged_stepd_completions(
                &store,
                &admissions,
                &allocation,
                &controller_addr,
                &reporting_node,
            )
            .await
            {
                Ok(reconciled) if !reconciled.is_empty() => {
                    tracing::info!(
                        completions = reconciled.len(),
                        "reconciled durable runtime completions"
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%error, "failed to replay durable runtime completions");
                }
            }
            // A settled user step's record is held past its completion report so
            // a restart can still answer for it; nothing else sweeps it later.
            if let Err(error) = store.prune_finalized() {
                tracing::warn!(%error, "failed to prune finalized runtime sessions");
            }
        }
    });
}

type PmixLaunchSetup = (
    PmixLaunchGuard,
    PmixLaunchPlan,
    Option<Vec<HashMap<String, String>>>,
);

/// Agent-hosted PMIx, for the launch paths that have no supervisor of their own.
fn start_pmix_launch(
    mpi_host: Arc<MpiPluginHost>,
    proto_plan: &spur_proto::proto::PmixLaunchPlan,
    task_offset: u32,
    tasks_on_node: u32,
) -> Result<PmixLaunchSetup, Status> {
    let plan = mpi_plugin::plan_from_proto(proto_plan).map_err(Status::failed_precondition)?;
    let guard =
        PmixLaunchGuard::start(mpi_host.clone(), &plan).map_err(Status::failed_precondition)?;
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
        let key = self.key;
        if let Ok(mut steps) = self.steps.try_lock() {
            steps.remove(&key);
        } else if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let steps = self.steps.clone();
            handle.spawn(async move {
                steps.lock().await.remove(&key);
            });
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

fn signal_step_process_group(pid: u32, signal: i32) {
    let sig =
        nix::sys::signal::Signal::try_from(signal).unwrap_or(nix::sys::signal::Signal::SIGTERM);
    let leader = nix::unistd::Pid::from_raw(pid as i32);
    if let Err(e) = nix::sys::signal::killpg(leader, sig) {
        if let Err(kill_err) = nix::sys::signal::kill(leader, Some(sig)) {
            warn!(
                pid,
                signal,
                killpg = %e,
                kill = %kill_err,
                "step process group signal failed (step may already have exited)"
            );
        }
    }
}

/// SIGKILL a step's whole process group, then reap the leader: every step shape
/// runs the user's command in a descendant of the spawned wrapper.
async fn kill_step_process_group(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        signal_step_process_group(pid, nix::sys::signal::Signal::SIGKILL as i32);
    }
    let _ = child.kill().await;
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

#[cfg(test)]
mod step_cgroup_tests {
    use super::{escaped_job_cgroup, stat_shows_running};

    fn cgroup_holding(pids: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("cgroup.procs"), pids).expect("cgroup.procs");
        dir
    }

    #[test]
    fn a_step_that_joined_the_job_cgroup_runs() {
        let cgroup = cgroup_holding("17\n4242\n");
        assert!(!escaped_job_cgroup(true, Some(cgroup.path()), Some(4242)));
    }

    #[test]
    fn a_required_cgroup_catches_a_live_step_that_did_not_join() {
        // The test process: certainly running, and an empty cgroup cannot
        // contain it whatever pid the runner handed us.
        let cgroup = cgroup_holding("");
        assert!(escaped_job_cgroup(
            true,
            Some(cgroup.path()),
            Some(std::process::id())
        ));
    }

    // A step can exit between `spawn` returning and this probe. The zombie is
    // absent from `cgroup.procs` without ever having escaped it.
    #[test]
    fn a_step_that_exited_before_the_probe_has_not_escaped() {
        let cgroup = cgroup_holding("");
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn a short-lived child");
        let pid = child.id();
        let nix_pid = nix::unistd::Pid::from_raw(pid as i32);
        // Blocks until the child exits, but WNOWAIT leaves it unreaped, so the
        // assertion below runs against a genuine zombie.
        nix::sys::wait::waitid(
            nix::sys::wait::Id::Pid(nix_pid),
            nix::sys::wait::WaitPidFlag::WEXITED | nix::sys::wait::WaitPidFlag::WNOWAIT,
        )
        .expect("child should reach a waitable state");
        assert!(
            nix::sys::signal::kill(nix_pid, None).is_ok(),
            "a zombie answers a signal-0 probe, which is why liveness is read from /proc"
        );

        assert!(!escaped_job_cgroup(true, Some(cgroup.path()), Some(pid)));

        child.wait().expect("reap the child");
    }

    #[test]
    fn the_state_field_is_read_past_a_parenthesized_comm() {
        assert!(stat_shows_running("4242 (weird ) name) S 1 4242 4242 0"));
        assert!(!stat_shows_running("4242 (weird ) name) Z 1 4242 4242 0"));
    }

    #[test]
    fn an_optional_cgroup_lets_an_unjoined_step_run() {
        let cgroup = cgroup_holding("17\n");
        assert!(!escaped_job_cgroup(false, Some(cgroup.path()), Some(4242)));
    }

    #[test]
    fn a_job_with_no_cgroup_has_none_to_escape() {
        // Config can disable enforcement outright; `required` then has no
        // cgroup to check and must not refuse every step on the node.
        assert!(!escaped_job_cgroup(true, None, Some(4242)));
    }
}

#[cfg(test)]
mod allocation_cgroup_tests {
    use super::{allocation_cgroup, AllocationCgroup};

    #[test]
    fn a_created_cgroup_is_recorded_on_the_allocation() {
        let path = std::path::PathBuf::from("/sys/fs/cgroup/spur/job_9");
        let decision = allocation_cgroup(Ok(Some(path.clone())), true);
        let AllocationCgroup::Record(recorded) = decision else {
            panic!("a created cgroup must reach the tracked job");
        };
        assert_eq!(recorded, Some(path));
    }

    // `setup_cgroup` returns no path without an error only when `[cgroup]
    // enabled` is off, which leaves `required` nothing to gate.
    #[test]
    fn a_cgroup_disabled_by_config_records_an_unenforced_allocation() {
        assert!(matches!(
            allocation_cgroup(Ok(None), true),
            AllocationCgroup::Record(None)
        ));
    }

    // Without a cgroup the allocation's steps escape the device filter, so a
    // node that must enforce refuses the allocation rather than pretend.
    #[test]
    fn a_required_cgroup_refuses_an_allocation_it_cannot_enforce() {
        let setup = Err(anyhow::anyhow!("cgroup root unavailable"));
        let AllocationCgroup::Refuse(reason) = allocation_cgroup(setup, true) else {
            panic!("required enforcement must refuse, not record");
        };
        assert!(reason.contains("cgroup root unavailable"));
    }

    #[test]
    fn an_optional_cgroup_degrades_to_an_unenforced_allocation() {
        let setup = Err(anyhow::anyhow!("cgroup creation failed (not root)"));
        let AllocationCgroup::Degraded(reason) = allocation_cgroup(setup, false) else {
            panic!("an optional cgroup must degrade, not refuse");
        };
        assert!(reason.contains("not root"));
    }
}

#[cfg(test)]
mod teardown_tests {
    use super::{remove_tracked_job, TrackedJob};
    use std::collections::HashMap;

    // A temp directory stands in for the job's cgroup: cleanup takes a path, so
    // the real removal runs without root or a cgroupfs.
    fn tracked_with_cgroup() -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().expect("tempdir");
        let cgroup = root.path().join("job_7");
        std::fs::create_dir(&cgroup).expect("cgroup dir");
        (root, cgroup)
    }

    #[test]
    fn removing_a_job_releases_its_cgroup() {
        let (_root, cgroup) = tracked_with_cgroup();
        let mut running = HashMap::new();
        running.insert(7, TrackedJob::allocation_only(Some(cgroup.clone())));

        let (removed, release) = remove_tracked_job(&mut running, 7);
        let removed = removed.expect("the job was tracked");
        drop(release);

        assert!(!cgroup.exists(), "teardown must remove the job's cgroup");
        assert!(
            removed.cgroup_path.is_none(),
            "the released path must be taken off the job"
        );
    }

    // The removal is what blocks, so it belongs to the caller, to run once the
    // map is unlocked. Taking the job out must not be what removes the cgroup.
    #[tokio::test]
    async fn the_cgroup_is_released_by_its_token_and_not_under_the_lock() {
        let (_root, cgroup) = tracked_with_cgroup();
        let running = super::new_running_jobs();
        running
            .lock()
            .await
            .insert(7, TrackedJob::allocation_only(Some(cgroup.clone())));

        let (removed, release) = {
            let mut jobs = running.lock().await;
            let taken = remove_tracked_job(&mut jobs, 7);
            assert!(
                cgroup.exists(),
                "the cgroup must outlive the removal that holds the lock"
            );
            taken
        };

        assert!(removed.is_some(), "the job was tracked");
        assert!(
            running.try_lock().is_ok(),
            "the map must be unlocked before the cgroup is released"
        );
        drop(release);
        assert!(!cgroup.exists(), "releasing the token removes the cgroup");
    }

    // A re-dispatched job derives the same job_<id> path, so a second teardown
    // pass over an already-released job would delete a live run's cgroup.
    #[test]
    fn a_released_cgroup_is_not_released_again() {
        let (_root, cgroup) = tracked_with_cgroup();
        let mut running = HashMap::new();
        running.insert(7, TrackedJob::allocation_only(Some(cgroup.clone())));
        let (released, first) = remove_tracked_job(&mut running, 7);
        drop(first);

        std::fs::create_dir_all(&cgroup).expect("successor cgroup");
        running.insert(7, released.expect("the job was tracked"));
        let (_, second) = remove_tracked_job(&mut running, 7);
        drop(second);

        assert!(
            cgroup.exists(),
            "a cgroup already released must not be released a second time"
        );
    }

    #[test]
    fn removing_a_job_without_a_cgroup_releases_nothing() {
        let mut running = HashMap::new();
        running.insert(8, TrackedJob::allocation_only(None));

        let (removed, release) = remove_tracked_job(&mut running, 8);
        let removed = removed.expect("the job was tracked");
        drop(release);

        assert!(removed.cgroup_path.is_none());
        assert!(running.is_empty(), "the job must still leave the map");
    }

    #[test]
    fn removing_an_untracked_job_yields_nothing() {
        let (removed, release) = remove_tracked_job(&mut HashMap::new(), 9);
        drop(release);
        assert!(removed.is_none());
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
/// `step` names a step even when it is 0; `step_id` cannot, because 0 has always
/// meant the job's own output there.
fn requested_step_of(req: &spur_proto::proto::StreamJobOutputRequest) -> Option<u32> {
    req.step.or((req.step_id != 0).then_some(req.step_id))
}

/// Which namespaces a launch lands a job in. Shared so a job adopted after an
/// agent restart is described exactly as the live launch described it.
fn launch_namespaces(cfg: &executor::JobLaunchConfig, is_root: bool) -> LaunchNamespaces {
    // Derived from the same predicate the launch uses: a spec claiming namespaces
    // the launch never created makes adoption re-enter ones that do not exist.
    let unshared = executor::would_use_namespaces(cfg, is_root);
    let is_container = cfg.container.is_some();
    LaunchNamespaces {
        pid: unshared || is_container,
        user: is_container && !is_root,
        mount: unshared || is_container,
    }
}

struct LaunchNamespaces {
    pid: bool,
    user: bool,
    mount: bool,
}

/// A resumed terminal's shell was reparented to init when its old agent died,
/// so there is no child handle to wait on; watch the pid instead.
async fn wait_for_reparented_exit(pid: i32) -> i32 {
    let path = std::path::PathBuf::from(format!("/proc/{pid}"));
    while path.exists() {
        tokio::time::sleep(REPARENTED_EXIT_POLL).await;
    }
    0
}

/// A step that builds its own container keeps the agent in its exec path, so it
/// cannot be handed to a supervisor. Entering a parent job's namespaces can.
fn step_can_be_supervised(container_image: &str) -> bool {
    container_image.is_empty()
}

/// The plan a step's supervisor hosts its PMIx server from. Re-keyed to the step
/// being launched: a server and a plan naming different steps rendezvous nowhere.
fn supervised_step_pmix(
    proto: &spur_proto::proto::PmixLaunchPlan,
    config: spur_core::config::MpiConfig,
    step_id: spur_core::step::StepId,
    tasks_on_node: u32,
) -> Result<crate::stepd::StepdPmix, Status> {
    let mut plan = mpi_plugin::plan_from_proto(proto).map_err(Status::failed_precondition)?;
    // The supervisor sizes the rank wrapper from the plan, so a plan that
    // disagrees with the launch would silently start the wrong rank count.
    if plan.local_procs.len() as u32 != tasks_on_node {
        return Err(Status::failed_precondition(format!(
            "PMIx plan carries {} local ranks but this node launches {tasks_on_node}",
            plan.local_procs.len()
        )));
    }
    plan.rekey_to_step(step_id);
    Ok(crate::stepd::StepdPmix {
        plan,
        config,
        user_script_path: String::new(),
        wrapper_path: String::new(),
    })
}

/// The user-script and wrapper file names for one step. Concurrent steps of a
/// job share a scratch directory, so the step id has to be part of the name.
fn step_scratch_names(node_id: u32, step_id: u32) -> (String, String) {
    (
        format!("cmd_{node_id}_{step_id}.sh"),
        format!("wrapper_{node_id}_{step_id}.sh"),
    )
}

/// The environment an interactive shell starts from: the job's own, never
/// spurd's, plus the identity a login shell expects to find.
fn terminal_environment(
    entry: &crate::job_entry::JobEntry,
    job_id: u32,
) -> HashMap<String, String> {
    let mut environment = HashMap::from([("TERM".to_string(), "xterm-256color".to_string())]);
    environment.extend(AgentService::session_environ(entry));
    environment.extend(entry.env_vars(job_id));
    if entry.uid > 0 {
        if let Some(user) = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(entry.uid))
            .ok()
            .flatten()
        {
            environment.insert("HOME".into(), user.dir.to_string_lossy().into_owned());
            environment.insert("USER".into(), user.name.clone());
            environment.insert("LOGNAME".into(), user.name);
            environment.insert("SHELL".into(), user.shell.to_string_lossy().into_owned());
        }
    }
    environment
}

/// Only a supervisor confirmed dead may be launched over. An unreadable one
/// counts as alive: fencing it would SIGTERM a shell that is still serving.
fn confirmed_gone(liveness: std::io::Result<crate::stepd::StepdLiveness>) -> bool {
    matches!(liveness, Ok(crate::stepd::StepdLiveness::Stale))
}

fn terminal_supervisor_is_gone(descriptor: &crate::stepd::StepdDescriptor) -> bool {
    confirmed_gone(crate::stepd::stepd_liveness(descriptor))
}

struct TerminalIdentity {
    job_id: u32,
    step_id: u32,
    node: String,
}

/// The step id stays the client's user step, which is what puts the shell in a
/// cgroup leaf under the job rather than on the job node itself.
fn terminal_launch_config(
    entry: &crate::job_entry::JobEntry,
    identity: TerminalIdentity,
    argv: &[String],
    winsize: Option<crate::pty::WindowSize>,
    job: TerminalJobFacts,
    memlock: spur_core::config::MemlockLimit,
    cgroup: CgroupConfig,
) -> Result<executor::JobLaunchConfig, Status> {
    let TerminalIdentity {
        job_id,
        step_id,
        node,
    } = identity;
    let environment = terminal_environment(entry, job_id);
    let script = supervised_step_script(entry, entry.uid, entry.gid, &session_shell(entry, argv))?;
    Ok(executor::JobLaunchConfig {
        job_id,
        step_id,
        run_attempt: job.run_attempt,
        script,
        work_dir: match entry.work_dir.is_empty() {
            true => "/tmp".to_string(),
            false => entry.work_dir.clone(),
        },
        name: format!("{job_id}.{step_id}"),
        user: environment.get("USER").cloned().unwrap_or_default(),
        node,
        array_job_id: None,
        array_task_id: None,
        environment,
        // The terminal is the session's only output; nothing is spooled.
        stdout_path: String::new(),
        stderr_path: String::new(),
        stdin_path: String::new(),
        open_mode: None,
        cpus: job.cpus,
        memory_mb: job.memory_mb,
        gpu_devices: job.gpu_devices,
        cpu_ids: Vec::new(),
        uid: entry.uid,
        gid: entry.gid,
        container: None,
        prolog_script: None,
        partition: job.partition,
        nodelist: job.nodelist,
        mpi: String::new(),
        host_device_plan: None,
        memlock,
        cgroup,
        io_mode: executor::LaunchIo::Pty(winsize),
        pmix_multi_task: false,
        joins_parent_namespaces: entry.has_namespaces() && entry.pid > 0,
        allocation_holder: false,
    })
}

/// What an interactive session runs when the client named nothing. A job in its
/// own mount namespace is probed through /proc, not on the host filesystem.
fn session_shell(entry: &crate::job_entry::JobEntry, argv: &[String]) -> Vec<String> {
    if !argv.is_empty() {
        return argv.to_vec();
    }
    let bash = match entry.pid > 0 && entry.has_mount_namespace {
        true => format!("/proc/{}/root/bin/bash", entry.pid),
        false => "/bin/bash".to_string(),
    };
    match std::path::Path::new(&bash).exists() {
        true => vec!["/bin/bash".to_string()],
        false => vec!["/bin/sh".to_string()],
    }
}

/// The script a supervised step runs. A step joining a running job enters its
/// namespaces from inside the script, so the supervisor itself stays outside
/// them and survives independently of the job it is running work for.
fn supervised_step_script(
    job_entry: &crate::job_entry::JobEntry,
    uid: u32,
    gid: u32,
    command: &[String],
) -> Result<String, Status> {
    if !(job_entry.has_namespaces() && job_entry.pid > 0) {
        return build_one_shot_command_script(command);
    }
    let priv_drop = crate::privdrop::PrivDrop::resolve_if_needed(uid, gid);
    let plan = build_launch_plan(job_entry, priv_drop.as_ref(), command);
    let entering = shlex::try_join(
        std::iter::once(plan.program.as_str()).chain(plan.args.iter().map(String::as_str)),
    )
    .map_err(|e| Status::invalid_argument(format!("step command is not shell-safe: {e}")))?;
    Ok(format!("#!/bin/bash\nexec {entering}\n"))
}

/// The same shape RunCommand would have returned, output included: a user
/// step's spool outlives its teardown precisely so this read-back can happen.
async fn awaited_step_response(
    job_id: u32,
    step_id: u32,
    exit_code: i32,
    signal: i32,
) -> RunCommandResponse {
    RunCommandResponse {
        exit_code: spur_core::process::shell_exit_code(&step_exit_status(
            crate::step_completion::StepOutcome { exit_code, signal },
        )),
        stdout: read_back_step_output(job_id, step_id, false).await,
        stderr: read_back_step_output(job_id, step_id, true).await,
    }
}

async fn read_back_step_output(job_id: u32, step_id: u32, stderr: bool) -> String {
    for path in crate::executor::step_output_path_candidates(job_id, step_id, stderr) {
        if let Ok(bytes) = tokio::fs::read(&path).await {
            return String::from_utf8_lossy(&bytes).into_owned();
        }
    }
    String::new()
}

/// wait(2) encoding, so a caller comparing against a directly-reaped step's
/// status sees the same shape.
fn step_exit_status(outcome: crate::step_completion::StepOutcome) -> std::process::ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    if outcome.signal != 0 {
        std::process::ExitStatus::from_raw(outcome.signal)
    } else {
        std::process::ExitStatus::from_raw(outcome.exit_code << 8)
    }
}

/// The supervisor forks the workload after the gate opens, so its pid is not
/// available the instant `start_job` returns.
async fn supervised_step_workload_pid(descriptor: &crate::stepd::StepdDescriptor) -> Option<u32> {
    for _ in 0..WORKLOAD_PID_POLLS {
        match crate::stepd::query_state(descriptor, uuid::Uuid::new_v4().to_string()).await {
            Ok(snapshot) if snapshot.job_pid > 0 => return Some(snapshot.job_pid as u32),
            Ok(_) => {}
            Err(error) => {
                warn!(
                    job_id = descriptor.job_id,
                    step_id = descriptor.step_id,
                    %error,
                    "could not read the supervised step's workload pid"
                );
                return None;
            }
        }
        tokio::time::sleep(WORKLOAD_PID_POLL_INTERVAL).await;
    }
    None
}

async fn run_tokio_step_to_spool(
    mut cmd: tokio::process::Command,
    step_files: crate::executor::StepOutputFiles,
    active_steps: &Arc<Mutex<HashMap<(u32, u32), ActiveStep>>>,
    step_key: (u32, u32),
    cgroup: Option<&std::path::Path>,
    cgroup_required: bool,
) -> Result<Option<std::process::ExitStatus>, Status> {
    use std::process::Stdio;
    let mut child = cmd
        .stdout(Stdio::from(step_files.stdout))
        .stderr(Stdio::from(step_files.stderr))
        .spawn()
        .map_err(|e| Status::internal(format!("step command failed to spawn: {e}")))?;

    // The pre-exec join reports whether it wrote `cgroup.procs`; this verifies the
    // step actually landed there. Under `[cgroup] required` a step that did not is
    // refused and its group killed rather than run outside the limits and filter.
    if escaped_job_cgroup(cgroup_required, cgroup, child.id()) {
        kill_step_process_group(&mut child).await;
        return Err(Status::failed_precondition(
            "[cgroup] required but the step did not join its cgroup",
        ));
    }

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

/// What a containerized child execs after `container_init`. Both the buffered
/// step (a script file inside the rootfs) and the interactive PTY step (an
/// interactive shell or inline command) route through the same fork core.
enum StepChildCommand {
    /// Run `[entrypoint &&] <shell> <script_path>` — the buffered step path.
    Script {
        script_path: String,
        entrypoint: Option<String>,
    },
    /// Exec an interactive shell in place. `None` cmdline = the image's default
    /// shell; `Some(cmdline)` runs `<shell> -c "exec <cmdline>"` so the workload
    /// replaces the shell (keeping the PTY session leader as the real process,
    /// which job control and Ctrl-C depend on).
    Shell {
        cmdline: Option<String>,
        entrypoint: Option<String>,
    },
}

/// Reject NUL bytes in strings that reach `execve`: a NUL would silently degrade
/// the child's exec (e.g. to `bash -c ""`, which exits 0), reporting a step that
/// never ran as success. Checked before the fork so the error propagates cleanly.
fn reject_nul_bytes(cmd: &StepChildCommand) -> Result<(), Status> {
    let strings: [Option<&str>; 2] = match cmd {
        StepChildCommand::Script {
            script_path,
            entrypoint,
        } => [Some(script_path.as_str()), entrypoint.as_deref()],
        StepChildCommand::Shell {
            cmdline,
            entrypoint,
        } => [cmdline.as_deref(), entrypoint.as_deref()],
    };
    for s in strings.into_iter().flatten() {
        if s.as_bytes().contains(&0) {
            return Err(Status::invalid_argument(
                "container entrypoint or step command contains a NUL byte",
            ));
        }
    }
    Ok(())
}

/// Runs in a freshly forked child after its stdio has been wired (spool fds or a
/// PTY slave): joins the job cgroup while still root, runs `container_init`
/// (namespace unshare, mounts, device injection, pivot_root, priv drop), signals
/// readiness on `ready_w`, then execs `cmd`. Never returns. The whole namespace
/// setup is shared by the buffered and PTY containerized-step paths.
fn container_child_exec(
    container_cfg: &crate::container::ContainerConfig,
    rootfs: &std::path::Path,
    ready_w: std::os::fd::OwnedFd,
    memlock: spur_core::config::MemlockLimit,
    cgroup_join: &Option<executor::CgroupJoin>,
    env_base: HashMap<String, String>,
    cmd: StepChildCommand,
) -> ! {
    use std::os::fd::AsRawFd;
    let ready_w_fd = ready_w.as_raw_fd();

    // Join while still root, before container_init's pivot_root hides the host
    // cgroupfs and close_inherited_fds reaps the log fd.
    if let Some(ref join) = cgroup_join {
        let _ = join.join();
    }

    crate::container::close_inherited_fds(ready_w_fd);
    executor::apply_memlock(memlock);

    let hook_env = match crate::container::container_init(container_cfg, rootfs) {
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

    let mut final_env = env_base;
    for (k, v) in &container_cfg.container_env {
        final_env.insert(k.clone(), v.clone());
    }
    for (k, v) in hook_env {
        final_env.insert(k, v);
    }

    // Probe for a shell in the image (busybox/alpine has no /bin/bash), matching
    // the batch path in executor.rs.
    let shell = if std::path::Path::new("/bin/bash").exists() {
        "/bin/bash"
    } else {
        "/bin/sh"
    };
    let c_shell = std::ffi::CString::new(shell)
        .unwrap_or_else(|_| std::ffi::CString::new("/bin/sh").unwrap());
    let cstr = |s: &str| std::ffi::CString::new(s).unwrap_or_default();

    // NUL bytes were rejected before the fork, so CString::new cannot fail here.
    let argv: Vec<std::ffi::CString> = match cmd {
        StepChildCommand::Script {
            script_path,
            entrypoint,
        } => match entrypoint {
            Some(ep) => vec![
                c_shell.clone(),
                cstr("-c"),
                cstr(&format!("{ep} && {shell} {script_path}")),
            ],
            None => vec![c_shell.clone(), cstr(&script_path)],
        },
        StepChildCommand::Shell {
            cmdline,
            entrypoint,
        } => match (cmdline, entrypoint) {
            (None, None) => vec![c_shell.clone()],
            (Some(cmd), None) => vec![c_shell.clone(), cstr("-c"), cstr(&format!("exec {cmd}"))],
            (None, Some(ep)) => {
                vec![
                    c_shell.clone(),
                    cstr("-c"),
                    cstr(&format!("{ep} && exec {shell}")),
                ]
            }
            (Some(cmd), Some(ep)) => {
                vec![
                    c_shell.clone(),
                    cstr("-c"),
                    cstr(&format!("{ep} && exec {cmd}")),
                ]
            }
        },
    };
    let c_env: Vec<std::ffi::CString> = final_env
        .iter()
        .filter_map(|(k, v)| std::ffi::CString::new(format!("{k}={v}")).ok())
        .collect();
    let argv_refs: Vec<&std::ffi::CStr> = argv.iter().map(|s| s.as_c_str()).collect();
    let env_refs: Vec<&std::ffi::CStr> = c_env.iter().map(|s| s.as_c_str()).collect();
    let _ = nix::unistd::execve(&argv[0], &argv_refs, &env_refs);
    eprintln!(
        "spur: execve {shell} failed: {}",
        std::io::Error::last_os_error()
    );
    std::process::exit(127);
}

/// Parent-side readiness handshake for a containerized-step fork: read the "OK"
/// (or "E:<msg>") the child writes after `container_init`, then verify the child
/// landed in its cgroup under `[cgroup] required`. Kills and reaps the child on
/// either failure. Shared by the buffered and PTY containerized-step paths.
fn container_parent_ready(
    child_pid: nix::unistd::Pid,
    ready_r: std::os::fd::OwnedFd,
    cgroup_required: bool,
    cgroup: Option<&std::path::Path>,
) -> Result<(), Status> {
    use std::os::fd::AsRawFd;
    let ready_r_fd = ready_r.as_raw_fd();
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

    if escaped_job_cgroup(cgroup_required, cgroup, Some(child_pid.as_raw() as u32)) {
        crate::executor::kill_process_tree(child_pid.as_raw(), nix::sys::signal::Signal::SIGKILL);
        let _ = nix::sys::wait::waitpid(child_pid, None);
        return Err(Status::failed_precondition(
            "[cgroup] required but the step did not join its cgroup",
        ));
    }
    Ok(())
}

/// Reap a directly-forked child and map its wait status to a shell-style exit
/// code (128 + signal when killed). Used by the PTY bridge to await a container
/// step that was forked raw (no `tokio::process::Child` to `wait()` on).
fn waitpid_exit_code(pid: nix::unistd::Pid) -> i32 {
    match nix::sys::wait::waitpid(pid, None) {
        Ok(nix::sys::wait::WaitStatus::Exited(_, code)) => code,
        Ok(nix::sys::wait::WaitStatus::Signaled(_, sig, _)) => 128 + sig as i32,
        _ => 128,
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
    cgroup: Option<&std::path::Path>,
    cgroup_required: bool,
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

    // Raw fds of the spool files the step's stdout/stderr are redirected to.
    let stdout_fd = step_files.stdout.as_raw_fd();
    let stderr_fd = step_files.stderr.as_raw_fd();

    // Start from the image's own config.Env (as `docker run` does and as the
    // batch container path does), with the job environment layered on top; read
    // from the rootfs before the fork. Keeps a containerized step consistent with
    // a containerized batch job (e.g. tools on the image's PATH resolve).
    let env_base = spur_net::oci::container_base_env(&rootfs, env);
    let child_cmd = StepChildCommand::Script {
        script_path,
        entrypoint: container_cfg.entrypoint.clone(),
    };
    reject_nul_bytes(&child_cmd)?;

    // Built parent-side (nothing between fork and exec may allocate); the child
    // joins itself below while still root, and `required` is verified parent-side.
    let cgroup_join = executor::CgroupJoin::for_cgroup(cgroup);

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
            container_child_exec(
                &container_cfg,
                &rootfs,
                ready_w,
                memlock,
                &cgroup_join,
                env_base,
                child_cmd,
            );
        }
        nix::unistd::ForkResult::Parent { child: child_pid } => {
            // === PARENT ===
            drop(ready_w);
            // The spool-file fds belong to the child now; close our copies.
            drop(step_files);

            container_parent_ready(child_pid, ready_r, cgroup_required, cgroup)?;

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
    plugstack_path: String,
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
    /// Where a supervised step's launching RPC parks for its exit status.
    step_completions: crate::step_completion::StepCompletions,
    /// Jobs this agent is currently bridging a terminal for. A job absent here
    /// with a terminal in custody has been orphaned by a restart.
    live_ptys: Arc<Mutex<std::collections::HashSet<u32>>>,
    /// Serializes setup against teardown for a job id, which a re-dispatch reuses.
    lifecycle: crate::job_lifecycle::JobLifecycle,
    stepds: Arc<Mutex<StepdMap>>,
    stepd_state_dir: std::path::PathBuf,
    /// Unit tests exercise launch mechanics without a built `spurstepd`, so they
    /// keep the legacy path. Production always supervises.
    #[cfg(test)]
    force_legacy_launch: bool,
    /// `[auth] allow_root_jobs` — when false (default) this agent refuses to execute as uid 0.
    allow_root_jobs: bool,
    /// Whether spurd runs as root. Stored (not queried per call) so tests can drive the refusal
    /// path through a real RPC on an unprivileged runner.
    spurd_is_root: bool,
}

impl AgentService {
    /// Whether this launch must be refused. A run with no record has no cutoff
    /// and no admitted digest, so only its own deadline can refuse it.
    fn check_launch_fences(
        &self,
        job_id: u32,
        run_attempt: u32,
        launch_step: spur_core::step::StepId,
        fences: &crate::admission::LaunchFences,
    ) -> Option<crate::admission::LaunchRefusal> {
        let admissions = self.admissions();
        let run = named_run(job_id, run_attempt)?;
        let recorded = admissions.load_run(run).ok();
        // Only a live run can be asked to run a second command. A settled one's
        // digest would refuse a fresh job that merely reuses its identity.
        let digest = recorded
            .as_ref()
            .filter(|run| !run.is_over())
            .and_then(|_| {
                admissions
                    .participants(run)
                    .ok()
                    .and_then(|(participants, _)| {
                        participants
                            .into_iter()
                            .find(|p| p.step_id == launch_step && !p.command_digest.is_empty())
                            .map(|p| p.command_digest)
                    })
            });
        fences.check(
            crate::admission::now_unix_ms(),
            recorded.map(|r| r.reject_before_unix_ms).unwrap_or(0),
            digest.as_deref(),
        )
    }

    /// The entitlement ledger, keyed to the node this agent serves.
    pub(crate) fn admissions(&self) -> crate::admission::AdmissionStore {
        crate::admission::AdmissionStore::new(&self.stepd_state_dir, &self.reporter.hostname)
    }
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
            // Enforcement off: `/sys/fs/cgroup/spur` is a real path, so a root
            // runner would otherwise have these tests creating real cgroups.
            CgroupConfig {
                enabled: false,
                ..CgroupConfig::default()
            },
            MpiConfig::default(),
            new_running_jobs(),
            spur_core::config::AuthConfig::default().allow_root_jobs,
        )
        // Deterministic regardless of whether the test runner is root: a root runner would
        // otherwise make every launch test (which uses the default uid 0) hit the refusal.
        .with_root_override(false)
        // Every job is supervised now, so a test that launches needs a writable
        // runtime root; the production default is not writable unprivileged.
        .with_runtime_state_dir(
            std::env::temp_dir().join(format!("spur-test-runtime-{}", uuid::Uuid::new_v4())),
        )
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
            plugstack_path,
            mpi_host: Arc::new(MpiPluginHost::new(mpi)),
            hooks: Arc::new(hooks),
            limits,
            cgroup,
            device_registry,
            k0s: Arc::new(crate::cluster::K0sAgent::from_config(cluster)),
            active_steps: Arc::new(Mutex::new(HashMap::new())),
            step_completions: crate::step_completion::StepCompletions::new(),
            live_ptys: Arc::new(Mutex::new(std::collections::HashSet::new())),
            lifecycle: crate::job_lifecycle::JobLifecycle::default(),
            stepds: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(test)]
            force_legacy_launch: true,
            stepd_state_dir: std::env::var("SPUR_STEPD_STATE_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| std::path::PathBuf::from("/var/spool/spur")),
            allow_root_jobs,
            spurd_is_root: crate::privdrop::spurd_runs_as_root(),
        }
    }

    /// Give this node an epilog hook, so a test reaches the teardown ordering a
    /// cluster with one has: the record is marked cleaned before the hook runs.
    #[cfg(test)]
    fn with_epilog_hook(mut self, script: &str) -> Self {
        self.hooks = Arc::new(HooksConfig {
            epilog: Some(script.into()),
            ..HooksConfig::default()
        });
        self
    }

    /// Pretend spurd is (or is not) root, so a test can drive the uid-0 refusal through a real RPC
    /// on an unprivileged runner. Production always uses the value read at construction.
    #[cfg(test)]
    fn with_root_override(mut self, is_root: bool) -> Self {
        self.spurd_is_root = is_root;
        self
    }

    /// Take the supervised dispatch a test would otherwise never reach. The
    /// spawn fails without a built `spurstepd`, which is enough to pin the
    /// decisions made before it.
    #[cfg(test)]
    fn with_supervised_launch(mut self) -> Self {
        self.force_legacy_launch = false;
        self
    }

    /// Handle to the RPC-driven k0s component owner. spurd `main()` spawns its supervise loop.
    pub fn k0s(&self) -> Arc<crate::cluster::K0sAgent> {
        self.k0s.clone()
    }

    pub fn with_runtime_state_dir(mut self, state_dir: impl Into<std::path::PathBuf>) -> Self {
        self.stepd_state_dir = state_dir.into();
        self
    }

    pub async fn adopt_stepds(&self, descriptors: &[crate::stepd::StepdDescriptor]) {
        let mut sessions = self.stepds.lock().await;
        for descriptor in descriptors {
            let mut descriptor = descriptor.clone();
            // A recorded path is treated as authoritative for teardown, so one
            // that no longer exists has to fall back to answering for the workload.
            if !descriptor.cgroup_path.as_os_str().is_empty() && !descriptor.cgroup_path.is_dir() {
                warn!(
                    job_id = descriptor.job_id,
                    run_attempt = descriptor.run_attempt,
                    step_id = descriptor.step_id,
                    cgroup_path = %descriptor.cgroup_path.display(),
                    "adopted a stepd whose recorded cgroup is gone; deriving instead"
                );
                descriptor.cgroup_path = std::path::PathBuf::new();
            }
            sessions.insert(stepd_key(&descriptor), descriptor);
        }
    }

    /// Rebuild the claim index from the admission records, which are exact and
    /// complete before any supervisor exists. Nothing here releases anything.
    pub async fn replay_admitted_allocations(
        &self,
        descriptors: &[crate::stepd::StepdDescriptor],
    ) -> Vec<AdoptedRunOutcome> {
        let admissions = self.admissions();
        let store = crate::stepd::StepdStore::new(&self.stepd_state_dir);
        let boot_id = crate::admission::current_boot_id();
        // Before the read below, so the rebuilt ledger sees the settled values.
        // Gated on the owner: an adopted supervisor outlives this restart.
        match admissions.settle_hooks_whose_owner_is_gone(hook_owner_did_not_survive_restart) {
            Ok(settled) if settled > 0 => {
                warn!(
                    runs = settled,
                    "settled epilogs whose owner did not survive"
                )
            }
            Ok(_) => {}
            Err(error) => warn!(%error, "could not settle the epilogs of a previous agent"),
        }
        let loaded = match admissions.load_all() {
            Ok(loaded) => loaded,
            // An empty index reads as a free node while the jobs are still on
            // it, so fall back to what the descriptors can still account for.
            Err(error) => {
                error!(%error, "could not read the admission ledger; falling back to descriptors");
                self.replay_adopted_allocations(descriptors).await;
                return Vec::new();
            }
        };

        let mut outcomes = Vec::new();
        for rejected in &loaded.rejected {
            let (job_id, run_attempt) =
                crate::admission::parse_run_dir_name(&rejected.path).unwrap_or_default();
            // Its slice is unknown, so nothing is restored for it. The node is
            // short until the controller reconciles, which is the safe side.
            warn!(
                job_id,
                run_attempt,
                path = %rejected.path.display(),
                reason = %rejected.reason,
                "unreadable admission record; its claim needs reconciliation"
            );
            outcomes.push(AdoptedRunOutcome {
                job_id,
                run_attempt,
                disposition: crate::admission::RunDisposition::Corrupt,
            });
        }

        // A record whose supervisor is proven gone must not take a contested core
        // from one still running: the loser of that race goes unprotected.
        let mut ordered: Vec<&crate::admission::AdmittedRun> = loaded.runs.iter().collect();
        ordered.sort_by_key(|admitted| recorded_supervisor_liveness(admitted) == Liveness::Gone);

        let mut allocation = self.allocation.lock().await;
        for admitted in ordered {
            let run = &admitted.run;
            let evidence =
                self.gather_run_evidence(admitted, descriptors, &store, boot_id.as_deref());
            let disposition = crate::admission::classify_run(&evidence);

            // Every disposition holds, but a slice already handed back does not:
            // re-charging one takes a core from whatever the node gave it to.
            if run.slice_released {
                tracing::debug!(
                    job_id = run.job_id,
                    run_attempt = run.run_attempt,
                    ?disposition,
                    "an admitted run's slice was already released; not re-charging it"
                );
            } else {
                // The recorded slice is exact, so this does not under-count the
                // way a descriptor rebuild does.
                match allocation.restore_for_job(
                    run.job_id,
                    run.run_attempt,
                    &run.allocation.cpu_ids,
                    run.allocation.memory_mb,
                    &run.allocation.gpu_devices,
                ) {
                    Ok(_) => info!(
                        job_id = run.job_id,
                        run_attempt = run.run_attempt,
                        cpus = run.allocation.cpu_ids.len(),
                        gpus = run.allocation.gpu_devices.len(),
                        ?disposition,
                        "restored an admitted run's claim"
                    ),
                    // Superseded means a newer attempt already holds the slice,
                    // which is the expected outcome, not a loss.
                    Err(spur_sched::cons_tres::AllocError::Superseded) => tracing::debug!(
                        job_id = run.job_id,
                        run_attempt = run.run_attempt,
                        "a newer attempt already holds this run's slice"
                    ),
                    Err(error) => error!(
                        job_id = run.job_id,
                        run_attempt = run.run_attempt,
                        ?error,
                        "could not restore an admitted run's claim; the slice is unprotected"
                    ),
                }
            }
            outcomes.push(AdoptedRunOutcome {
                job_id: run.job_id,
                run_attempt: run.run_attempt,
                disposition,
            });
        }
        drop(allocation);

        // A session with no record predates this ledger. Falling back keeps an
        // in-place upgrade over a live node lossless.
        let unrecorded: Vec<crate::stepd::StepdDescriptor> = descriptors
            .iter()
            .filter(|d| {
                !loaded
                    .runs
                    .iter()
                    .any(|r| r.run.job_id == d.job_id && r.run.run_attempt == d.run_attempt)
            })
            .cloned()
            .collect();
        if !unrecorded.is_empty() {
            info!(
                sessions = unrecorded.len(),
                "replaying sessions that predate the admission ledger"
            );
            self.replay_adopted_allocations(&unrecorded).await;
        }
        outcomes
    }

    fn gather_run_evidence(
        &self,
        admitted: &crate::admission::AdmittedRun,
        descriptors: &[crate::stepd::StepdDescriptor],
        store: &crate::stepd::StepdStore,
        boot_id: Option<&str>,
    ) -> crate::admission::RunEvidence {
        let run = &admitted.run;
        let mine: Vec<&crate::stepd::StepdDescriptor> = descriptors
            .iter()
            .filter(|d| d.job_id == run.job_id && d.run_attempt == run.run_attempt)
            .collect();
        let boot_scope_of_supervisor = admitted
            .participants
            .iter()
            .filter_map(|p| p.supervisor.as_ref())
            .map(|s| s.boot_scope(boot_id))
            .chain(mine.iter().map(|d| {
                crate::admission::SupervisorRef {
                    pid: d.pid,
                    start_ticks: d.process_start_ticks,
                    boot_id: d.boot_id.clone(),
                }
                .boot_scope(boot_id)
            }))
            // A single definite reboot settles the run; otherwise an unknown
            // must not be promoted into a change.
            .reduce(|a, b| match (a, b) {
                (crate::admission::BootScope::Different, _)
                | (_, crate::admission::BootScope::Different) => {
                    crate::admission::BootScope::Different
                }
                (crate::admission::BootScope::Same, _) | (_, crate::admission::BootScope::Same) => {
                    crate::admission::BootScope::Same
                }
                _ => crate::admission::BootScope::Unknown,
            });
        // From the record's participants, not the live set: an exit only exists
        // once the supervisor is gone, so a live descriptor never carries one.
        let recorded_exit = admitted.participants.iter().find_map(|p| {
            store
                .observed_exit(run.job_id, run.run_attempt, p.step_id)
                .ok()
                .flatten()
        });
        // Re-checked rather than trusted from the caller's bucket: reporting a
        // dead supervisor as running holds a phantom indefinitely.
        let live = mine.iter().any(|d| {
            matches!(
                crate::stepd::stepd_liveness(d),
                Ok(crate::stepd::StepdLiveness::Live)
            )
        });
        crate::admission::RunEvidence {
            supervisor_matches_a_live_process: live,
            boot_scope_of_supervisor,
            recorded_exit,
            never_spawned: !live
                && mine.is_empty()
                && admitted.participants.iter().all(|p| p.supervisor.is_none()),
            residual_or_corrupt: false,
        }
    }

    /// Put adopted jobs back on the node's ledger, which is built empty — only
    /// correct back when a restart killed every job it could have held.
    pub async fn replay_adopted_allocations(&self, descriptors: &[crate::stepd::StepdDescriptor]) {
        let mut allocation = self.allocation.lock().await;
        for descriptor in replayable_allocations(descriptors) {
            let resources = &descriptor.resources;
            // Dropping a contested core costs this job some occupancy; dropping
            // the whole replay would leave its live GPUs reading free.
            let cpu_ids = if resources.cpu_ids.is_empty() {
                fallback_cpu_ids(&allocation, resources.cpus)
            } else {
                allocation.claimable_cpu_ids(descriptor.job_id, &resources.cpu_ids)
            };
            if cpu_ids.len() < resources.cpus as usize {
                warn!(
                    job_id = descriptor.job_id,
                    wanted = resources.cpus,
                    got = cpu_ids.len(),
                    "fewer free cores than an adopted job recorded; its cores are \
                     under-counted until it ends"
                );
            }
            match allocation.restore_for_job(
                descriptor.job_id,
                descriptor.run_attempt,
                &cpu_ids,
                resources.memory_mb,
                &resources.gpu_devices,
            ) {
                Ok(_) => info!(
                    job_id = descriptor.job_id,
                    run_attempt = descriptor.run_attempt,
                    cpus = cpu_ids.len(),
                    gpus = resources.gpu_devices.len(),
                    "restored an adopted job's allocation"
                ),
                // Refusing to serve would strand the adopted job with no agent to
                // report it, and nothing reconciles a job the ledger never saw.
                Err(error) => warn!(
                    job_id = descriptor.job_id,
                    run_attempt = descriptor.run_attempt,
                    ?error,
                    "could not restore an adopted job's allocation; this node's \
                     accounting is short until the job ends"
                ),
            }
        }
    }

    /// A stale session's supervisor is confirmed dead, so nothing adopts it and
    /// nothing else will ever speak for it: carry any exit it recorded to the
    /// caller still waiting on the step, and release it so it can be pruned.
    pub async fn settle_stale_stepds(&self, descriptors: &[crate::stepd::StepdDescriptor]) {
        let store = crate::stepd::StepdStore::new(&self.stepd_state_dir);
        for descriptor in descriptors {
            match durable_runtime_exit(&store, descriptor) {
                Ok(Some((exit_code, signal))) => {
                    self.step_completions
                        .complete(
                            descriptor.job_id,
                            descriptor.run_attempt,
                            descriptor.step_id,
                            crate::step_completion::StepOutcome { exit_code, signal },
                        )
                        .await;
                }
                // A job-level session is usually given a synthetic exit at
                // startup; whatever is left here the recovery report speaks for.
                Ok(None) => {}
                Err(error) => {
                    warn!(
                        job_id = descriptor.job_id,
                        run_attempt = descriptor.run_attempt,
                        step_id = descriptor.step_id,
                        %error,
                        "failed to read a stale session's recorded exit"
                    );
                }
            }
            if let Err(error) = crate::stepd::record_resources_released(descriptor) {
                warn!(
                    job_id = descriptor.job_id,
                    run_attempt = descriptor.run_attempt,
                    step_id = descriptor.step_id,
                    %error,
                    "failed to record runtime resource release for a stale session"
                );
            }
        }
    }

    pub fn monitor_recovered_stepds(&self, descriptors: &[crate::stepd::StepdDescriptor]) {
        monitor_recovered_stepds(
            RecoveryContext {
                running: self.running.clone(),
                allocation: self.allocation.clone(),
                stepds: self.stepds.clone(),
                completions: self.step_completions.clone(),
                store: crate::stepd::StepdStore::new(&self.stepd_state_dir),
                admissions: self.admissions(),
                controller_addr: self.reporter.controller_addr.clone(),
            },
            descriptors.to_vec(),
        );
    }

    pub fn monitor_stepd_liveness(&self) {
        monitor_stepd_liveness(self.completion_listener_context());
    }

    /// A handle to the tracked-Stepd map — the only jobs that survive
    /// this process exiting.
    pub fn stepds_handle(&self) -> Arc<Mutex<StepdMap>> {
        self.stepds.clone()
    }

    /// A handle to this node's ledger of charged slices.
    pub fn allocation_handle(&self) -> Arc<Mutex<NodeAllocation>> {
        self.allocation.clone()
    }

    /// The job's own process, asked of its supervisor over the control socket —
    /// live state, so it is not kept in the durable descriptor.
    async fn supervised_state(&self, job_id: u32) -> Option<crate::stepd::StepdSnapshot> {
        let descriptors = stepds_for_job(&*self.stepds.lock().await, job_id);
        for descriptor in descriptors {
            let state =
                crate::stepd::query_state(&descriptor, uuid::Uuid::new_v4().to_string()).await;
            match state {
                Ok(snapshot) if snapshot.job_pid != 0 => return Some(snapshot),
                Ok(_) => {}
                Err(error) => warn!(job_id, step_id = descriptor.step_id, %error,
                    "could not read the supervised job's state"),
            }
        }
        None
    }

    /// Run a numbered step under its own supervisor. The job already holds its
    /// allocation and tracking, so none of LaunchJob's bookkeeping repeats here.
    async fn run_supervised_step_to_spool(
        &self,
        cfg: &executor::JobLaunchConfig,
        pmix: Option<crate::stepd::StepdPmix>,
        step_files: crate::executor::StepOutputFiles,
        step_key: (u32, u32),
    ) -> Result<Option<std::process::ExitStatus>, Status> {
        let (job_id, step_id) = step_key;
        let run_attempt = cfg.run_attempt;
        // The supervisor opens the spool files itself from the launch spec;
        // holding our own copies would pin the fds for the life of the step.
        drop(step_files);

        fence_displaced_stepd(&self.stepds, job_id, step_id, run_attempt)
            .await
            .map_err(|error| {
                Status::internal(format!(
                    "could not fence a displaced step supervisor: {error}"
                ))
            })?;

        // Registered before the spawn: a step that outruns this RPC would
        // otherwise deliver its exit status to nobody.
        let waiter = self
            .step_completions
            .register(job_id, run_attempt, step_id)
            .await;

        let launched = launch_stepd(
            cfg,
            run_attempt,
            &self.reporter.controller_addr,
            &self.reporter.hostname,
            &self.stepd_state_dir,
            StepdLaunchOptions {
                step_id,
                allocation_only: false,
                container_rootfs_mode: None,
                hooks: (*self.hooks).clone(),
                plugstack_path: self.plugstack_path.clone(),
                pmix,
            },
        )
        .await;
        let descriptor = match launched {
            Ok((_, descriptor)) => descriptor,
            Err(error) => {
                self.step_completions
                    .deregister(job_id, run_attempt, step_id)
                    .await;
                return Err(Status::internal(format!(
                    "step supervisor failed to start: {error}"
                )));
            }
        };

        if let Err(descriptor) = claim_stepd_slot(&self.stepds, descriptor.clone()).await {
            self.step_completions
                .deregister(job_id, run_attempt, step_id)
                .await;
            discard_stepd_session(&descriptor).await;
            return Err(Status::aborted(
                "step supervisor superseded before it could be tracked",
            ));
        }

        if let Err(error) =
            crate::stepd::start_job(&descriptor, uuid::Uuid::new_v4().to_string()).await
        {
            // The gate is still shut, so the workload never ran.
            self.step_completions
                .deregister(job_id, run_attempt, step_id)
                .await;
            if let Err(stop_error) = stop_stepd_process(&descriptor).await {
                warn!(job_id, step_id, %stop_error, "failed to stop an unreleased step supervisor");
            }
            release_stepd_tracking(
                &self.running,
                &self.allocation,
                &self.stepds,
                &descriptor,
                "supervised step could not be released",
            )
            .await;
            return Err(Status::internal(format!(
                "failed to release the supervised step: {error}"
            )));
        }

        // Record the workload's pid, not the supervisor's, so the existing
        // cancel path signals the step's own tree and leaves its reporter alive.
        if let Some(pid) = supervised_step_workload_pid(&descriptor).await {
            let cancel_now = {
                let mut steps = self.active_steps.lock().await;
                match steps.get_mut(&step_key) {
                    Some(step) => {
                        step.pid = Some(pid);
                        step.cancel_requested
                    }
                    None => false,
                }
            };
            if cancel_now {
                signal_step_tree(pid, nix::sys::signal::Signal::SIGTERM as i32);
            }
        }

        match waiter.await {
            Ok(outcome) => Ok(Some(step_exit_status(outcome))),
            // The supervisor died without reporting; its cgroup and session are
            // reaped by the crash watchdog, so only the status is lost here.
            Err(_) => Err(Status::internal(
                "step supervisor exited without reporting a status",
            )),
        }
    }

    /// Take up this step's terminal, opening one only if the step has none.
    /// A reattach must find the shell already running, not start a second one.
    async fn attach_terminal(
        &self,
        entry: &crate::job_entry::JobEntry,
        init: &InitSession,
        argv: &[String],
        winsize: Option<crate::pty::WindowSize>,
    ) -> Result<SupervisedTerminal, Status> {
        let held = stepds_for_job(&*self.stepds.lock().await, init.job_id)
            .into_iter()
            .find(|descriptor| descriptor.step_id == init.step_id);
        // Only a supervisor that is confirmed gone may be launched over: fencing
        // a live one would SIGTERM the shell whose terminal was asked for.
        if let Some(descriptor) = held.filter(|descriptor| !terminal_supervisor_is_gone(descriptor))
        {
            return self.resume_terminal(&descriptor).await;
        }
        self.launch_supervised_terminal(entry, init.job_id, init.step_id, argv, winsize)
            .await
    }

    /// Take over a terminal whose supervisor is still running. Refused rather
    /// than shared: two bridges on one master interleave the user's keystrokes.
    async fn resume_terminal(
        &self,
        descriptor: &crate::stepd::StepdDescriptor,
    ) -> Result<SupervisedTerminal, Status> {
        let (job_id, step_id) = (descriptor.job_id, descriptor.step_id);
        let waiter = self
            .step_completions
            .reregister(job_id, descriptor.run_attempt, step_id)
            .await
            .ok_or_else(|| {
                Status::already_exists(format!(
                    "job {job_id} step {step_id} already has a client attached"
                ))
            })?;
        match self.claim_terminal(descriptor, waiter).await {
            Some(terminal) => {
                info!(
                    job_id,
                    step_id,
                    child_pid = terminal.pid,
                    "resumed a supervised terminal"
                );
                Ok(terminal)
            }
            None => {
                // Ours to clear: reregister refused to hand one out while another
                // client's was parked, so this waiter can only be the one above.
                self.step_completions
                    .deregister(job_id, descriptor.run_attempt, step_id)
                    .await;
                Err(Status::unavailable(format!(
                    "job {job_id} step {step_id} is running but its terminal could not be read"
                )))
            }
        }
    }

    /// The shell becomes a child of a process that outlives the agent, and
    /// `scancel` reaches it through the stepd map like every other step.
    async fn launch_supervised_terminal(
        &self,
        entry: &crate::job_entry::JobEntry,
        job_id: u32,
        step_id: u32,
        argv: &[String],
        winsize: Option<crate::pty::WindowSize>,
    ) -> Result<SupervisedTerminal, Status> {
        let tracked = {
            let jobs = self.running.lock().await;
            let tracked = jobs.get(&job_id).ok_or_else(|| {
                Status::failed_precondition(format!("job {job_id} is not running on this node"))
            })?;
            TerminalJobFacts {
                run_attempt: tracked.run_attempt,
                cpus: tracked.cpus,
                memory_mb: tracked.memory_mb,
                gpu_devices: tracked.gpu_devices.clone(),
                partition: tracked.partition.clone(),
                nodelist: tracked.nodelist.clone(),
            }
        };
        let run_attempt = tracked.run_attempt;

        let config = terminal_launch_config(
            entry,
            TerminalIdentity {
                job_id,
                step_id,
                node: self.reporter.hostname.clone(),
            },
            argv,
            winsize,
            tracked,
            self.limits.memlock,
            self.cgroup.clone(),
        )?;

        fence_displaced_stepd(&self.stepds, job_id, step_id, run_attempt)
            .await
            .map_err(|error| {
                Status::internal(format!(
                    "could not fence a displaced terminal supervisor: {error}"
                ))
            })?;

        // Registered before the spawn: a shell that exits immediately would
        // otherwise deliver its status to nobody.
        let waiter = self
            .step_completions
            .register(job_id, run_attempt, step_id)
            .await;

        let descriptor = match launch_stepd(
            &config,
            run_attempt,
            &self.reporter.controller_addr,
            &self.reporter.hostname,
            &self.stepd_state_dir,
            StepdLaunchOptions {
                step_id,
                allocation_only: false,
                container_rootfs_mode: None,
                hooks: (*self.hooks).clone(),
                plugstack_path: self.plugstack_path.clone(),
                pmix: None,
            },
        )
        .await
        {
            Ok((_, descriptor)) => descriptor,
            Err(error) => {
                self.step_completions
                    .deregister(job_id, run_attempt, step_id)
                    .await;
                return Err(Status::internal(format!(
                    "terminal supervisor failed to start: {error}"
                )));
            }
        };

        if let Err(descriptor) = claim_stepd_slot(&self.stepds, descriptor.clone()).await {
            self.step_completions
                .deregister(job_id, run_attempt, step_id)
                .await;
            discard_stepd_session(&descriptor).await;
            return Err(Status::aborted(
                "terminal supervisor superseded before it could be tracked",
            ));
        }

        if let Err(error) =
            crate::stepd::start_job(&descriptor, uuid::Uuid::new_v4().to_string()).await
        {
            self.step_completions
                .deregister(job_id, run_attempt, step_id)
                .await;
            self.abandon_terminal_supervisor(&descriptor, "terminal could not be released")
                .await;
            return Err(Status::internal(format!(
                "failed to release the terminal: {error}"
            )));
        }

        match self.claim_terminal(&descriptor, waiter).await {
            Some(terminal) => Ok(terminal),
            None => {
                self.step_completions
                    .deregister(job_id, run_attempt, step_id)
                    .await;
                self.abandon_terminal_supervisor(&descriptor, "terminal never became reachable")
                    .await;
                Err(Status::internal(
                    "the terminal supervisor never handed back a terminal",
                ))
            }
        }
    }

    /// Discard a supervisor whose terminal this agent could not take up. The
    /// shell may already be running, and stopping the supervisor alone would
    /// leave it on init still holding the step's cgroup.
    async fn abandon_terminal_supervisor(
        &self,
        descriptor: &crate::stepd::StepdDescriptor,
        reason: &'static str,
    ) {
        discard_stepd_session(descriptor).await;
        release_stepd_tracking(
            &self.running,
            &self.allocation,
            &self.stepds,
            descriptor,
            reason,
        )
        .await;
    }

    /// Take up a terminal a supervisor is holding: its shell's pid names the
    /// master in custody, and its exit arrives on the step-completion channel.
    async fn claim_terminal(
        &self,
        descriptor: &crate::stepd::StepdDescriptor,
        waiter: tokio::sync::oneshot::Receiver<crate::step_completion::StepOutcome>,
    ) -> Option<SupervisedTerminal> {
        let pid = supervised_step_workload_pid(descriptor).await?;
        let session_dir = descriptor.socket_path.parent()?;
        let master = match crate::stepd::reclaim_pty_master(session_dir, pid).await {
            Ok(master) => master?,
            Err(error) => {
                warn!(
                    job_id = descriptor.job_id,
                    step_id = descriptor.step_id,
                    %error,
                    "could not take up the supervised terminal"
                );
                return None;
            }
        };
        // AsyncFd in the bridge polls this fd; a blocking master would stall the
        // whole reactor on the first read that has nothing behind it.
        if let Err(error) = nix::fcntl::fcntl(
            &master,
            nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
        ) {
            warn!(job_id = descriptor.job_id, %error, "could not set the terminal non-blocking");
            return None;
        }
        Some(SupervisedTerminal {
            master,
            pid: pid as i32,
            exit: Box::pin(async move {
                match waiter.await {
                    Ok(outcome) => spur_core::process::shell_exit_code(&step_exit_status(outcome)),
                    Err(_) => 128,
                }
            }),
        })
    }

    pub fn stepd_recovery_cleanup(&self) -> StepdRecoveryCleanup {
        StepdRecoveryCleanup {
            running: self.running.clone(),
            allocation: self.allocation.clone(),
            stepds: self.stepds.clone(),
        }
    }

    pub fn completion_listener_context(&self) -> CompletionListenerContext {
        CompletionListenerContext {
            running: self.running.clone(),
            step_completions: self.step_completions.clone(),
            allocation: self.allocation.clone(),
            stepds: self.stepds.clone(),
            stepds_store: crate::stepd::StepdStore::new(&self.stepd_state_dir),
            admissions: self.admissions(),
            controller_addr: self.reporter.controller_addr.clone(),
            hostname: self.reporter.hostname.clone(),
        }
    }

    /// Spawn a background task to monitor running jobs and report completions.
    pub fn start_monitor(&self, controller_addr: String) {
        let running = self.running.clone();
        let allocation = self.allocation.clone();
        let spank = self.spank.clone();
        let mpi_host = self.mpi_host.clone();
        let hooks = self.hooks.clone();
        let lifecycle = self.lifecycle.clone();
        let admissions = self.admissions();
        let stepd_store = crate::stepd::StepdStore::new(&self.stepd_state_dir);
        let epilog = epilog_owed(&hooks);
        tokio::spawn(async move {
            let mut epilog_held_since: HashMap<RunKey, std::time::Instant> = HashMap::new();
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
                            let cgroup = tracked.take_cgroup();
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

                // Stays under `jobs`: release_job is job_id-keyed with no generation tag,
                // so dropping the lock first lets a redispatch's new attempt get torn down
                // by this cleanup, and reconcile below would misclassify it as an orphan.
                for c in &completed {
                    jobs.remove(&c.job_id);
                }
                // Teardown below SIGKILLs each cgroup and blocks retrying rmdir,
                // then runs hooks and RPCs; none of it may hold this lock.
                drop(jobs);

                for c in &completed {
                    teardown_completed_job(
                        c,
                        &lifecycle,
                        &running,
                        &allocation,
                        &mpi_host,
                        &admissions,
                        epilog,
                    )
                    .await;
                }

                // Report allocations with no tracked, non-launching job. `running` is re-taken before `allocation`,
                // the order commit_job uses, so the live set the reclaim reads
                // can't race a committing launch.
                {
                    let jobs = running.lock().await;
                    let unbacked = {
                        let alloc = allocation.lock().await;
                        alloc.unbacked_claims(
                            &jobs.keys().copied().collect(),
                            std::time::Instant::now(),
                            LAUNCHING_TTL,
                        )
                    };
                    drop(jobs);
                    // Strictly before the two sweeps below, which read the record
                    // this writes to decide whether a hook still holds the cores.
                    resolve_supervised_epilogs(&stepd_store, &admissions, &mut epilog_held_since)
                        .await;
                    // Record-driven, so a report path that records an
                    // acknowledgement without releasing cannot strand a slice.
                    settle_cancelled_runs(&lifecycle, &allocation, &admissions).await;
                    release_due_allocations(&allocation, &admissions).await;
                    flag_unbacked_allocations(&unbacked, &admissions).await;
                    // Strictly after the releases above: collecting a record
                    // before its slice is freed destroys the instruction to free it.
                    collect_settled_admissions(&allocation, &admissions).await;
                }

                let local_hostname = hostname::get()
                    .map(|h| h.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "localhost".into());

                // job_id -> drain reason, piggybacked on each job's completion
                // report so the node goes idle-and-drained in one message (no
                // window where a bad node looks schedulable).
                let mut drain_jobs: std::collections::HashMap<u32, String> =
                    std::collections::HashMap::new();

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
                        // Marked before the hook: an agent that dies inside one
                        // leaves a `Running` that reloads as unknowable.
                        let hooked = named_run(c.job_id, c.run_attempt);
                        if let Some(run) = hooked {
                            let _ =
                                admissions.record_epilog(run, crate::admission::HookState::Running);
                        }
                        let outcome = spur_core::hooks::run_hook(epilog_script, &ctx).await;
                        if let Some(run) = hooked {
                            let _ = admissions.record_epilog(run, epilog_outcome(outcome.is_err()));
                        }
                        if let Err(e) = outcome {
                            error!(
                                job_id = c.job_id,
                                error = %e,
                                "epilog hook failed — requesting node drain"
                            );
                            drain_jobs.insert(c.job_id, "epilog script failed".into());
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
                    let drain = drain_jobs.get(&c.job_id).map(|reason| DrainRequest {
                        reason: reason.clone(),
                    });
                    let acknowledged = report_completion(
                        &controller_addr,
                        CompletionReport {
                            job_id: c.job_id,
                            exit_code: c.exit_code,
                            signal: c.signal,
                            run_attempt: c.run_attempt,
                            reporting_node: &local_hostname,
                            drain: drain.as_ref(),
                            // Unsupervised path: the report speaks for the job.
                            step_id: None,
                            payload: PayloadEvidence::Reaped,
                        },
                    )
                    .await
                    .settled();
                    if !acknowledged {
                        // Held, not freed: nothing but this node knows the work
                        // stopped until the controller commits the completion.
                        warn!(
                            job_id = c.job_id,
                            run_attempt = c.run_attempt,
                            "completion unacknowledged; holding this run's resources"
                        );
                        continue;
                    }
                    if let Some(run) = named_run(c.job_id, c.run_attempt) {
                        settle_acknowledged_completion(
                            &allocation,
                            &admissions,
                            run,
                            spur_core::step::STEP_BATCH,
                        )
                        .await;
                    }
                }
            }
        });
    }
}

pub(crate) struct DrainRequest {
    pub(crate) reason: String,
}

/// How long a reservation may sit mid-launch before the sweep reports it,
/// sized above a typical image pull and fork.
const LAUNCHING_TTL: std::time::Duration = std::time::Duration::from_secs(600);

/// Flag claims the agent has no tracked job for. It does **not** free them:
/// forgetting a job is not evidence its work finished, and no timeout makes it so.
async fn flag_unbacked_allocations(
    unbacked: &[(u32, u32)],
    admissions: &crate::admission::AdmissionStore,
) {
    for &(job_id, run_attempt) in unbacked {
        let Some(run) = named_run(job_id, run_attempt) else {
            continue;
        };
        let store = admissions.clone();
        let taken = tokio::task::spawn_blocking(move || {
            store.take_conflict_hold(run, "held with no tracked job on this agent")
        })
        .await;
        match taken {
            // On the transition only: a claim held for the controller to
            // resolve would otherwise warn on every tick, forever.
            Ok(Ok(crate::admission::HoldOutcome::Taken)) => warn!(
                job_id,
                run_attempt, "holding a claim with no tracked job; it needs reconciliation"
            ),
            Ok(Ok(crate::admission::HoldOutcome::AlreadyHeld)) => {}
            Ok(Ok(crate::admission::HoldOutcome::AlreadyReleased)) => debug!(
                job_id,
                run_attempt, "a claim released since the sweep read it needs no hold"
            ),
            Ok(Ok(crate::admission::HoldOutcome::NoRecord)) => tracing::debug!(
                job_id,
                run_attempt,
                "holding a claim with neither a tracked job nor a record"
            ),
            Ok(Err(error)) => {
                warn!(job_id, run_attempt, %error, "failed to record a conflict hold")
            }
            Err(error) => warn!(job_id, run_attempt, %error, "conflict-hold task failed"),
        }
    }
}

/// Settle runs the controller cancelled, once their teardown has finished. A
/// completion for a run it has forgotten is never acknowledged, so nothing frees it.
async fn settle_cancelled_runs(
    lifecycle: &crate::job_lifecycle::JobLifecycle,
    allocation: &Arc<Mutex<NodeAllocation>>,
    admissions: &crate::admission::AdmissionStore,
) {
    let Ok(loaded) = admissions.load_all() else {
        return;
    };
    for admitted in loaded.runs {
        let run = &admitted.run;
        if !run.cancelled_by_controller
            || run.state != crate::admission::RunState::Cleaned
            || run.controller_ack.is_given()
        {
            continue;
        }
        let (job_id, run_attempt) = (run.job_id, run.run_attempt);
        let Some(run_key) = run.key() else { continue };
        // Held across the settle and the release under it, as the RPC that does
        // the same thing does: a re-dispatch of this id must not straddle them.
        let _lifecycle = lifecycle.acquire(job_id).await;
        // Settled and freed together, not on the next tick: settling makes the
        // record collectable and the sweep may take it before another pass reads it.
        match settle_and_release_run(allocation, admissions, run_key).await {
            Ok(SettleRunOutcome::NotQuiescent) => {
                debug!(
                    job_id,
                    run_attempt, "a hook is still running under a cancelled run"
                )
            }
            Ok(_) => {}
            Err(error) => warn!(job_id, run_attempt, %error, "failed to settle a cancelled run"),
        }
    }
}

/// Free every run the records say is due. Record-driven and idempotent, so a
/// report that acknowledges without releasing cannot strand a slice.
async fn release_due_allocations(
    allocation: &Arc<Mutex<NodeAllocation>>,
    admissions: &crate::admission::AdmissionStore,
) {
    let Ok(loaded) = admissions.load_all() else {
        return;
    };
    for admitted in loaded.runs {
        let (Some(run), Some(step_id)) = (admitted.run.key(), admitted.lifecycle_step()) else {
            continue;
        };
        release_acknowledged_allocation(allocation, admissions, run, step_id).await;
    }
}

/// Collect records nothing is owed on, sparing every run the allocator still
/// charges: its record is the only thing that can still order that release.
async fn collect_settled_admissions(
    allocation: &Arc<Mutex<NodeAllocation>>,
    admissions: &crate::admission::AdmissionStore,
) {
    // A charge no attempt names still spares its own job's records — widened into
    // the set — rather than stopping the sweep for every other job on the node.
    let (charged, unnameable) = allocation.lock().await.charged_runs();
    for job_id in unnameable {
        warn!(
            job_id,
            "this node charges a reservation that names no run; keeping its records"
        );
    }
    let store = admissions.clone();
    let swept = tokio::task::spawn_blocking(move || {
        store.sweep(
            crate::admission::now_unix_ms(),
            crate::admission::DEFAULT_RETENTION_SECS * 1000,
            &charged,
        )
    })
    .await;
    match swept {
        Ok(Ok(0)) => {}
        Ok(Ok(swept)) => info!(swept, "swept settled admission records"),
        Ok(Err(error)) => warn!(%error, "failed to sweep admission records"),
        Err(error) => warn!(%error, "admission sweep task failed"),
    }
}

/// A refusal the controller can resolve by looking at this node. Carried in a
/// response, not a transport error, which has nowhere to say any of this.
#[derive(Debug)]
pub(crate) struct HeldRefusal {
    message: String,
    conflict: Option<LaunchConflict>,
}

impl HeldRefusal {
    /// The node can say which job holds the resources, so the controller can
    /// look that job up and decide which of the two is stale.
    fn named(job_id: u32, run_attempt: u32, held: &AllocationResult) -> Self {
        Self {
            message: format!("allocated resources are held on this node by job {job_id}"),
            conflict: Some(LaunchConflict {
                job_id,
                run_attempt,
                cpu_ids: held.cpu_ids.clone(),
                gpu_devices: held.gpu_ids.clone(),
            }),
        }
    }

    /// The node is holding something it cannot attribute to a job: only a full
    /// ledger pull can tell the controller what.
    fn unattributed(detail: &str) -> Self {
        Self {
            message: detail.to_string(),
            conflict: None,
        }
    }

    /// Derived from the conflict rather than stored beside it: an overlap the
    /// controller is told to look up must always carry the job to look up.
    pub(crate) fn kind(&self) -> LaunchFailureKind {
        match self.conflict {
            Some(_) => LaunchFailureKind::LaunchFailureLocalOverlap,
            None => LaunchFailureKind::LaunchFailureResidualState,
        }
    }

    pub(crate) fn into_response(self) -> LaunchJobResponse {
        let failure_kind = self.kind() as i32;
        LaunchJobResponse {
            success: false,
            error: self.message,
            stdout_path: String::new(),
            stderr_path: String::new(),
            failure_kind,
            conflict: self.conflict,
        }
    }
}

/// Describe what the node holds, or say it cannot. Both are answers the
/// controller can act on; only the first names a job for it to adjudicate.
fn held_refusal(alloc: &NodeAllocation, holder: Option<u32>, detail: &str) -> HeldRefusal {
    match holder.and_then(|job_id| alloc.claim_of(job_id).map(|claim| (job_id, claim))) {
        Some((job_id, (run_attempt, held))) => HeldRefusal::named(job_id, run_attempt, &held),
        None => HeldRefusal::unattributed(detail),
    }
}

/// A committed owner the node is not tracking as live that holds cores: the
/// claim behind a shortfall. One holding none of them explains nothing.
fn unexplained_cpu_holder(alloc: &NodeAllocation, live: &HashSet<u32>) -> Option<u32> {
    alloc.committed_owners().into_iter().find(|owner| {
        !live.contains(owner)
            && alloc
                .claim_of(*owner)
                .is_some_and(|(_, held)| !held.cpu_ids.is_empty())
    })
}

/// Why a launch could not reserve its slice: something this node is holding,
/// which the controller resolves by looking, or an error only it can act on.
#[derive(Debug)]
pub(crate) enum LaunchRejection {
    Held(HeldRefusal),
    Failed(Status),
}

impl From<Status> for LaunchRejection {
    fn from(status: Status) -> Self {
        Self::Failed(status)
    }
}

/// What became of a release the record was asked to license.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReleaseOutcome {
    Freed,
    AlreadyFree,
    /// The record licenses nothing yet, so the slice stays charged.
    NotDue,
}

/// Free a run's slice, now that the controller has committed its completion.
/// This is the only path that frees one: an exit alone never does.
async fn release_acknowledged_allocation(
    allocation: &Arc<Mutex<NodeAllocation>>,
    admissions: &crate::admission::AdmissionStore,
    run: RunKey,
    step_id: spur_core::step::StepId,
) -> ReleaseOutcome {
    let warrant = match admissions.release_is_due(run, step_id) {
        Ok(Some(warrant)) => warrant,
        Ok(None) => return ReleaseOutcome::NotDue,
        Err(error) => {
            warn!(%run, %error, "could not tell whether a release is due; holding");
            return ReleaseOutcome::NotDue;
        }
    };
    // Recorded on the release line below: the decision a slice rests on is the
    // whole of the rule that only a committed acknowledgement may free one.
    let ground = warrant.ground();
    let released = allocation.lock().await.release_job(warrant);
    // The cut is driven off this, so a slice given back without it recorded
    // stays advertised as a claim the node no longer holds.
    if let Err(error) = admissions.record_slice_released(run) {
        warn!(%run, %error, "failed to record a released slice");
    }
    if !released {
        // The audit reads the line below as the release; without this one, a
        // slice freed elsewhere is indistinguishable from a leak.
        debug!(%run, %ground, "a run's slice was already free at its acknowledgement");
        return ReleaseOutcome::AlreadyFree;
    }
    info!(%run, %ground, "released a run's slice on its acknowledgement");
    ReleaseOutcome::Freed
}

/// What came of taking the controller's word that it is not accounting for a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettleRunOutcome {
    Released,
    AlreadyFree,
    NoRecord,
    NotQuiescent,
    /// The record settled but the release gate still licensed nothing, so this
    /// agent freed no slice and must not be counted as having freed one.
    Declined,
}

/// Settle a run the controller has no record of and hand its slice back, in that
/// order: cores freed ahead of the record are cores a restart cannot re-charge.
async fn settle_and_release_run(
    allocation: &Arc<Mutex<NodeAllocation>>,
    admissions: &crate::admission::AdmissionStore,
    run: RunKey,
) -> std::io::Result<SettleRunOutcome> {
    let store = admissions.clone();
    let permit = tokio::task::spawn_blocking(move || store.settle_permit(run))
        .await
        .map_err(|error| std::io::Error::other(format!("settle permit task failed: {error}")))??;
    let step_id = match permit {
        crate::admission::SettlePermit::Due(step_id) => step_id,
        crate::admission::SettlePermit::NoRecord => return Ok(SettleRunOutcome::NoRecord),
        crate::admission::SettlePermit::NotQuiescent => return Ok(SettleRunOutcome::NotQuiescent),
    };
    let store = admissions.clone();
    let settled = tokio::task::spawn_blocking(move || store.settle_acknowledged_run(run))
        .await
        .map_err(|error| std::io::Error::other(format!("settle task failed: {error}")))??;
    if !settled {
        return Ok(SettleRunOutcome::NoRecord);
    }
    Ok(
        match release_acknowledged_allocation(allocation, admissions, run, step_id).await {
            ReleaseOutcome::Freed => SettleRunOutcome::Released,
            ReleaseOutcome::AlreadyFree => SettleRunOutcome::AlreadyFree,
            ReleaseOutcome::NotDue => SettleRunOutcome::Declined,
        },
    )
}

/// Settle a run the controller has taken the completion of, and free its slice.
/// The one path from an acknowledgement to a released core, so none goes unsaid.
pub(crate) async fn settle_acknowledged_completion(
    allocation: &Arc<Mutex<NodeAllocation>>,
    admissions: &crate::admission::AdmissionStore,
    run: RunKey,
    step_id: spur_core::step::StepId,
) -> bool {
    let _ = admissions.record_controller_ack(run, 1);
    let _ = admissions.record_report_acknowledged(run, step_id);
    // Settling a hook still in flight is the teardown's to do, never an
    // acknowledgement's: the controller cannot see whose hook is still running.
    let _ = admissions.mark_acknowledged_run_cleaned(run);
    release_acknowledged_allocation(allocation, admissions, run, step_id).await
        == ReleaseOutcome::Freed
}

/// What a run's record still owes once teardown finishes. Teardown runs ahead of
/// the hook, so a configured epilog is a debt the mark has to carry.
pub(crate) fn epilog_owed(hooks: &HooksConfig) -> crate::admission::EpilogOwed {
    match hooks.epilog {
        Some(_) => crate::admission::EpilogOwed::Yes,
        None => crate::admission::EpilogOwed::No,
    }
}

/// What this node declares at registration. One reading of the hook config, so
/// the controller's gate and the agent's own debt cannot disagree.
pub fn runs_job_epilog(hooks: &HooksConfig) -> bool {
    epilog_owed(hooks) == crate::admission::EpilogOwed::Yes
}

/// The step whose supervisor runs this run's epilog. A numbered step never runs
/// one, so only the step owning the job's lifetime can answer for it.
fn hook_owner_step(admitted: &crate::admission::AdmittedRun) -> Option<spur_core::step::StepId> {
    admitted
        .participants
        .iter()
        .find(|participant| {
            participant.supervisor.is_some() && !spur_core::step::is_user_step(participant.step_id)
        })
        .map(|participant| participant.step_id)
}

/// Whether nothing that could still run this run's epilog survived the restart.
/// A run that named no supervisor had the previous agent's own hook.
fn hook_owner_did_not_survive_restart(admitted: &crate::admission::AdmittedRun) -> bool {
    if admitted.recorded_supervisors().next().is_none() {
        return true;
    }
    matches!(recorded_supervisor_liveness(admitted), Liveness::Gone)
}

/// What a run still owes once the agent stops tracking it. Only proof that
/// nothing can run the hook cancels the debt; anything less holds the slice.
fn supervised_epilog_owed(
    admitted: &crate::admission::AdmittedRun,
    hooks: &HooksConfig,
) -> crate::admission::EpilogOwed {
    if hooks.epilog.is_none() {
        return crate::admission::EpilogOwed::No;
    }
    match recorded_supervisor_liveness(admitted) {
        Liveness::Gone => crate::admission::EpilogOwed::No,
        Liveness::Live | Liveness::CannotTell => crate::admission::EpilogOwed::Yes,
    }
}

/// What a cancelled run's teardown still owes. A record that cannot be read in
/// full never says no hook is owed; the owner-loss sweep ends that hold instead.
fn cancelled_epilog_owed(
    admissions: &crate::admission::AdmissionStore,
    hooks: &HooksConfig,
    run: RunKey,
) -> crate::admission::EpilogOwed {
    // Nothing can owe a hook this node does not run.
    if hooks.epilog.is_none() {
        return crate::admission::EpilogOwed::No;
    }
    let (job_id, run_attempt) = (run.job_id(), run.attempt().unwrap_or_default());
    match admissions.load_admitted(run) {
        Ok((admitted, rejected)) if rejected.is_empty() => supervised_epilog_owed(&admitted, hooks),
        Ok(_) => crate::admission::EpilogOwed::Yes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            crate::admission::EpilogOwed::No
        }
        Err(error) => {
            warn!(job_id, run_attempt, %error,
                "could not read a cancelled run's participants; holding its slice");
            crate::admission::EpilogOwed::Yes
        }
    }
}

/// Carry the hook a cancelled run's teardown still owes into the record the gate
/// reads. A run whose hook has already spoken keeps its answer.
fn hold_cancelled_run_for_epilog(
    admissions: &crate::admission::AdmissionStore,
    hooks: &HooksConfig,
    run: RunKey,
) -> std::io::Result<()> {
    if cancelled_epilog_owed(admissions, hooks, run) == crate::admission::EpilogOwed::No {
        return Ok(());
    }
    admissions.record_epilog_if_unstarted(run, crate::admission::HookState::Pending)?;
    Ok(())
}

/// Record how a run's epilog ended, from the step that owns the hook. A numbered
/// step reports `epilog_failed: false` for a hook it never ran.
fn record_run_epilog(
    admissions: &crate::admission::AdmissionStore,
    run: RunKey,
    step_id: spur_core::step::StepId,
    state: crate::admission::HookState,
) {
    if spur_core::step::is_user_step(step_id) {
        return;
    }
    let _ = admissions.record_epilog(run, state);
}

/// How long a hook may hold a slice before it is called out by name. An epilog
/// standing on the cores is correct, but an unbounded one must not be silent.
const EPILOG_HOLD_STUCK_AFTER: std::time::Duration = std::time::Duration::from_secs(900);

/// Whether this hold has gone on long enough to say so, restarting its clock when
/// it has. A hook has no deadline, so the only thing that can be periodic is this.
fn epilog_hold_is_worth_saying<K: std::hash::Hash + Eq>(
    held_since: &mut HashMap<K, std::time::Instant>,
    held: K,
) -> bool {
    let since = held_since
        .entry(held)
        .or_insert_with(std::time::Instant::now);
    if since.elapsed() < EPILOG_HOLD_STUCK_AFTER {
        return false;
    }
    *since = std::time::Instant::now();
    true
}

/// Mirror each in-flight epilog's outcome from the supervisor's own ledger into
/// the record the gate reads. The hook's owner writes it, so a cancel cannot.
async fn resolve_supervised_epilogs(
    store: &crate::stepd::StepdStore,
    admissions: &crate::admission::AdmissionStore,
    held_since: &mut HashMap<RunKey, std::time::Instant>,
) {
    let Ok(loaded) = admissions.load_all() else {
        return;
    };
    let mut still_held = HashSet::new();
    for admitted in loaded.runs {
        if !admitted.run.cleanup.epilog.is_in_flight() {
            continue;
        }
        let Some(run) = admitted.run.key() else {
            continue;
        };
        let (job_id, run_attempt) = (admitted.run.job_id, admitted.run.run_attempt);
        // No owner was ever named, so no ledger can answer for the hook. The
        // hold ends at the restart that proves the owner did not survive.
        let Some(step_id) = hook_owner_step(&admitted) else {
            still_held.insert(run);
            if epilog_hold_is_worth_saying(held_since, run) {
                warn!(
                    job_id,
                    run_attempt, "an epilog with no recorded owner is holding this run's slice"
                );
            }
            continue;
        };
        let state = match store.epilog_result(job_id, run_attempt, step_id) {
            Ok(Some(failed)) => epilog_outcome(failed),
            // Nothing the ledger can be read as; only proof the owner is gone
            // may settle it, never the wait itself.
            Ok(None) if recorded_supervisor_liveness(&admitted) == Liveness::Gone => {
                warn!(
                    job_id,
                    run_attempt, "settling an epilog whose supervisor is gone"
                );
                crate::admission::HookState::Unknown
            }
            Ok(None) => {
                still_held.insert(run);
                if epilog_hold_is_worth_saying(held_since, run) {
                    warn!(
                        job_id,
                        run_attempt, "an epilog is still holding this run's slice"
                    );
                }
                continue;
            }
            // A ledger that cannot be read is never proof the hook ended, but a
            // hold it takes must still end when nothing is left to end it.
            Err(error) if recorded_supervisor_liveness(&admitted) == Liveness::Gone => {
                warn!(job_id, run_attempt, %error,
                    "settling an unreadable epilog whose supervisor is gone");
                crate::admission::HookState::Unknown
            }
            Err(error) => {
                still_held.insert(run);
                if epilog_hold_is_worth_saying(held_since, run) {
                    warn!(job_id, run_attempt, %error, "could not read an epilog's outcome; holding");
                }
                continue;
            }
        };
        if let Err(error) = admissions.record_epilog(run, state) {
            warn!(job_id, run_attempt, %error, "failed to record an epilog's outcome");
        }
    }
    held_since.retain(|run, _| still_held.contains(run));
}

/// How a run's epilog ended, as the record spells it.
pub(crate) fn epilog_outcome(failed: bool) -> crate::admission::HookState {
    if failed {
        crate::admission::HookState::Failed
    } else {
        crate::admission::HookState::Succeeded
    }
}

/// Releases a launch reservation if the handler exits between reserve and
/// commit, including on future cancellation which no error path can catch.
/// Disarmed once the job is committed to the running set.
struct LaunchReservationGuard {
    allocation: Arc<Mutex<NodeAllocation>>,
    admissions: crate::admission::AdmissionStore,
    run: RunKey,
    armed: bool,
    spawned: bool,
}

impl LaunchReservationGuard {
    fn new(
        allocation: Arc<Mutex<NodeAllocation>>,
        admissions: crate::admission::AdmissionStore,
        run: RunKey,
    ) -> Self {
        Self {
            allocation,
            admissions,
            run,
            armed: true,
            spawned: false,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }

    /// Past this point something is running, so an aborted launch must keep the
    /// record: a live process with no ledger entry reads as free capacity.
    fn mark_spawned(&mut self) {
        self.spawned = true;
    }

    /// The workload was killed and reaped before the launch returned, so the
    /// record describes nothing again and must not outlive the call.
    fn mark_reaped(&mut self) {
        self.spawned = false;
    }
}

impl Drop for LaunchReservationGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let run = self.run;
        // Something is running against this reservation, so the record and the
        // slice both still describe it. Only the controller can end it now.
        if self.spawned {
            warn!(%run, "an aborted launch left a payload running; its slice stays held");
            return;
        }
        // Not forgetfulness: this is the process that made the reservation,
        // aborting before anything was spawned against it.
        if let Err(error) = self.admissions.remove_run(run) {
            warn!(%run, %error, "failed to drop the admission record of an aborted launch");
        }
        if let Ok(mut alloc) = self.allocation.try_lock() {
            alloc.release_job(ReleaseWarrant::never_spawned(run));
        } else if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let allocation = self.allocation.clone();
            handle.spawn(async move {
                allocation
                    .lock()
                    .await
                    .release_job(ReleaseWarrant::never_spawned(run));
            });
        } else {
            // Nothing sweeps this now, so it has to be visible.
            warn!(%run, "could not release an aborted launch's reservation; it stays held");
        }
    }
}

use spur_proto::controller_rpc_retryable;

const CONTROLLER_RPC_ATTEMPTS: u32 = 3;
const CONTROLLER_RPC_RETRY_GAP: std::time::Duration = std::time::Duration::from_secs(1);

/// The completion report is now the only thing that frees a slice, so the
/// obligation to deliver it is unbounded in attempts and bounded only in rate.
const COMPLETION_RETRY_CAP: std::time::Duration = std::time::Duration::from_secs(60);

/// How long a report may go unacknowledged before it is called out. A run that
/// holds its slice forever is correct but must not be silent.
const COMPLETION_STUCK_AFTER: std::time::Duration = std::time::Duration::from_secs(900);

/// Next backoff, capped. Jittered so a controller coming back does not meet
/// every node's retry at the same instant.
fn completion_backoff(attempt: u32, job_id: u32) -> std::time::Duration {
    let base = CONTROLLER_RPC_RETRY_GAP
        .saturating_mul(1u32 << attempt.min(6))
        .min(COMPLETION_RETRY_CAP);
    let jitter = std::time::Duration::from_millis(u64::from(job_id % 1000));
    (base + jitter).min(COMPLETION_RETRY_CAP + std::time::Duration::from_secs(1))
}

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

    /// Only the controller's own "no such job" settles a run. Everything else,
    /// including a rejection no retry can fix, leaves the slice held.
    #[test]
    fn only_a_definite_no_such_job_settles_a_completion() {
        use super::CompletionOutcome;
        assert!(
            CompletionOutcome::for_error(&ControllerRpcError::Rpc(Status::not_found(
                "job 5 not found"
            )))
            .settled()
        );
        for status in [
            Status::invalid_argument("node n1 is not allocated to job 5"),
            Status::unavailable("not the Raft leader"),
            Status::internal("raft propose failed"),
        ] {
            let outcome = CompletionOutcome::for_error(&ControllerRpcError::Rpc(status.clone()));
            assert!(
                !outcome.settled(),
                "{status:?} is not the controller saying it has no record"
            );
        }
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
    // An allocation owns no process, so try_wait never settles: polling it would
    // spin for the lifetime of the agent, pinning whatever the task captured.
    if job.is_allocation_only() {
        return;
    }
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
/// the script a supervised step runs.
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

/// What a process launched into a running job applies to itself between fork
/// and exec, for either spawn shape decided by [`build_launch_plan`].
struct ChildContainment<'a> {
    /// Both shapes join: on the nsenter shape the join lands before `nsenter`
    /// execs, so the namespaces it enters inherit the membership.
    cgroup: Option<&'a std::path::Path>,
    /// `None` on the nsenter shape, which drops inside the namespace via
    /// `setpriv` — dropping here as well would break the namespace entry.
    priv_drop: Option<crate::privdrop::PrivDrop>,
    /// Abort the exec if the cgroup join fails, rather than run the command
    /// outside the job's device filter and limits.
    cgroup_required: bool,
}

impl<'a> ChildContainment<'a> {
    fn for_plan(
        plan: &LaunchPlan,
        entry: &'a crate::job_entry::JobEntry,
        priv_drop: Option<crate::privdrop::PrivDrop>,
        cgroup_required: bool,
    ) -> Self {
        Self {
            cgroup: entry.cgroup_path.as_deref(),
            priv_drop: if plan.apply_priv_in_child {
                priv_drop
            } else {
                None
            },
            cgroup_required,
        }
    }

    /// Register the child's hook. Both inputs are built parent-side: nothing
    /// between fork and exec may allocate.
    fn register(self, cmd: &mut tokio::process::Command) {
        let cgroup_join = executor::CgroupJoin::for_cgroup(self.cgroup);
        let priv_drop = self.priv_drop;
        let required = self.cgroup_required;
        unsafe {
            cmd.pre_exec(move || {
                // Before the drop: an unprivileged process cannot write another cgroup's
                // `cgroup.procs`. Under `required` a failed join aborts rather than exec unfiltered.
                if let Some(ref join) = cgroup_join {
                    if !join.join() && required {
                        return Err(std::io::Error::other(
                            "[cgroup] required but the process did not join the job cgroup",
                        ));
                    }
                }
                if let Some(ref pd) = priv_drop {
                    pd.apply()
                        .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
                }
                Ok(())
            });
        }
    }
}

#[cfg(test)]
mod child_containment_tests {
    use super::{build_launch_plan, ChildContainment};
    use crate::privdrop::PrivDrop;

    const CGROUP: &str = "/sys/fs/cgroup/spur/job_7";

    fn entry(namespaced: bool, cgroup: Option<&str>) -> crate::job_entry::JobEntry {
        crate::job_entry::JobEntry {
            pid: if namespaced { 1234 } else { 0 },
            has_pid_namespace: namespaced,
            has_user_namespace: false,
            has_mount_namespace: namespaced,
            uid: 1000,
            gid: 1000,
            work_dir: "/home/user".into(),
            cgroup_path: cgroup.map(std::path::PathBuf::from),
        }
    }

    #[test]
    fn an_nsenter_launch_joins_the_cgroup_without_dropping_privilege_itself() {
        let entry = entry(true, Some(CGROUP));
        let pd = PrivDrop::for_test(1000, 1000);
        let plan = build_launch_plan(&entry, Some(&pd), &["id".to_string()]);
        assert!(!plan.apply_priv_in_child, "expected the nsenter shape");

        let containment = ChildContainment::for_plan(&plan, &entry, Some(pd), false);

        assert_eq!(containment.cgroup, Some(std::path::Path::new(CGROUP)));
        // setpriv drops inside the namespace; dropping here too would leave the
        // child unable to enter it.
        assert!(containment.priv_drop.is_none());
    }

    #[test]
    fn a_direct_launch_joins_the_cgroup_and_drops_privilege_itself() {
        let entry = entry(false, Some(CGROUP));
        let pd = PrivDrop::for_test(1000, 1000);
        let plan = build_launch_plan(&entry, Some(&pd), &["id".to_string()]);
        assert!(plan.apply_priv_in_child, "expected the direct-spawn shape");

        let containment = ChildContainment::for_plan(&plan, &entry, Some(pd), false);

        assert_eq!(containment.cgroup, Some(std::path::Path::new(CGROUP)));
        assert!(containment.priv_drop.is_some());
    }

    #[test]
    fn a_job_without_a_cgroup_leaves_the_child_nothing_to_join() {
        // A non-root agent creates no cgroup, and the launch still has to run.
        for namespaced in [true, false] {
            let entry = entry(namespaced, None);
            let pd = PrivDrop::for_test(1000, 1000);
            let plan = build_launch_plan(&entry, Some(&pd), &["id".to_string()]);

            assert!(ChildContainment::for_plan(&plan, &entry, Some(pd), false)
                .cgroup
                .is_none());
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

impl StepScriptCleanup {
    fn stage_in_rootfs(&self, rootfs: &std::path::Path, uid: u32, gid: u32) -> anyhow::Result<()> {
        for source in &self.paths {
            let relative = source
                .strip_prefix("/")
                .map_err(|_| anyhow::anyhow!("step script path must be absolute"))?;
            if relative
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
            {
                anyhow::bail!("step script path contains unsupported components");
            }
            let destination = rootfs.join(relative);
            let parent = destination
                .parent()
                .ok_or_else(|| anyhow::anyhow!("step script has no parent directory"))?;
            std::fs::create_dir_all(parent)?;
            let content = std::fs::read_to_string(source)?;
            crate::executor::write_job_scratch(&destination, &content, uid, gid)?;
        }
        Ok(())
    }
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

/// One node's account of a finished run. `step_id` is `None` when the report
/// speaks for the job rather than a single step.
pub(crate) struct CompletionReport<'a> {
    pub job_id: u32,
    pub exit_code: i32,
    pub signal: i32,
    pub run_attempt: u32,
    pub reporting_node: &'a str,
    pub drain: Option<&'a DrainRequest>,
    pub step_id: Option<spur_core::step::StepId>,
    pub payload: PayloadEvidence<'a>,
}

/// Carried on every report, so a path reporting a run over cannot be written
/// without answering for the processes it says have stopped.
pub(crate) enum PayloadEvidence<'a> {
    /// The agent spawned and reaped the payload itself, so none of it is left.
    Reaped,
    /// A `spurstepd` session owns the payload; its published identity decides.
    Supervised(&'a crate::stepd::StepdStore),
}

/// Read from the recorded `(pid, start_ticks)` identity, not cgroup population:
/// available where cgroups are not, and names only the process the report ends.
fn payload_still_executing(report: &CompletionReport<'_>) -> bool {
    let PayloadEvidence::Supervised(store) = report.payload else {
        return false;
    };
    // Every participant, not the reporting step alone: this report ends the
    // whole run, and a sibling's payload is as much of it as the owner's.
    let sessions = match store.published_sessions_for_run(report.job_id, report.run_attempt) {
        Ok(sessions) => sessions,
        Err(error) => {
            warn!(
                job_id = report.job_id,
                run_attempt = report.run_attempt,
                %error,
                "could not enumerate a run's sessions; withholding its completion report"
            );
            return true;
        }
    };
    for published in &sessions {
        let live = crate::stepd::workload_liveness(published);
        if live == crate::stepd::WorkloadLiveness::Gone {
            continue;
        }
        warn!(
            job_id = report.job_id,
            run_attempt = report.run_attempt,
            step_id = published.as_ref().map(|s| s.step_id).unwrap_or_default(),
            ?live,
            "this run's payload is still executing; withholding its completion report"
        );
        return true;
    }
    false
}

/// What the controller said when handed one node's account of a finished run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompletionOutcome {
    /// The controller committed the completion.
    Acknowledged,
    /// The controller answered that it has no record of the run.
    NoSuchRun,
    /// Nothing that settles the run; the report is still owed.
    Undelivered,
}

impl CompletionOutcome {
    /// A definite "no such job" is the controller's own word that it is not
    /// accounting for the run; anything else leaves the report owed.
    fn for_error(error: &ControllerRpcError) -> Self {
        match error {
            ControllerRpcError::Rpc(status) if status.code() == tonic::Code::NotFound => {
                Self::NoSuchRun
            }
            _ => Self::Undelivered,
        }
    }

    /// Whether the controller has settled this run, so its slice may go. A run
    /// the controller has no record of will never be acknowledged by anything.
    pub(crate) fn settled(self) -> bool {
        matches!(self, Self::Acknowledged | Self::NoSuchRun)
    }
}

pub(crate) async fn report_completion(
    controller_addr: &str,
    report: CompletionReport<'_>,
) -> CompletionOutcome {
    // Before the RPC, not after: an acknowledgement frees the slice, and a
    // payload still on the cores makes this report a lie the controller acts on.
    if payload_still_executing(&report) {
        return CompletionOutcome::Undelivered;
    }
    let CompletionReport {
        job_id,
        exit_code,
        signal,
        run_attempt,
        reporting_node,
        drain,
        step_id,
        payload: _,
    } = report;
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
            step_id,
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

    let Err(error) = result else {
        info!(
            job_id,
            exit_code,
            controller = %controller_addr,
            "reported completion to controller"
        );
        return CompletionOutcome::Acknowledged;
    };

    let outcome = CompletionOutcome::for_error(&error);
    if outcome == CompletionOutcome::NoSuchRun {
        warn!(
            job_id,
            exit_code,
            controller = %controller_addr,
            "controller has no record of this run; no acknowledgement will ever come for it"
        );
        return outcome;
    }
    // Deliberately not called non-retryable: every caller replays or holds
    // until the controller takes it, whatever the code says about one attempt.
    error!(
        job_id,
        exit_code,
        error = %error,
        "controller did not accept this completion; the report stays owed"
    );
    outcome
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

    async fn request_node_ledger(
        &self,
        request: Request<RequestNodeLedgerRequest>,
    ) -> Result<Response<RequestNodeLedgerResponse>, Status> {
        Self::require_controller(&request)?;
        let reason = request.into_inner().reason;
        let admissions = self.admissions();
        let session = self.reporter.agent_session_id().to_string();
        let cut = tokio::task::spawn_blocking(move || admissions.ledger_cut(&session))
            .await
            .map_err(|error| Status::internal(format!("ledger task failed: {error}")))?;
        info!(
            %reason,
            entries = cut.entries.len(),
            complete = cut.inventory_complete,
            "answered a ledger pull"
        );
        self.reporter.note_ledger_pulled();
        Ok(Response::new(RequestNodeLedgerResponse {
            ledger: Some(crate::reporter::ledger_to_proto(cut)),
        }))
    }

    async fn fence_run(
        &self,
        request: Request<FenceRunRequest>,
    ) -> Result<Response<FenceRunResponse>, Status> {
        Self::require_controller(&request)?;
        let req = request.into_inner();
        let admissions = self.admissions();
        let (job_id, run_attempt, cutoff) =
            (req.job_id, req.run_attempt, req.reject_before_unix_ms);
        let run = named_run(job_id, run_attempt)
            .ok_or_else(|| Status::invalid_argument("run attempt 0 names no run to fence"))?;
        let fenced = tokio::task::spawn_blocking(move || admissions.fence_run(run, cutoff))
            .await
            .map_err(|error| Status::internal(format!("fence task failed: {error}")))?;
        match fenced {
            Ok(reject_before_unix_ms) => {
                info!(
                    job_id,
                    run_attempt, reject_before_unix_ms, "fenced a run against in-flight launches"
                );
                Ok(Response::new(FenceRunResponse {
                    success: true,
                    error: String::new(),
                    reject_before_unix_ms,
                }))
            }
            Err(error) => Err(Status::unavailable(format!(
                "could not persist the fence for job {job_id}: {error}"
            ))),
        }
    }

    async fn settle_run(
        &self,
        request: Request<SettleRunRequest>,
    ) -> Result<Response<SettleRunResponse>, Status> {
        Self::require_controller(&request)?;
        let req = request.into_inner();
        let (job_id, run_attempt) = (req.job_id, req.run_attempt);
        let run = named_run(job_id, run_attempt)
            .ok_or_else(|| Status::invalid_argument("run attempt 0 names no run to settle"))?;
        // Held across the running check and the release under it, as teardown does,
        // so a re-dispatch either lands before this or finds nothing taken from it.
        let _lifecycle = self.lifecycle.acquire(job_id).await;
        // Declining costs a slice until the next reconcile; releasing one a
        // payload is still on hands its cores to a second job.
        if self
            .running
            .lock()
            .await
            .get(&job_id)
            .is_some_and(|tracked| tracked.run_attempt == run_attempt)
        {
            warn!(
                job_id,
                run_attempt, "declining to settle a run this agent is still running"
            );
            return Ok(Response::new(SettleRunResponse {
                released: false,
                error: "this agent is still running the run".into(),
            }));
        }
        let outcome = settle_and_release_run(&self.allocation, &self.admissions(), run)
            .await
            .map_err(|error| {
                Status::unavailable(format!(
                    "could not settle run {job_id}.{run_attempt}: {error}"
                ))
            })?;
        let declined = match outcome {
            SettleRunOutcome::Released => {
                info!(
                    job_id,
                    run_attempt,
                    "controller answered a claim it has no record of; released the slice"
                );
                return Ok(Response::new(SettleRunResponse {
                    released: true,
                    error: String::new(),
                }));
            }
            SettleRunOutcome::AlreadyFree => "the slice was already free",
            SettleRunOutcome::NoRecord => "this agent has no record of the run",
            SettleRunOutcome::NotQuiescent => "the run is still being torn down",
            SettleRunOutcome::Declined => "this agent's release gate licensed nothing",
        };
        warn!(
            job_id,
            run_attempt, declined, "declining to report a slice as released"
        );
        Ok(Response::new(SettleRunResponse {
            released: false,
            error: declined.into(),
        }))
    }

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
            return Ok(Response::new(LaunchJobResponse {
                success: false,
                error: format!(
                    "launch targeted node '{}' but this agent serves '{}'",
                    req.target_node, self.reporter.hostname
                ),
                stdout_path: String::new(),
                stderr_path: String::new(),
                failure_kind: LaunchFailureKind::LaunchFailureTargetMismatch as i32,
                conflict: None,
            }));
        }
        // Before anything is reserved or spawned. These refuse a stale command
        // from the controller; they authenticate nobody.
        let fences = crate::admission::LaunchFences {
            issued_at_unix_ms: req.issued_at_unix_ms,
            expires_at_unix_ms: req.expires_at_unix_ms,
            command_digest: req.command_digest.clone(),
        };
        let launch_step = launch_step_id(req.spec.as_ref().map(|spec| spec.pty).unwrap_or(false));
        if let Some(refusal) =
            self.check_launch_fences(job_id, req.run_attempt, launch_step, &fences)
        {
            warn!(
                job_id,
                run_attempt = req.run_attempt,
                ?refusal,
                "refusing a launch"
            );
            return Ok(Response::new(LaunchJobResponse {
                conflict: None,
                success: false,
                error: format!("launch refused: {refusal:?}"),
                stdout_path: String::new(),
                stderr_path: String::new(),
                failure_kind: match refusal {
                    crate::admission::LaunchRefusal::Fenced => {
                        LaunchFailureKind::LaunchFailureFenced as i32
                    }
                    crate::admission::LaunchRefusal::Expired => {
                        LaunchFailureKind::LaunchFailureExpired as i32
                    }
                    crate::admission::LaunchRefusal::ConflictingDigest => {
                        LaunchFailureKind::LaunchFailureConflictingDigest as i32
                    }
                },
            }));
        }

        let peer_nodes = req.peer_nodes;
        let task_offset = req.task_offset;
        // Per-task array identity is controller-assigned on the launch request,
        // not part of the (user-supplied) job spec.
        let array_job_id = req.array_job_id;
        let array_task_id = req.array_task_id;
        let run_attempt = req.run_attempt;
        let run_key = named_run(job_id, run_attempt)
            .ok_or_else(|| Status::invalid_argument("run attempt 0 names no run to launch"))?;
        let spec = req
            .spec
            .ok_or_else(|| Status::invalid_argument("missing job spec"))?;
        // Only the batch fallback reaches here with a pty; supervising it would put
        // the script's terminal in custody, where a reclaim hands it to any client.
        let stepd_enabled = !spec.pty;
        #[cfg(test)]
        let stepd_enabled = stepd_enabled && !self.force_legacy_launch;

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

        // Held until this run is tracked (or its half-built state is cleaned up), so a
        // re-dispatch of the id never builds on top of the previous run's teardown.
        let lifecycle = self.lifecycle.acquire(job_id).await;

        info!(
            job_id,
            name = %spec.name,
            task_offset,
            num_peers = peer_nodes.len(),
            "received job launch request"
        );

        let launch_step = launch_step_id(spec.pty);

        if stepd_enabled {
            let already_tracked =
                runtime_attempt_already_tracked(&self.stepds, job_id, launch_step, run_attempt)
                    .await;
            if already_tracked {
                // Idempotent retry: this exact attempt is already tracked and
                // alive on this node (e.g. spurctld retried after losing the
                // ack for a LaunchJob it had already delivered, since a
                // dispatch failure never advances run_attempt before the next
                // requeue). Report success without touching the live
                // allocation or session — allocate_local_resources below
                // would otherwise release and reallocate resources out from
                // under the still-running process before fencing ever runs.
                let paths = self
                    .running
                    .lock()
                    .await
                    .get(&job_id)
                    .filter(|tracked| tracked.run_attempt == run_attempt)
                    .map(|tracked| (tracked.stdout_path.clone(), tracked.stderr_path.clone()))
                    .unwrap_or_default();
                info!(
                    job_id,
                    run_attempt,
                    "stepd already tracked for this attempt; treating retried launch as success"
                );
                return Ok(Response::new(LaunchJobResponse {
                    conflict: None,
                    success: true,
                    error: String::new(),
                    stdout_path: paths.0,
                    stderr_path: paths.1,
                    failure_kind: LaunchFailureKind::LaunchFailureUnspecified as i32,
                }));
            }
        }

        // Left empty rather than defaulted to a flat, shared DEFAULT_WORK_DIR
        // here: `executor::launch_job` resolves an empty work_dir into a
        // per-job scratch directory once run_attempt is known, avoiding a
        // collision-prone shared anchor for relative output paths.
        let work_dir = spec.work_dir.clone();

        let allocation_holder =
            batch_script_uses_step_launch(&spec.script) && task_offset > 0 && !req.task_fanout;
        let script = if allocation_holder {
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

        // A supervised launch hosts its own server, so the agent only forwards
        // the plan: an agent restart must not take the rendezvous down with it.
        let mut pmix_guard = None;
        let mut supervised_pmix = None;
        let mut pmix_plan: Option<PmixLaunchPlan> = None;
        let mut pmix_per_local_rank_env: Option<Vec<HashMap<String, String>>> = None;
        if spec.mpi == MPI_PMIX && !batch_script_uses_step_launch(&spec.script) {
            let proto = req.pmix_plan.as_ref().ok_or_else(|| {
                Status::failed_precondition("missing PMIx launch plan for --mpi=pmix job")
            })?;
            if stepd_enabled {
                let mut plan =
                    mpi_plugin::plan_from_proto(proto).map_err(Status::failed_precondition)?;
                plan.rekey_to_step(launch_step);
                // Only this RPC can still answer the caller; once the supervisor
                // owns the launch a plugin it cannot load reads as a bare SIGKILL.
                self.mpi_host
                    .validate_pmix_dispatch(&plan)
                    .map_err(|error| {
                        warn!(job_id, %error, "refusing a pmix job this node cannot host");
                        Status::failed_precondition(error)
                    })?;
                supervised_pmix = Some(crate::stepd::StepdPmix {
                    plan,
                    config: self.mpi_host.config().clone(),
                    user_script_path: String::new(),
                    wrapper_path: String::new(),
                });
            } else {
                let (guard, plan, per_local_rank_env) =
                    start_pmix_launch(self.mpi_host.clone(), proto, task_offset, tasks_per_node)?;
                pmix_guard = Some(guard);
                pmix_plan = Some(plan);
                pmix_per_local_rank_env = per_local_rank_env;
            }
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
                    // The per-rank wrapper needs each rank's PMIx environment,
                    // which only the process hosting the server can hand out.
                    if let Some(pmix) = supervised_pmix.as_mut() {
                        pmix.user_script_path = user_script_path.clone();
                        launch_script
                    } else {
                        build_multi_task_pmix_wrapper(
                            &user_script_path,
                            tasks_per_node,
                            pmix_per_local_rank_env.as_ref().ok_or_else(|| {
                                Status::internal("missing PMIx per-rank env for multi-task launch")
                            })?,
                            Some(&spec.environment),
                        )
                        .map_err(Status::failed_precondition)?
                    }
                } else {
                    build_multi_task_wrapper(&user_script_path, tasks_per_node, None)
                }
            } else {
                launch_script
            };

        let (cpus, memory_mb) =
            resolve_cgroup_budget(req.allocated.as_ref(), &spec, tasks_per_node);

        let reserved = self
            .allocate_local_resources(
                job_id,
                run_attempt,
                &spec,
                req.allocated.as_ref(),
                cpus,
                memory_mb,
            )
            .await;
        let (alloc_result, allocated_device_ids) = match reserved {
            Ok(reserved) => reserved,
            // A transport error has nowhere to carry the holder, so the
            // controller's reconcile never fires on one.
            Err(LaunchRejection::Held(refusal)) => {
                return Ok(Response::new(refusal.into_response()))
            }
            Err(LaunchRejection::Failed(status)) => return Err(status),
        };

        // Release the reservation on any exit before commit, including a
        // cancelled launch future; disarmed once committed to `running`.
        let admissions = self.admissions();
        let mut reservation_guard =
            LaunchReservationGuard::new(self.allocation.clone(), admissions.clone(), run_key);

        // Durable before anything is spawned against it: a crash before this
        // leaves neither a record nor a process, a crash after leaves both.
        let mut run_record = crate::admission::RunAdmission::new(
            job_id,
            run_attempt,
            &self.reporter.hostname,
            crate::admission::AdmittedResources {
                cpu_ids: alloc_result.cpu_ids.clone(),
                memory_mb: alloc_result.memory_mb,
                gpu_devices: allocated_device_ids.clone(),
            },
            crate::admission::now_unix_ms(),
        );
        run_record.lifecycle_owner_step = Some(launch_step);
        if let Err(error) = admissions.admit_run_async(run_record.clone()).await {
            error!(job_id, run_attempt, %error, "failed to persist the run admission record");
            return Err(Status::unavailable(format!(
                "could not record the admission for job {job_id}: {error}"
            )));
        }
        let mut participant_record = crate::admission::ParticipantAdmission::new(
            job_id,
            run_attempt,
            launch_step,
            &self.reporter.hostname,
            run_record.allocation.clone(),
        );
        participant_record.final_report.required = true;
        participant_record.command_digest = req.command_digest.clone();
        participant_record.issued_at_unix_ms = req.issued_at_unix_ms;
        participant_record.expires_at_unix_ms = req.expires_at_unix_ms;
        if let Err(error) = admissions.admit_participant_async(participant_record).await {
            error!(job_id, run_attempt, %error, "failed to persist the participant admission record");
            return Err(Status::unavailable(format!(
                "could not record the admission for job {job_id}: {error}"
            )));
        }

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
                    conflict: None,
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
            step_id: launch_step,
            job_id,
            run_attempt,
            script: launch_script,
            work_dir: work_dir.clone(),
            name: spec.name.clone(),
            user: spec.user.clone(),
            node: req.target_node.clone(),
            array_job_id: (array_job_id != 0).then_some(array_job_id),
            array_task_id: (array_job_id != 0).then_some(array_task_id),
            environment: env,
            pmix_multi_task,
            joins_parent_namespaces: false,
            allocation_holder,
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
            mpi: spec.mpi.clone(),
            host_device_plan: Some(host_device_plan),
            memlock: self.limits.memlock,
            cgroup: self.cgroup.clone(),
            io_mode: if spec.pty {
                executor::LaunchIo::Pty(None)
            } else {
                executor::LaunchIo::File
            },
        };

        let launch_result = if stepd_enabled {
            fence_displaced_stepd(&self.stepds, job_id, launch_step, run_attempt)
                .await
                .map_err(|error| {
                    Status::unavailable(format!(
                        "failed to fence displaced stepd before launch: {error}"
                    ))
                })?;
            launch_stepd(
                &launch_cfg,
                run_attempt,
                &self.reporter.controller_addr,
                &self.reporter.hostname,
                &self.stepd_state_dir,
                StepdLaunchOptions {
                    step_id: launch_step,
                    allocation_only: false,
                    container_rootfs_mode: launch_cfg
                        .container
                        .as_ref()
                        .map(|_| rootfs_mode.clone()),
                    hooks: (*self.hooks).clone(),
                    plugstack_path: self.plugstack_path.clone(),
                    pmix: supervised_pmix,
                },
            )
            .await
            .map(|(result, descriptor)| (result, Some(descriptor)))
        } else {
            executor::launch_job(&launch_cfg, (*self.spank).as_ref())
                .await
                .map(|result| (result, None))
        };

        match launch_result {
            Ok((mut result, runtime_descriptor)) => {
                pmix_guard.as_mut().map(PmixLaunchGuard::disarm);
                reservation_guard.mark_spawned();
                if let Some(ref descriptor) = runtime_descriptor {
                    if let Err(error) = admissions.record_supervisor(
                        run_key,
                        launch_step,
                        crate::admission::SupervisorRef {
                            pid: descriptor.pid,
                            start_ticks: descriptor.process_start_ticks,
                            boot_id: descriptor.boot_id.clone(),
                        },
                    ) {
                        error!(job_id, run_attempt, %error,
                            "failed to record the supervisor identity; this run's slice is \
                             held until the agent restarts");
                    }
                }

                // Claim the stepd slot before committing anything
                // else. A concurrent LaunchJob for the same job (a retry
                // racing the tail of this slower, now-superseded launch) may
                // have already tracked a strictly newer attempt; if so, this
                // launch lost the race and must not clobber it or commit the
                // allocation it just (redundantly) reserved.
                if let Some(descriptor) = runtime_descriptor.clone() {
                    if let Err(descriptor) = claim_stepd_slot(&self.stepds, descriptor).await {
                        warn!(
                            job_id,
                            run_attempt,
                            "stepd superseded by a newer attempt before it could be tracked; aborting"
                        );
                        if let Err(e) = self.mpi_host.stop_pmix_job(job_id) {
                            warn!(job_id, error = %e, "PMIx stop failed after superseded stepd");
                        }
                        // This session never entered `stepds`, so the
                        // crash watchdog will never see it either — reap it here.
                        discard_stepd_session(&descriptor).await;
                        let _ = result.job.kill_signal(nix::sys::signal::Signal::SIGKILL);
                        tokio::spawn(reap_killed_job(result.job));
                        reservation_guard.mark_reaped();
                        return Ok(Response::new(LaunchJobResponse {
                            conflict: None,
                            success: false,
                            error: "stepd superseded by a newer attempt".into(),
                            stdout_path: String::new(),
                            stderr_path: String::new(),
                            failure_kind: LaunchFailureKind::LaunchFailureUnspecified as i32,
                        }));
                    }
                }

                let mut jobs = self.running.lock().await;
                // Commit the reservation: the job now has a tracked process, so
                // it is no longer exempt from reconcile. Take the running lock
                // first so a job is never briefly absent from BOTH `running` and
                // `launching` (which would let reconcile reclaim it).
                let committed = self.allocation.lock().await.commit_job(job_id, run_attempt);
                if committed {
                    reservation_guard.disarm();
                }

                // reconcile reclaimed the reservation mid-launch (launch exceeded
                // the TTL). Don't track a job with no backing allocation — kill,
                // reap, and clean up its cgroup/rootfs/spool (mirroring the
                // monitor loop's completion teardown, which never runs since the
                // job never enters `running`), then fail the launch.
                if !committed {
                    drop(jobs);
                    reservation_guard.mark_reaped();
                    warn!(
                        job_id,
                        "reservation reclaimed during launch; aborting to avoid running unbacked"
                    );
                    if let Err(e) = self.mpi_host.stop_pmix_job(job_id) {
                        warn!(job_id, error = %e, "PMIx stop failed after reclaimed reservation");
                    }
                    // `kill_signal` cannot reach a supervised workload, and the tracked
                    // session would otherwise be fenced into a completion for this failure.
                    if let Some(ref descriptor) = runtime_descriptor {
                        if claim_stepd(&self.stepds, descriptor).await {
                            discard_stepd_session(descriptor).await;
                        }
                    }
                    let _ = result.job.kill_signal(nix::sys::signal::Signal::SIGKILL);
                    let cgroup = result.cgroup_path.take();
                    let running = self.running.clone();
                    // The guard moves into the task: reaping can outlive this call, and
                    // the id must stay closed to a re-dispatch until the state is gone.
                    tokio::spawn(async move {
                        let _lifecycle = lifecycle;
                        reap_killed_job(result.job).await;
                        // rootfs, spool and the job_<id> cgroup all derive from
                        // job_id, so a re-dispatch of that id reuses them: tearing
                        // any of them down now would destroy the live run.
                        if !running.lock().await.contains_key(&job_id) {
                            crate::container::cleanup_rootfs(
                                &crate::container::job_rootfs_base(job_id),
                                &rootfs_mode,
                            );
                            crate::executor::cleanup_job_spool(job_id);
                            if let Some(ref cg) = cgroup {
                                crate::executor::cleanup_cgroup(cg);
                            }
                        }
                    });
                    return Ok(Response::new(LaunchJobResponse {
                        conflict: None,
                        success: false,
                        error: "reservation reclaimed during launch".into(),
                        stdout_path: String::new(),
                        stderr_path: String::new(),
                        failure_kind: LaunchFailureKind::LaunchFailureUnspecified as i32,
                    }));
                }

                info!(job_id, gpus = ?launch_cfg.gpu_devices, "job launched successfully");
                let namespaces = launch_namespaces(&launch_cfg, nix::unistd::geteuid().is_root());
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
                        has_pid_namespace: namespaces.pid,
                        has_user_namespace: namespaces.user,
                        has_mount_namespace: namespaces.mount,
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
                        cgroup_path: result.cgroup_path,
                    },
                );
                drop(jobs);
                // The gate stays shut until the controller has committed the
                // job Running and calls StartJob: a step launched from the
                // script's first line would otherwise ask about a job the
                // cluster still reports as Pending.
                // Already claimed into `stepds` above, before the
                // allocation/running commit; completion arrives by push
                // notification, not by polling.
                // Re-dispatch onto the same node reuses job_id and displaces an
                // older run. If its process ignored SIGTERM and outlived the
                // requeue, kill and reap it here — the monitor loop no longer
                // tracks it, so without this it would leak as an orphan/zombie.
                // Its cgroup is deliberately left alone: the path is derived
                // from job_id, so it is the cgroup the new run just joined.
                if let Some(old) = displaced {
                    if old.run_attempt < run_attempt {
                        let _ = old.job.kill_signal(nix::sys::signal::Signal::SIGKILL);
                        tokio::spawn(reap_killed_job(old.job));
                    }
                }
                Ok(Response::new(LaunchJobResponse {
                    conflict: None,
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
                    conflict: None,
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
        // A pre-flight, not a server: the ranks' server belongs to the
        // supervisor that will own them, and it does not exist yet.
        match self.mpi_host.validate_pmix_dispatch(&plan) {
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
        // PreparePmix only validates, so a prepare that never launched left no
        // server here to roll back. Kept on the wire for controller compatibility.
        let _ = request.into_inner().job_id;
        Ok(Response::new(ReleasePmixResponse {}))
    }

    async fn start_job(
        &self,
        request: Request<spur_proto::proto::AgentStartJobRequest>,
    ) -> Result<Response<()>, Status> {
        Self::require_controller(&request)?;
        let req = request.into_inner();
        let job_id = req.job_id;

        // Only the job's own supervisor waits at a gate; a numbered step is
        // released by the agent as it launches, since its job already runs.
        let gated: Vec<crate::stepd::StepdDescriptor> =
            stepds_for_job(&*self.stepds.lock().await, job_id)
                .into_iter()
                .filter(|descriptor| !spur_core::step::is_user_step(descriptor.step_id))
                .filter(|descriptor| {
                    req.run_attempt == 0 || descriptor.run_attempt == req.run_attempt
                })
                .collect();
        if gated.is_empty() {
            return Err(Status::not_found(format!(
                "job {job_id} has no supervisor waiting to start on this node"
            )));
        }

        for descriptor in &gated {
            if let Err(error) =
                crate::stepd::start_job(descriptor, uuid::Uuid::new_v4().to_string()).await
            {
                // Nothing ran, so tear the supervisor down rather than leave
                // the controller holding a job whose workload never starts.
                warn!(job_id, run_attempt = req.run_attempt, %error,
                    "failed to release the supervised job");
                release_stepd_tracking(
                    &self.running,
                    &self.allocation,
                    &self.stepds,
                    descriptor,
                    "supervised job could not be released",
                )
                .await;
                discard_stepd_session(descriptor).await;
                return Err(Status::internal(format!(
                    "failed to release the supervised job: {error}"
                )));
            }
        }
        Ok(Response::new(()))
    }

    async fn cancel_job(
        &self,
        request: Request<AgentCancelJobRequest>,
    ) -> Result<Response<()>, Status> {
        Self::require_controller(&request)?;
        let req = request.into_inner();
        let job_id = req.job_id;

        if req.signal > 0 {
            self.send_explicit_signal(job_id, req.run_attempt, req.signal)
                .await;
        } else {
            self.graceful_cancel(job_id, req.run_attempt).await;
        }

        // Excludes the launch of this id, which holds this same guard across its own
        // commit. Must stay below the signal paths above: they reach drop_tracked_job.
        let _lifecycle = self.lifecycle.acquire(job_id).await;
        let tracked_attempt = {
            let jobs = self.running.lock().await;
            jobs.get(&job_id).map(|tracked| tracked.run_attempt)
        };
        let nothing_tracked = tracked_attempt.is_none();

        // An allocation ends here rather than through the completion teardown,
        // so this is where its cgroup node has to go: the steps inside only
        // ever remove their own leaf. A cancel need not name an attempt, and
        // the zero it carries then names a path no job ever used, so fall back
        // to the attempt this node is running.
        let supervised_attempt = match (req.run_attempt, tracked_attempt) {
            (0, None) => stepds_for_job(&*self.stepds.lock().await, job_id)
                .first()
                .map(|descriptor| descriptor.run_attempt),
            _ => None,
        };
        let doomed_attempt = match req.run_attempt {
            // A reservation whose launch never reached the tracking map is named
            // only by the claim index, and an unnamed release cannot be gated.
            0 => match tracked_attempt.or(supervised_attempt) {
                named @ Some(_) => named,
                None => self.charged_attempt(job_id).await,
            },
            named => Some(named),
        };
        let supervised = !stepds_for_job(&*self.stepds.lock().await, job_id).is_empty();
        if let Some(attempt) = cgroup_to_reap_on_cancel(doomed_attempt, supervised) {
            crate::executor::cleanup_cgroup(&crate::executor::expected_cgroup_path(
                job_id, attempt,
            ));
        }

        if !pmix_teardown_is_due(doomed_attempt, tracked_attempt) {
            info!(
                job_id,
                doomed_attempt,
                tracked_attempt,
                "leaving PMIx alone; this job's namespaces belong to a live attempt"
            );
        } else if let Err(err) = self.mpi_host.stop_pmix_job(job_id) {
            warn!(job_id, error = %err, "PMIx teardown on cancel failed");
        }

        // Nothing tracked means the record is all that is left of the claim and
        // its slice just went back; a tracked one is still tearing down.
        if let Some(attempt) = doomed_attempt {
            if let Some(run) = named_run(job_id, attempt) {
                if nothing_tracked {
                    self.settle_cancelled_run(run).await;
                } else {
                    self.mark_controller_cancelled(run).await;
                }
            }
        }

        Ok(Response::new(()))
    }

    async fn cancel_step(
        &self,
        request: Request<CancelStepRequest>,
    ) -> Result<Response<()>, Status> {
        Self::require_controller(&request)?;
        let req = request.into_inner();
        // Step ids restart at 0 on a requeue, so a cancel from a superseded
        // attempt would otherwise signal the redispatch's step of the same id.
        if !self.runs_attempt(req.job_id, req.run_attempt).await {
            warn!(
                job_id = req.job_id,
                run_attempt = req.run_attempt,
                step_id = req.step_id,
                "dropping a step cancel for a superseded run"
            );
            return Ok(Response::new(()));
        }
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
                None => None,
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

    async fn probe_stepd(
        &self,
        request: Request<StepdProbeRequest>,
    ) -> Result<Response<StepdProbeResponse>, Status> {
        Self::require_controller(&request)?;
        let request = request.into_inner();
        let descriptor = self
            .stepds
            .lock()
            .await
            .get(&(request.job_id, request.step_id))
            .filter(|descriptor| descriptor.run_attempt == request.run_attempt)
            .cloned();
        let Some(descriptor) = descriptor else {
            return Ok(Response::new(StepdProbeResponse { active: false }));
        };
        // A supervisor that cannot be reached or does not answer in time reads as
        // inactive; the control request is bounded, so this cannot hold the RPC open.
        let active = crate::stepd::query_state(&descriptor, uuid::Uuid::new_v4().to_string())
            .await
            .map(|state| state.active)
            .unwrap_or(false);
        Ok(Response::new(StepdProbeResponse { active }))
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
        if plan.apply_priv_in_child {
            // Direct spawn only: the parent applies this before exec, in the
            // host's mount namespace, where a job's work_dir need not exist.
            cmd.current_dir(&entry.work_dir);
        }
        // env_clear so spurd's own environment (secrets included) never leaks in.
        cmd.env_clear();
        cmd.env(
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        );
        for (k, v) in Self::session_environ(&entry) {
            cmd.env(k, v);
        }
        ChildContainment::for_plan(&plan, &entry, priv_drop, self.cgroup.required)
            .register(&mut cmd);

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
    ///
    /// Controller-only: `uid` and `user` reach the tracked job straight from the wire, and the exec
    /// paths authorize against `user` while executing as `uid` — a caller setting both is anyone.
    async fn register_job_allocation(
        &self,
        request: Request<RegisterJobAllocationRequest>,
    ) -> Result<Response<RegisterJobAllocationResponse>, Status> {
        Self::require_controller(&request)?;
        let req = request.into_inner();
        if req.job_id == 0 {
            return Err(Status::invalid_argument("job_id is required"));
        }
        // Creates this id's cgroup, so it must not run while a prior run's teardown
        // still owns the directory.
        let _lifecycle = self.lifecycle.acquire(req.job_id).await;

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

        // Resolved before the running lock and the reservation: this is the one
        // step that awaits another lock, and failing here needs no release.
        let device_paths = if controller_gpu_ids.is_empty() {
            Vec::new()
        } else {
            let injection = self.device_registry.lock().await.build_job_injection_plans(
                "gpu",
                &controller_gpu_ids,
                req.uid,
                req.gid,
            );
            match injection {
                Ok((host, _)) => host.device_paths,
                Err(e) => {
                    error!(job_id = req.job_id, error = %e, "device registry resolution failed");
                    return Err(Status::failed_precondition(format!(
                        "device resolution failed: {}",
                        e
                    )));
                }
            }
        };

        // Hold the running lock across the duplicate check, reserve+commit, and
        // insert (running → allocation, as in commit) so the job is never
        // committed-but-absent-from-running, which the reclaim reads as stale.
        let alloc_run = named_run(req.job_id, req.run_attempt).ok_or_else(|| {
            Status::invalid_argument("run attempt 0 names no run to hold an allocation")
        })?;
        let mut jobs = self.running.lock().await;
        if jobs.contains_key(&req.job_id) {
            return Err(Status::already_exists(format!(
                "job {} already registered on this node",
                req.job_id
            )));
        }
        let alloc_result = {
            let mut alloc = self.allocation.lock().await;
            let result = alloc
                .allocate_for_job(
                    req.job_id,
                    req.run_attempt,
                    cpus,
                    memory_mb,
                    &controller_gpu_ids,
                )
                .map_err(|e| match e {
                    AllocError::GpusUnavailable => Status::resource_exhausted(
                        "controller-allocated GPUs unavailable on this node",
                    ),
                    AllocError::DuplicateJob => Status::already_exists(format!(
                        "job {} already registered on this node",
                        req.job_id
                    )),
                    AllocError::Superseded => Status::failed_precondition(format!(
                        "job {} was superseded by a newer attempt on this node",
                        req.job_id
                    )),
                    AllocError::CpusUnavailable => {
                        Status::resource_exhausted("allocated cores unavailable on this node")
                    }
                    AllocError::MemoryUnavailable => {
                        Status::resource_exhausted("allocated memory unavailable on this node")
                    }
                })?;
            result
        };
        // Releases the allocation on any exit that does not record the job,
        // including a cancelled future; disarmed once it reaches `running`.
        let admissions = self.admissions();
        let mut reservation_guard =
            LaunchReservationGuard::new(self.allocation.clone(), admissions.clone(), alloc_run);

        // An srun allocation holds a slice with no launch of its own, so it
        // needs the same record: the claim outlives whatever steps join it.
        let mut run_record = crate::admission::RunAdmission::new(
            req.job_id,
            req.run_attempt,
            &self.reporter.hostname,
            crate::admission::AdmittedResources {
                cpu_ids: alloc_result.cpu_ids.clone(),
                memory_mb: alloc_result.memory_mb,
                gpu_devices: controller_gpu_ids.clone(),
            },
            crate::admission::now_unix_ms(),
        );
        run_record.lifecycle_owner_step = Some(spur_core::step::STEP_EXTERN);
        if let Err(error) = admissions.admit_run_async(run_record.clone()).await {
            error!(job_id = req.job_id, %error, "failed to persist the run admission record");
            return Err(Status::unavailable(format!(
                "could not record the admission for job {}: {error}",
                req.job_id
            )));
        }
        // The allocation's own participant. Without it the run has no step that
        // can own a hook, and its teardown reads as owing nothing.
        let mut participant_record = crate::admission::ParticipantAdmission::new(
            req.job_id,
            req.run_attempt,
            spur_core::step::STEP_EXTERN,
            &self.reporter.hostname,
            run_record.allocation.clone(),
        );
        participant_record.final_report.required = true;
        if let Err(error) = admissions.admit_participant_async(participant_record).await {
            error!(job_id = req.job_id, %error, "failed to persist the participant admission record");
            return Err(Status::unavailable(format!(
                "could not record the admission for job {}: {error}",
                req.job_id
            )));
        }

        // This allocation launches nothing, so its cgroup has to be created
        // here or the first step arriving has none to join.
        let setup = executor::setup_cgroup(
            executor::CgroupScope {
                job_id: req.job_id,
                run_attempt: req.run_attempt,
                step_id: spur_core::step::STEP_EXTERN,
            },
            &self.cgroup,
            cpus,
            memory_mb,
            &alloc_result.cpu_ids,
            &device_paths,
        );
        let cgroup_path = match allocation_cgroup(setup, self.cgroup.required) {
            AllocationCgroup::Record(path) => path,
            AllocationCgroup::Degraded(reason) => {
                warn!(job_id = req.job_id, reason = %reason, "allocation registered without cgroup enforcement");
                None
            }
            AllocationCgroup::Refuse(reason) => {
                let (status, cgroup) =
                    refuse_allocation(req.job_id, reservation_guard, &mut jobs, &reason);
                // Guard first, then the cgroup: removing it blocks retrying rmdir.
                drop(jobs);
                drop(cgroup);
                return Err(status);
            }
        };
        drop(jobs);

        info!(
            job_id = req.job_id,
            cpus,
            memory_mb,
            gpus = ?controller_gpu_ids,
            "registered srun allocation"
        );

        #[cfg(test)]
        let supervise_allocation = !self.force_legacy_launch;
        #[cfg(not(test))]
        let supervise_allocation = true;
        let runtime_descriptor = if supervise_allocation {
            let config = executor::JobLaunchConfig {
                job_id: req.job_id,
                run_attempt: req.run_attempt,
                step_id: spur_core::step::STEP_EXTERN,
                joins_parent_namespaces: false,
                allocation_holder: false,
                script: String::new(),
                work_dir: req.work_dir.clone(),
                name: String::new(),
                user: req.user.clone(),
                node: self.reporter.hostname.clone(),
                array_job_id: None,
                array_task_id: None,
                environment: HashMap::new(),
                stdout_path: String::new(),
                stderr_path: String::new(),
                stdin_path: String::new(),
                cpus,
                memory_mb,
                gpu_devices: controller_gpu_ids.clone(),
                cpu_ids: alloc_result.cpu_ids.clone(),
                open_mode: None,
                uid: req.uid,
                gid: req.gid,
                container: None,
                prolog_script: None,
                partition: req.partition.clone(),
                nodelist: req.nodelist.clone(),
                mpi: req.mpi.clone(),
                host_device_plan: None,
                memlock: self.limits.memlock,
                cgroup: self.cgroup.clone(),
                io_mode: executor::LaunchIo::File,
                pmix_multi_task: false,
            };
            fence_displaced_stepd(
                &self.stepds,
                req.job_id,
                spur_core::step::STEP_EXTERN,
                req.run_attempt,
            )
            .await
            .map_err(|error| {
                Status::unavailable(format!(
                    "failed to fence displaced stepd before allocation launch: {error}"
                ))
            })?;
            let (_, mut descriptor) = launch_stepd(
                &config,
                req.run_attempt,
                &self.reporter.controller_addr,
                &self.reporter.hostname,
                &self.stepd_state_dir,
                StepdLaunchOptions {
                    step_id: spur_core::step::STEP_EXTERN,
                    allocation_only: true,
                    container_rootfs_mode: None,
                    hooks: (*self.hooks).clone(),
                    plugstack_path: self.plugstack_path.clone(),
                    pmix: None,
                },
            )
            .await
            .map_err(|error| {
                Status::unavailable(format!("failed to start allocation stepd: {error}"))
            })?;

            // A supervisor is live against this reservation now, so an abandoned
            // launch must leave the slice held rather than hand it back.
            reservation_guard.mark_spawned();

            // Names who answers for this run's hooks. Unrecorded, no ledger can
            // answer for them, so the gate holds until a restart settles it.
            if let Err(error) = admissions.record_supervisor(
                alloc_run,
                spur_core::step::STEP_EXTERN,
                crate::admission::SupervisorRef {
                    pid: descriptor.pid,
                    start_ticks: descriptor.process_start_ticks,
                    boot_id: descriptor.boot_id.clone(),
                },
            ) {
                error!(job_id = req.job_id, %error,
                    "failed to record the supervisor identity; this allocation's slice is \
                     held until the agent restarts");
            }

            // The allocation's cgroup is created here, not by the supervisor, so
            // record it or a stale session leaves its steps unreaped.
            if let Some(path) = cgroup_path.as_ref() {
                descriptor.cgroup_path = path.clone();
                let store = crate::stepd::StepdStore::new(&self.stepd_state_dir);
                if let Err(error) = store.publish(&descriptor) {
                    warn!(job_id = req.job_id, %error, "failed to record allocation cgroup in the runtime descriptor");
                }
            }

            // Claim the slot before committing anything else, mirroring
            // LaunchJob: a concurrent registration for a strictly newer
            // attempt may have already tracked its own session while we were
            // setting this one up.
            if let Err(descriptor) = claim_stepd_slot(&self.stepds, descriptor).await {
                warn!(
                    job_id = req.job_id,
                    run_attempt = req.run_attempt,
                    "stepd superseded by a newer attempt before it could be tracked; aborting"
                );
                discard_stepd_session(&descriptor).await;
                return Err(Status::failed_precondition(format!(
                    "job {} was superseded by a newer attempt on this node",
                    req.job_id
                )));
            }
            true
        } else {
            false
        };

        let mut jobs = self.running.lock().await;
        if jobs.contains_key(&req.job_id) {
            drop(jobs);
            if runtime_descriptor {
                let removed = self
                    .stepds
                    .lock()
                    .await
                    .remove(&(req.job_id, spur_core::step::STEP_EXTERN));
                if let Some(descriptor) = removed {
                    discard_stepd_session(&descriptor).await;
                }
            }
            return Err(Status::already_exists(format!(
                "job {} already registered on this node",
                req.job_id
            )));
        }
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
                // Matches the epoch the allocation table was keyed with, which
                // the attempt-0 refusal above has already made a real one.
                run_attempt: req.run_attempt,
                cgroup_path,
            },
        );
        // Commit under `running`, the order commit_job expects, so the job is
        // never committed while absent from the map a reclaim reads.
        let _ = self
            .allocation
            .lock()
            .await
            .commit_job(req.job_id, req.run_attempt);
        reservation_guard.disarm();
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
        let supervised = self.supervised_state(job_id).await;
        let supervised_pid = supervised.as_ref().map(|state| state.job_pid);
        // The supervisor creates the cgroup after spurd's launch returns, so it
        // is read live rather than from the descriptor written before the exec.
        let supervised_cgroup = supervised
            .as_ref()
            .map(|state| std::path::PathBuf::from(&state.cgroup_path))
            .filter(|path| !path.as_os_str().is_empty());
        let (gpu_devices, partition, cpus, memory_mb, nodelist, job_mpi, job_entry, job_attempt) = {
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
            // A supervised job is tracked without a pid of its own; its namespaces
            // belong to the process at the root of the job's cgroup.
            let pid = match tracked.job.pid() {
                Some(pid) => pid as i32,
                None => supervised_pid
                    .or_else(|| tracked.cgroup_path.as_deref().and_then(cgroup_root_pid))
                    .unwrap_or(0),
            };
            let entry = crate::job_entry::JobEntry {
                pid,
                has_pid_namespace: tracked.has_pid_namespace,
                has_user_namespace: tracked.has_user_namespace,
                has_mount_namespace: tracked.has_mount_namespace,
                uid: tracked.uid,
                gid: tracked.gid,
                work_dir: tracked.work_dir.clone(),
                cgroup_path: tracked
                    .cgroup_path
                    .clone()
                    .or_else(|| supervised_cgroup.clone()),
            };
            (
                tracked.gpu_devices.clone(),
                tracked.partition.clone(),
                tracked.cpus,
                tracked.memory_mb,
                nodelist,
                tracked.mpi.clone(),
                entry,
                tracked.run_attempt,
            )
        };

        let job_nodelist = nodelist;
        let step_nodelist = if req.nodelist.trim().is_empty() {
            job_nodelist.clone()
        } else {
            req.nodelist.clone()
        };
        let agent_hostname = self.reporter.hostname.clone();
        let node_names: Vec<&str> = step_nodelist.split(',').filter(|s| !s.is_empty()).collect();
        let num_nodes = node_names.len().max(1) as u32;
        let node_id = node_names
            .iter()
            .position(|n| *n == agent_hostname)
            .unwrap_or(0) as u32;
        let job_num_nodes = job_nodelist
            .split(',')
            .filter(|s| !s.is_empty())
            .count()
            .max(1) as u32;

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
        senv.set_with_slurm_twin("SPUR_NODELIST", &step_nodelist);
        senv.set_with_slurm_twin("SPUR_JOB_NODELIST", &job_nodelist);
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
        senv.set_with_slurm_twin("SPUR_JOB_NUM_NODES", job_num_nodes);
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
        // Decided here rather than at the dispatch chain below because who runs
        // the step also decides who hosts its PMIx server.
        let supervise_step =
            step_can_be_supervised(req.container.as_ref().map_or("", |c| c.image.as_str()));
        #[cfg(test)]
        let supervise_step = supervise_step && !self.force_legacy_launch;

        // A supervised step hosts its own server, so the agent only forwards the
        // plan: an agent restart must not take the rendezvous down with it.
        let mut pmix_step_guard = None;
        let mut supervised_pmix: Option<crate::stepd::StepdPmix> = None;
        let mut pmix_plan: Option<PmixLaunchPlan> = None;
        let mut pmix_per_local_rank_env: Option<Vec<HashMap<String, String>>> = None;
        if step_mpi {
            let proto = req
                .pmix_plan
                .as_ref()
                .ok_or_else(|| Status::invalid_argument("missing PMIx launch plan"))?;
            if supervise_step {
                let pmix = supervised_step_pmix(
                    proto,
                    self.mpi_host.config().clone(),
                    step_id,
                    num_tasks,
                )?;
                // Only this RPC can still answer the caller; once the supervisor
                // owns the launch a plugin it cannot load reads as a bare SIGKILL.
                self.mpi_host
                    .validate_pmix_dispatch(&pmix.plan)
                    .map_err(|error| {
                        warn!(job_id, step_id, %error, "refusing a pmix step this node cannot host");
                        Status::failed_precondition(error)
                    })?;
                supervised_pmix = Some(pmix);
            } else {
                let (guard, plan, per_local_rank_env) =
                    start_pmix_launch(self.mpi_host.clone(), proto, req.task_offset, num_tasks)?;
                pmix_step_guard = Some(guard);
                pmix_plan = Some(plan);
                pmix_per_local_rank_env = per_local_rank_env;
            }
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

            let (user_script_name, wrapper_name) = step_scratch_names(node_id, step_id);
            let user_script_path = step_dir.join(user_script_name);
            let user_script = build_one_shot_command_script(&req.command)?;
            crate::executor::write_job_scratch(&user_script_path, &user_script, req.uid, req.gid)
                .map_err(|e| Status::internal(format!("failed to write step script: {e}")))?;
            guard.paths.push(user_script_path.clone());

            let wrapper_path = step_dir.join(wrapper_name);
            let wrapper = if num_tasks > 1 {
                if step_mpi {
                    match supervised_pmix.as_mut() {
                        // Only the process hosting the server can hand out each
                        // rank's environment, so it writes this file itself.
                        Some(pmix) => {
                            pmix.user_script_path = user_script_path.to_string_lossy().into_owned();
                            pmix.wrapper_path = wrapper_path.to_string_lossy().into_owned();
                            None
                        }
                        None => Some(
                            build_multi_task_pmix_wrapper(
                                user_script_path.to_string_lossy().as_ref(),
                                num_tasks,
                                pmix_per_local_rank_env.as_ref().ok_or_else(|| {
                                    Status::internal(
                                        "missing PMIx per-rank env for multi-task step",
                                    )
                                })?,
                                Some(&req.environment),
                            )
                            .map_err(Status::failed_precondition)?,
                        ),
                    }
                } else {
                    Some(build_multi_task_wrapper(
                        user_script_path.to_string_lossy().as_ref(),
                        num_tasks,
                        Some(&req.environment),
                    ))
                }
            } else {
                Some(spur_core::task_launch::build_labeled_single_task_wrapper(
                    user_script_path.to_string_lossy().as_ref(),
                    req.task_offset,
                    Some(&req.environment),
                ))
            };
            if let Some(wrapper) = wrapper {
                crate::executor::write_job_scratch(&wrapper_path, &wrapper, req.uid, req.gid)
                    .map_err(|e| Status::internal(format!("failed to write step wrapper: {e}")))?;
            }
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
                nodelist: job_nodelist.clone(),
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
        if step_mpi && pmix_per_local_rank_env.is_none() && supervised_pmix.is_none() {
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

        let joins_parent_namespaces = job_entry.has_namespaces() && job_entry.pid > 0;
        let mut supervised_step_cfg = if supervise_step {
            let command: Vec<String> = std::iter::once(program.clone())
                .chain(program_args.iter().cloned())
                .collect();
            Some(executor::JobLaunchConfig {
                step_id,
                job_id,
                run_attempt: job_attempt,
                script: supervised_step_script(&job_entry, req.uid, req.gid, &command)?,
                joins_parent_namespaces,
                allocation_holder: false,
                work_dir: work_dir.clone(),
                name: format!("{job_id}.{step_id}"),
                user: env
                    .get("USER")
                    .or_else(|| env.get("LOGNAME"))
                    .cloned()
                    .unwrap_or_default(),
                node: self.reporter.hostname.clone(),
                array_job_id: None,
                array_task_id: None,
                environment: env.clone(),
                // The spool files already exist, so the supervisor must not
                // truncate them when it opens them again.
                open_mode: Some("append".into()),
                stdout_path: stdout_path.clone(),
                stderr_path: stderr_path.clone(),
                stdin_path: String::new(),
                cpus,
                memory_mb,
                gpu_devices: gpu_devices.clone(),
                cpu_ids: Vec::new(),
                uid: req.uid,
                gid: req.gid,
                container: None,
                prolog_script: None,
                partition: partition.clone(),
                nodelist: job_nodelist.clone(),
                mpi: job_mpi.clone(),
                host_device_plan: None,
                memlock,
                cgroup: self.cgroup.clone(),
                io_mode: executor::LaunchIo::File,
                pmix_multi_task: step_mpi && num_tasks > 1,
            })
        } else {
            None
        };

        // Dispatch paths, each wiring the step's stdio to the spool files above.
        // `None` means the step was cancelled before it ran.
        let maybe_status: Option<std::process::ExitStatus> = if let Some(step_cfg) =
            supervised_step_cfg.take()
        {
            // Supervised: the step gets its own spurstepd and outlives a restart
            // of this agent. Its script enters the parent's namespaces, if any,
            // so the supervisor itself stays outside them.
            self.run_supervised_step_to_spool(
                &step_cfg,
                supervised_pmix.take(),
                step_files,
                step_key,
            )
            .await?
        } else if job_entry.has_namespaces() && job_entry.pid > 0 {
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
            // Empty the environment first so spurd's own (secrets included) is not
            // inherited; the step's resolved environment is applied on top.
            cmd.env_clear();
            for (k, v) in env {
                cmd.env(k, v);
            }
            unsafe {
                cmd.pre_exec(move || {
                    crate::executor::apply_memlock(memlock);
                    Ok(())
                });
            }
            ChildContainment::for_plan(&plan, &job_entry, priv_drop, self.cgroup.required)
                .register(&mut cmd);
            run_tokio_step_to_spool(
                cmd,
                step_files,
                &self.active_steps,
                step_key,
                job_entry.cgroup_path.as_deref(),
                self.cgroup.required,
            )
            .await?
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

            // Multi-task and labeled commands use agent-generated wrappers. A
            // fresh container cannot see their host paths after pivot_root.
            if let Some(ref scripts) = _step_script_guard {
                scripts
                    .stage_in_rootfs(&rootfs, step_uid, step_gid)
                    .map_err(|e| {
                        Status::internal(format!("failed to stage step scripts in container: {e}"))
                    })?;
            }

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
                job_entry.cgroup_path.as_deref(),
                self.cgroup.required,
            )
            .await?;
            if child_pid != 0 {
                rootfs_guard.pid = Some(child_pid);
            }
            maybe_status
        } else {
            // Case 3: no container — plain host process. Route through the same
            // launch plan as the other arms so the child joins the job cgroup.
            let priv_drop = crate::privdrop::PrivDrop::resolve_if_needed(req.uid, req.gid);
            let full_command: Vec<String> = std::iter::once(program.clone())
                .chain(program_args.iter().cloned())
                .collect();
            let plan = build_launch_plan(&job_entry, priv_drop.as_ref(), &full_command);
            let mut cmd = tokio::process::Command::new(&plan.program);
            cmd.args(&plan.args).current_dir(&work_dir).process_group(0);
            // Empty the environment first so spurd's own (secrets included) is not
            // inherited; the step's resolved environment is applied on top.
            cmd.env_clear();
            for (k, v) in env {
                cmd.env(k, v);
            }
            unsafe {
                cmd.pre_exec(move || {
                    crate::executor::apply_memlock(memlock);
                    Ok(())
                });
            }
            ChildContainment::for_plan(&plan, &job_entry, priv_drop, self.cgroup.required)
                .register(&mut cmd);
            run_tokio_step_to_spool(
                cmd,
                step_files,
                &self.active_steps,
                step_key,
                job_entry.cgroup_path.as_deref(),
                self.cgroup.required,
            )
            .await?
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
                nodelist: job_nodelist,
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
        // reading the spool files back, which is what bounds it in memory.
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

    async fn await_step(
        &self,
        request: Request<AwaitStepRequest>,
    ) -> Result<Response<RunCommandResponse>, Status> {
        let identity = Self::verified_identity(&request).cloned();
        let req = request.into_inner();
        self.check_job_access(req.job_id, identity.as_ref(), &req.user, "await a step of")
            .await?;

        let store = crate::stepd::StepdStore::new(&self.stepd_state_dir);
        let tracked = self
            .stepds
            .lock()
            .await
            .get(&(req.job_id, req.step_id))
            .cloned()
            // Step ids restart at 0 on a requeue, so a supervisor from another
            // attempt is a different run of this step, not this caller's.
            .filter(|descriptor| req.run_attempt == 0 || descriptor.run_attempt == req.run_attempt);
        let Some(descriptor) = tracked else {
            // Untracked is not unknown: a step whose exit was consumed before
            // this caller re-attached is settled, and answering not_found here
            // is what turns a successful step into a synthesised failure. The
            // memo answers within this agent's life, the ledger across restarts.
            let recorded = match self
                .step_completions
                .settled(req.job_id, req.run_attempt, req.step_id)
                .await
            {
                Some(outcome) => Ok(Some((outcome.exit_code, outcome.signal))),
                None if req.run_attempt == 0 => store.recorded_step_exit(req.job_id, req.step_id),
                None => store.observed_exit(req.job_id, req.run_attempt, req.step_id),
            };
            let settled = recorded.unwrap_or_else(|error| {
                warn!(job_id = req.job_id, run_attempt = req.run_attempt,
                    step_id = req.step_id, %error,
                    "failed to read a settled step's recorded exit");
                None
            });
            let Some((exit_code, signal)) = settled else {
                return Err(Status::not_found("this node is not running that step"));
            };
            return Ok(Response::new(
                awaited_step_response(req.job_id, req.step_id, exit_code, signal).await,
            ));
        };

        // A supervisor that finished while nobody was listening recorded its exit
        // durably; the rendezvous is in memory and does not survive a restart.
        if let Ok(Some((exit_code, signal))) = durable_runtime_exit(&store, &descriptor) {
            return Ok(Response::new(
                awaited_step_response(req.job_id, req.step_id, exit_code, signal).await,
            ));
        }

        let waiter = self
            .step_completions
            .reregister(req.job_id, descriptor.run_attempt, req.step_id)
            .await
            .ok_or_else(|| Status::already_exists("that step already has a caller awaiting it"))?;

        match waiter.await {
            Ok(outcome) => Ok(Response::new(
                awaited_step_response(req.job_id, req.step_id, outcome.exit_code, outcome.signal)
                    .await,
            )),
            Err(_) => Err(Status::internal(
                "the step's supervisor exited without reporting a status",
            )),
        }
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
        // srun step stream live and terminate at step exit.
        if let Some(requested_step) = requested_step_of(&req) {
            let active_steps = self.active_steps.clone();
            let stepds = self.stepds.clone();
            let want_stderr = req.stream == "stderr";
            let step_id = requested_step;
            let start_offset = req.start_offset;
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
                // Resume where a reconnecting reader left off rather than
                // repeating output it has already printed.
                let mut offset: u64 = start_offset;
                // An offset past the end means this is not the file the reader
                // was reading, so replay it whole: reprinting output is
                // recoverable for the user, dropping it silently is not.
                if matches!(file.metadata().await, Ok(meta) if meta.len() < offset) {
                    offset = 0;
                }
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
                    // Adoption restores a supervised step to `stepds`, never to
                    // `active_steps`, so a tail that reconnects after a restart
                    // would read an adopted step as finished and drop the rest
                    // of its output.
                    let still_running = active_steps.lock().await.contains_key(&step_key)
                        || stepds.lock().await.contains_key(&step_key);
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
        // proxy, so they inherit their own job.state check.
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
        // An allocation has no job-level output, and a session adopted from a
        // descriptor written before this was recorded has none to give either.
        if file_path.is_empty() {
            return Err(Status::not_found(format!(
                "job {job_id} has no {} file on this node",
                if req.stream == "stderr" {
                    "stderr"
                } else {
                    "stdout"
                }
            )));
        }

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

        // A reserved id names the job's own supervisor, which this would then
        // fence and replace with a shell.
        if !spur_core::step::is_user_step(init.step_id) {
            return Err(Status::invalid_argument(format!(
                "step {} is reserved and cannot host a session",
                init.step_id
            )));
        }

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
        // A non-interactive client (no TTY) closes its input stream on stdin-EOF
        // while still reading output; an interactive one only closes it on hangup.
        let interactive = !init.non_interactive;

        // Same defense-in-depth gate as exec_in_job: the uid comes from the tracked job, but an
        // interactive PTY into a root job must obey allow_root_jobs too. Checked here because
        // nothing further down the supervised launch can read the agent's config.
        if let Err(msg) = crate::privdrop::check_root_execution_allowed(
            entry.uid,
            self.allow_root_jobs,
            self.spurd_is_root,
        ) {
            warn!(job_id = init.job_id, uid = entry.uid, "{msg}");
            return Err(Status::permission_denied(msg));
        }

        // Dispatch mirrors run_command's three cases. A parent job with live
        // namespaces (containerized sbatch/salloc) is entered via nsenter; a step
        // that requested its own image with no such parent builds a fresh
        // container; otherwise the command runs on the host.
        let step_image = init
            .container
            .as_ref()
            .map(|c| c.image.as_str())
            .unwrap_or("");
        let parent_has_namespaces = entry.has_namespaces() && entry.pid > 0;

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<InteractiveOutput, Status>>(64);

        if !parent_has_namespaces && !step_image.is_empty() {
            // Case 2: fresh container for this interactive PTY step.
            let container = init
                .container
                .as_ref()
                .expect("image is non-empty in this arm");
            let (master_fd, child_pid, rootfs_guard) = self
                .spawn_containerized_pty_step(
                    init.job_id,
                    init.step_id,
                    container,
                    &argv,
                    &entry,
                    winsize.as_ref(),
                )
                .await?;

            info!(
                job_id = init.job_id,
                child_pid,
                image = %step_image,
                "interactive container session started"
            );

            let active_steps = self.active_steps.clone();
            let step_key = (init.job_id, init.step_id);
            let wait_pid = nix::unistd::Pid::from_raw(child_pid);
            tokio::spawn(async move {
                // Hold the rootfs guard and the active-steps entry for the whole
                // session: both drop when the bridge returns (child exit, client
                // disconnect, or scancel), cleaning up the rootfs and the
                // cancellation registration.
                let _rootfs_guard = rootfs_guard;
                let _step_guard = ActiveStepGuard {
                    steps: active_steps,
                    key: step_key,
                };
                let wait_exit = async move {
                    tokio::task::spawn_blocking(move || waitpid_exit_code(wait_pid))
                        .await
                        .unwrap_or(128)
                };
                Self::run_pty_bridge(master_fd, wait_exit, child_pid, interactive, inbound, tx)
                    .await;
            });

            return Ok(Response::new(ReceiverStream::new(rx)));
        }

        if !step_image.is_empty() {
            // Case 1: the parent job already provides a container; the step joins
            // its namespaces via nsenter rather than building a new one. Leave a
            // trace rather than silently dropping the requested image.
            warn!(
                job_id = init.job_id,
                image = %step_image,
                "step --container-image ignored: joining the parent job's running container"
            );
        }

        // Left by an agent that predates supervised terminals: its shell has no
        // supervisor of its own, so the job's holds the master on its behalf.
        let legacy_custody_dir = stepds_for_job(&*self.stepds.lock().await, init.job_id)
            .into_iter()
            .find(|descriptor| !spur_core::step::is_user_step(descriptor.step_id))
            .and_then(|descriptor| descriptor.socket_path.parent().map(|dir| dir.to_path_buf()));
        let already_bridging = self.live_ptys.lock().await.contains(&init.job_id);
        let legacy = match legacy_custody_dir.as_deref() {
            Some(dir) if !already_bridging => match crate::stepd::reclaim_orphaned_pty(dir).await {
                Ok(found) => found,
                Err(error) => {
                    warn!(job_id = init.job_id, %error, "could not look for an orphaned terminal");
                    None
                }
            },
            _ => None,
        };

        // Only a terminal actually taken from the job's custody is released back
        // to it; a supervised one is its own supervisor's to hold.
        let mut release_to_legacy_custody = false;
        let terminal = match legacy {
            Some((session_id, master)) => {
                let pid = session_id as i32;
                release_to_legacy_custody = true;
                info!(job_id = init.job_id, pid, "resumed an orphaned terminal");
                SupervisedTerminal {
                    master,
                    pid,
                    exit: Box::pin(wait_for_reparented_exit(pid)),
                }
            }
            None => self
                .attach_terminal(&entry, &init, &argv, winsize)
                .await
                .inspect(|terminal| {
                    info!(
                        job_id = init.job_id,
                        step_id = init.step_id,
                        child_pid = terminal.pid,
                        overlap = init.overlap,
                        "interactive session started"
                    )
                })?,
        };

        self.live_ptys.lock().await.insert(init.job_id);
        let live_ptys = self.live_ptys.clone();
        let job_id = init.job_id;
        let child_pid = terminal.pid;
        let bridge = Self::run_pty_bridge(
            terminal.master,
            terminal.exit,
            child_pid,
            interactive,
            inbound,
            tx,
        );
        tokio::spawn(async move {
            bridge.await;
            live_ptys.lock().await.remove(&job_id);
            if let Some(dir) = legacy_custody_dir
                .as_deref()
                .filter(|_| release_to_legacy_custody)
            {
                if let Err(error) = crate::stepd::release_pty_master(dir, child_pid as u32).await {
                    warn!(job_id, child_pid, %error, "failed to release a closed terminal");
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    // -- Native cluster component control: drive this node's k0s systemd unit. --
    async fn start_cluster_component(
        &self,
        request: Request<StartClusterComponentRequest>,
    ) -> Result<Response<StartClusterComponentResponse>, Status> {
        let req = request.into_inner();
        let role = crate::cluster::ClusterRole::parse_role(&req.role)
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
        let config_path = self.reporter.wg_config_dir.join(format!("{iface}.conf"));
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
                // Peers in this node's persisted config are never this reconcile's to prune, even
                // when absent from the pushed k0s membership.
                let protected: std::collections::HashSet<String> =
                    match spur_net::wireguard::WgConfig::read_from(&config_path) {
                        Ok(c) => c.peers.into_iter().map(|p| p.public_key).collect(),
                        // An unreadable config protects nothing, so say so — silently pruning
                        // peers because the file moved or is unreadable is the bad outcome here.
                        Err(e) if config_path.exists() => {
                            tracing::warn!(path = %config_path.display(), error = %e, "cannot read persisted WireGuard config; its peers are not protected from the mesh prune");
                            Default::default()
                        }
                        Err(_) => Default::default(),
                    };
                // Reconcile: prune peers no longer in the membership, then add/update the desired peers.
                let current = spur_net::wireguard::list_peers(&iface).unwrap_or_default();
                let (added, pruned) = spur_net::mesh::reconcile_mesh(
                    &iface,
                    &self_mesh_ip,
                    &members,
                    &current,
                    &protected,
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
    /// Drops the tracked entry for `job_id` only if it's still `run_attempt` —
    /// a concurrent redispatch can retrack the same job_id under a newer
    /// attempt between the caller's peek and this call, and that entry must
    /// survive.
    async fn drop_tracked_job(&self, job_id: u32, run_attempt: u32) {
        // Both admission paths refuse attempt 0, so nothing production tracks
        // widens here; the equality check below is what scopes the settle.
        let run = RunKey::new(job_id, run_attempt).unwrap_or(RunKey::any_attempt(job_id));
        // The cgroup removal and the settle below both key off the id, so a launch
        // reusing it must not interleave with them.
        let _lifecycle = self.lifecycle.acquire(job_id).await;
        // Scoped so the guard is gone before `cgroup` is dropped below: removing
        // a cgroup SIGKILLs its stragglers and then blocks retrying rmdir.
        let (removed, cgroup) = {
            let mut jobs = self.running.lock().await;
            if jobs
                .get(&job_id)
                .is_some_and(|current| current.run_attempt == run_attempt)
            {
                let (tracked, cgroup) = remove_tracked_job(&mut jobs, job_id);
                (tracked.is_some(), cgroup)
            } else {
                (false, executor::CgroupGuard::new(None))
            }
        };
        drop(cgroup);
        if !removed {
            return;
        }
        // Tracking is gone, but the slice is the record's to hand back: a hook
        // still running under this run is still standing on the cores.
        if run.attempt().is_some() {
            self.settle_cancelled_run(run).await;
        }
        let stale_sessions = {
            let mut sessions = self.stepds.lock().await;
            let stale: Vec<_> = stepds_for_job(&sessions, job_id)
                .into_iter()
                .filter(|current| current.run_attempt == run_attempt)
                .collect();
            for descriptor in &stale {
                sessions.remove(&stepd_key(descriptor));
            }
            stale
        };
        for descriptor in stale_sessions {
            if let Err(error) = crate::stepd::record_resources_released(&descriptor) {
                warn!(job_id, %error, "failed to record runtime resource release");
            }
        }
        if let Err(e) = self.mpi_host.stop_pmix_job(job_id) {
            warn!(job_id, error = %e, "PMIx stop failed on job drop");
        }
    }

    /// Settle a cancelled run, as an acknowledged completion settles one. What
    /// teardown still owes is recorded first, so the gate below can refuse.
    async fn settle_cancelled_run(&self, run: RunKey) {
        let (job_id, run_attempt) = (run.job_id(), run.attempt().unwrap_or_default());
        let admissions = self.admissions();
        let hooks = self.hooks.clone();
        let recorded = tokio::task::spawn_blocking(move || {
            let epilog = cancelled_epilog_owed(&admissions, &hooks, run);
            admissions.mark_controller_cancelled(run)?;
            admissions.mark_run_cleaned(run, epilog)
        })
        .await;
        match recorded {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                warn!(job_id, run_attempt, %error, "failed to record a cancelled run's teardown")
            }
            Err(error) => warn!(job_id, run_attempt, %error, "cancel-record task failed"),
        }
        match settle_and_release_run(&self.allocation, &self.admissions(), run).await {
            Ok(SettleRunOutcome::NotQuiescent) => info!(
                job_id,
                run_attempt, "holding a cancelled run's slice until its epilog ends"
            ),
            // Absent under the lifecycle guard, which excludes the launch that
            // would write it: no record then means no payload and no hook.
            Ok(SettleRunOutcome::NoRecord) => {
                if self
                    .allocation
                    .lock()
                    .await
                    .release_job(ReleaseWarrant::never_spawned(run))
                {
                    info!(
                        job_id,
                        run_attempt, "released a reservation nothing was spawned against"
                    );
                }
            }
            Ok(_) => {}
            Err(error) => {
                warn!(job_id, run_attempt, %error, "failed to settle a cancelled run's record")
            }
        }
    }

    /// The attempt this node's claim index still charges for `job_id`. The only
    /// name a reservation whose launch never reached the tracking map still has.
    async fn charged_attempt(&self, job_id: u32) -> Option<u32> {
        self.allocation.lock().await.charged_attempt(job_id)
    }

    /// Note the controller's cancel on a run still being torn down, and the hook
    /// its teardown owes: settling here frees the slice under processes still exiting.
    async fn mark_controller_cancelled(&self, run: RunKey) {
        let (job_id, run_attempt) = (run.job_id(), run.attempt().unwrap_or_default());
        let admissions = self.admissions();
        let hooks = self.hooks.clone();
        let marked = tokio::task::spawn_blocking(move || -> std::io::Result<bool> {
            if !admissions.mark_controller_cancelled(run)? {
                return Ok(false);
            }
            hold_cancelled_run_for_epilog(&admissions, &hooks, run)?;
            Ok(true)
        })
        .await;
        match marked {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                warn!(job_id, run_attempt, %error, "failed to record a run's cancellation")
            }
            Err(error) => warn!(job_id, run_attempt, %error, "cancel-record task failed"),
        }
    }

    /// Record controller-allocated GPUs and reserve the local CPU/memory budget.
    /// `cpus`/`memory_mb` are the resolved grant, so the core IDs match it.
    async fn allocate_local_resources(
        &self,
        job_id: u32,
        run_attempt: u32,
        spec: &JobSpec,
        allocated: Option<&ResourceAllocations>,
        cpus: u32,
        memory_mb: u64,
    ) -> Result<(AllocationResult, Vec<u32>), LaunchRejection> {
        // A zero request reserves an empty cpuset on a node with nothing free,
        // which confirms as an allocation mismatch instead of refusing here.
        if cpus == 0 {
            warn!(job_id, "rejecting launch: it would reserve no cpu at all");
            return Err(Status::resource_exhausted(
                "a launch must reserve at least one cpu on this node",
            )
            .into());
        }

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
            ))
            .into());
        }

        // Hold running across the reclaim (running-then-allocation, as in commit)
        // so a concurrent commit can't make a live owner look stale.
        let running = self.running.lock().await;
        let live: std::collections::HashSet<u32> = running.keys().copied().collect();

        let mut alloc = self.allocation.lock().await;

        let result =
            match alloc.allocate_for_job(job_id, run_attempt, cpus, memory_mb, &controller_gpu_ids)
            {
                Ok(result) => result,
                Err(AllocError::GpusUnavailable) => {
                    // An owner absent from the live set is unaccounted for, not
                    // finished. Displacing it puts two jobs on one GPU.
                    let conflicting = alloc.conflicting_owners(&controller_gpu_ids);
                    let unaccounted: Vec<u32> = conflicting
                        .iter()
                        .copied()
                        .filter(|owner| !live.contains(owner))
                        .collect();
                    warn!(
                        job_id,
                        requested = ?controller_gpu_ids,
                        ?unaccounted,
                        already_allocated = ?alloc.allocated_gpu_ids(),
                        "refusing dispatch: the allocated GPUs are held on this node"
                    );
                    // The unaccounted owner first: a live one holding the same
                    // device is the controller's own doing, not the drift.
                    let holder = unaccounted.first().or(conflicting.first()).copied();
                    return Err(LaunchRejection::Held(held_refusal(
                        &alloc,
                        holder,
                        "allocated resources unavailable on this node",
                    )));
                }
                Err(AllocError::DuplicateJob) => {
                    // A launch is already in flight for this job id (reserved, not
                    // yet committed or released). This is a concurrent duplicate,
                    // not resource exhaustion.
                    warn!(
                        job_id,
                        "rejecting duplicate launch: a launch is already in flight for this job"
                    );
                    return Err(Status::already_exists(format!(
                        "job {job_id} already has a launch in flight on this node"
                    ))
                    .into());
                }
                Err(AllocError::Superseded) => {
                    // A newer attempt already reserved/committed this job id; this
                    // is a late or duplicate LaunchJob for an older attempt.
                    warn!(
                    job_id,
                    run_attempt,
                    "rejecting launch: superseded by a newer attempt already tracked on this node"
                );
                    return Err(Status::failed_precondition(format!(
                        "job {job_id} was superseded by a newer attempt on this node"
                    ))
                    .into());
                }
                // Reachable on dispatch now that a shortfall refuses rather than
                // serving a short list the caller reads as a full allocation.
                Err(AllocError::CpusUnavailable) => {
                    warn!(job_id, "rejecting launch: allocated cores unavailable");
                    let holder = unexplained_cpu_holder(&alloc, &live);
                    return Err(LaunchRejection::Held(held_refusal(
                        &alloc,
                        holder,
                        "allocated cores unavailable on this node",
                    )));
                }
                // Unattributed whatever the node holds: a conflict carries cores
                // and devices, and neither can say whose memory is in the way.
                Err(AllocError::MemoryUnavailable) => {
                    warn!(job_id, "rejecting launch: no room for the requested memory");
                    return Err(LaunchRejection::Held(HeldRefusal::unattributed(
                        "allocated memory unavailable on this node",
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
    ) -> Result<(AllocationResult, Vec<u32>), LaunchRejection> {
        let (cpus, memory_mb) = resolve_cgroup_budget(allocated, spec, 1);
        self.allocate_local_resources(job_id, 1, spec, allocated, cpus, memory_mb)
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

    /// Whether this node is running the named epoch of a job. Zero names no
    /// epoch in particular and matches whatever is here.
    async fn runs_attempt(&self, job_id: u32, run_attempt: u32) -> bool {
        if run_attempt == 0 {
            return true;
        }
        if stepds_for_job(&*self.stepds.lock().await, job_id)
            .iter()
            .any(|descriptor| descriptor.run_attempt == run_attempt)
        {
            return true;
        }
        let jobs = self.running.lock().await;
        jobs.get(&job_id)
            .is_some_and(|tracked| tracked.run_attempt == run_attempt)
    }

    /// Send a user-specified signal to a running job.
    async fn send_explicit_signal(&self, job_id: u32, run_attempt: u32, signal: i32) {
        // A signal naming an epoch this node no longer runs belongs to a
        // superseded run, and the processes here are the redispatch that
        // replaced it. Killing them is worse than dropping the signal.
        if !self.runs_attempt(job_id, run_attempt).await {
            warn!(
                job_id,
                run_attempt, signal, "dropping a signal for a superseded run"
            );
            return;
        }
        // A signal reaches every step the job holds; one failing must not
        // silently spare its siblings.
        let runtimes = stepds_for_attempt(&*self.stepds.lock().await, job_id, run_attempt);
        let supervised = supervisor_owns_teardown(&runtimes);
        for descriptor in runtimes {
            if let Err(error) = crate::stepd::signal_allocation(
                &descriptor,
                uuid::Uuid::new_v4().to_string(),
                signal,
            )
            .await
            {
                warn!(job_id, step_id = descriptor.step_id, %error,
                    "runtime signal request failed");
            }
        }
        // A step without a supervisor of its own still runs under the agent, so
        // a supervised sibling must not spare it.
        self.cancel_active_steps_for_job(job_id, signal).await;
        if supervised {
            return;
        }
        let allocation_only_attempt = {
            let jobs = self.running.lock().await;
            jobs.get(&job_id)
                .filter(|tracked| tracked.job.is_allocation_only())
                .map(|tracked| tracked.run_attempt)
        };
        if let Some(run_attempt) = allocation_only_attempt {
            self.drop_tracked_job(job_id, run_attempt).await;
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
        let sig = if resume {
            nix::sys::signal::Signal::SIGCONT
        } else {
            nix::sys::signal::Signal::SIGSTOP
        };
        info!(job_id, resume, "sending suspend/resume signal to job");

        // A supervised process belongs to its supervisor, so freeze the whole
        // tree through it rather than the agent's own (absent) handle.
        let runtimes = stepds_for_job(&*self.stepds.lock().await, job_id);
        let supervised = owns_job_processes(&runtimes);
        for descriptor in runtimes {
            if let Err(error) = crate::stepd::signal_allocation(
                &descriptor,
                uuid::Uuid::new_v4().to_string(),
                sig as i32,
            )
            .await
            {
                warn!(job_id, step_id = descriptor.step_id, %error,
                    "runtime suspend/resume request failed");
            }
        }
        if supervised {
            return;
        }

        let jobs = self.running.lock().await;
        let Some(tracked) = jobs.get(&job_id) else {
            return;
        };
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
    /// Steps with a supervisor are skipped: it runs its own ordered shutdown,
    /// and the escalation below would cut that short.
    async fn cancel_active_steps_for_job(&self, job_id: u32, signal: i32) {
        let supervised: Vec<spur_core::step::StepId> =
            stepds_for_job(&*self.stepds.lock().await, job_id)
                .iter()
                .map(|descriptor| descriptor.step_id)
                .collect();
        // Snapshot (key, pid, epoch) under the lock, then signal *outside* it:
        // signal_step_tree walks /proc, so holding active_steps across it would
        // block every concurrent run_command (and the ActiveStepGuard's try_lock
        // cleanup, which silently skips on contention).
        let targets: Vec<((u32, u32), u32, u64)> = {
            let mut steps = self.active_steps.lock().await;
            let mut targets = Vec::new();
            for (key, step) in steps.iter_mut() {
                if key.0 == job_id && !supervised.contains(&key.1) {
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

    async fn graceful_cancel(&self, job_id: u32, run_attempt: u32) {
        // A cancel naming an epoch this node no longer runs belongs to a superseded
        // run, and the processes here are the redispatch that replaced it.
        if !self.runs_attempt(job_id, run_attempt).await {
            warn!(
                job_id,
                run_attempt, "dropping a cancel for a superseded run"
            );
            return;
        }
        let runtimes = stepds_for_attempt(&*self.stepds.lock().await, job_id, run_attempt);
        let supervised = supervisor_owns_teardown(&runtimes);
        for descriptor in runtimes {
            match crate::stepd::shutdown_allocation(&descriptor, uuid::Uuid::new_v4().to_string())
                .await
            {
                Ok(()) => {
                    let stepds = self.stepds.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                        let still_current = stepds
                            .lock()
                            .await
                            .get(&stepd_key(&descriptor))
                            .is_some_and(|current| stepd_is_current(current, &descriptor));
                        if !still_current {
                            return;
                        }
                        info!(
                            job_id,
                            run_attempt = descriptor.run_attempt,
                            "runtime grace period expired, sending SIGKILL"
                        );
                        if let Err(error) = crate::stepd::signal_allocation(
                            &descriptor,
                            uuid::Uuid::new_v4().to_string(),
                            nix::sys::signal::Signal::SIGKILL as i32,
                        )
                        .await
                        {
                            warn!(job_id, run_attempt = descriptor.run_attempt, %error,
                                "failed to SIGKILL stepd after grace period");
                        }
                    });
                }
                Err(error) => {
                    warn!(job_id, step_id = descriptor.step_id, %error,
                        "runtime termination request failed");
                }
            }
        }
        // A step without a supervisor of its own still runs under the agent, so
        // a supervised sibling must not spare it.
        self.cancel_active_steps_for_job(job_id, nix::sys::signal::Signal::SIGTERM as i32)
            .await;
        if supervised {
            return;
        }
        let allocation_only_attempt = {
            let jobs = self.running.lock().await;
            jobs.get(&job_id)
                .filter(|tracked| tracked.job.is_allocation_only())
                .map(|tracked| tracked.run_attempt)
        };
        if let Some(run_attempt) = allocation_only_attempt {
            self.drop_tracked_job(job_id, run_attempt).await;
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
        let supervised = self.supervised_state(job_id).await;
        let supervised_pid = supervised.as_ref().map(|state| state.job_pid);
        // The supervisor creates the cgroup after spurd's launch returns, so it
        // is read live rather than from the descriptor written before the exec.
        let supervised_cgroup = supervised
            .as_ref()
            .map(|state| std::path::PathBuf::from(&state.cgroup_path))
            .filter(|path| !path.as_os_str().is_empty());
        let jobs = self.running.lock().await;
        let tracked = jobs
            .get(&job_id)
            .ok_or_else(|| Status::not_found(format!("job {} not running on this node", job_id)))?;

        // A supervised job is tracked without a pid of its own; its namespaces
        // belong to the process at the root of the job's cgroup.
        let pid = match tracked.job.pid() {
            Some(pid) => pid as i32,
            None => supervised_pid
                .or_else(|| tracked.cgroup_path.as_deref().and_then(cgroup_root_pid))
                .unwrap_or(0),
        };

        Ok(crate::job_entry::JobEntry {
            pid,
            has_pid_namespace: tracked.has_pid_namespace,
            has_user_namespace: tracked.has_user_namespace,
            has_mount_namespace: tracked.has_mount_namespace,
            uid: tracked.uid,
            gid: tracked.gid,
            work_dir: tracked.work_dir.clone(),
            cgroup_path: tracked
                .cgroup_path
                .clone()
                .or_else(|| supervised_cgroup.clone()),
        })
    }

    /// Bidirectional PTY bridge: reads master fd, forwards inbound messages
    /// (stdin, resize, signal), and drains remaining output after child exit.
    ///
    /// `interactive` sets what closing the client's input stream means: for an
    /// interactive client (TTY) it is a hangup (SIGHUP the step and stop); for a
    /// non-interactive one (script/pipe/redirect) it is stdin-EOF, so stop
    /// forwarding input but keep draining until the command exits.
    async fn run_pty_bridge<S, F>(
        master: std::os::fd::OwnedFd,
        wait_exit: F,
        child_pid: i32,
        interactive: bool,
        mut inbound: S,
        tx: tokio::sync::mpsc::Sender<Result<InteractiveOutput, Status>>,
    ) where
        S: tokio_stream::Stream<Item = Result<InteractiveInput, Status>> + Unpin + Send,
        F: std::future::Future<Output = i32> + Send,
    {
        use crate::pty::WindowSize as PtyWinSize;
        use std::os::fd::AsRawFd;
        use tokio::io::unix::AsyncFd;
        use tokio_stream::StreamExt;

        // Pinned so `wait_exit` can be polled in the select and, if the loop exits
        // first (EOF/disconnect), awaited once more to reap.
        tokio::pin!(wait_exit);

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
        // Cleared when the client stops sending input; the branch is then parked so
        // a non-interactive stdin-EOF doesn't spin the select.
        let mut input_open = true;
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

                item = inbound.next(), if input_open && !child_exited => {
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
                        Some(Err(_)) => {
                            // Broken input stream: the client is gone. Hang up.
                            let _ = crate::pty::signal_foreground(
                                master_raw, child_pid, libc::SIGHUP,
                            );
                            break;
                        }
                        None => {
                            if interactive {
                                // The terminal went away — hang the step up.
                                let _ = crate::pty::signal_foreground(
                                    master_raw, child_pid, libc::SIGHUP,
                                );
                                break;
                            }
                            // Non-interactive stdin-EOF: the client still wants the
                            // output. Stop forwarding input and keep draining until
                            // the command exits; a truly-gone client is caught by a
                            // failing tx.send below.
                            input_open = false;
                        }
                    }
                }

                code = &mut wait_exit, if !child_exited => {
                    exit_code = code;
                    child_exited = true;
                }
            }
        }

        if !child_exited {
            exit_code = (&mut wait_exit).await;
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

    /// Launch an interactive PTY step in a fresh container rootfs, using the
    /// controller-resolved effective image. Like `run_command`'s Case 2, but wires
    /// the child's stdio to a PTY slave and returns the master fd for
    /// [`Self::run_pty_bridge`]. The caller holds the returned rootfs guard for the
    /// session so the rootfs is torn down when it ends.
    async fn spawn_containerized_pty_step(
        &self,
        job_id: u32,
        step_id: u32,
        container: &ContainerSpec,
        argv: &[String],
        entry: &crate::job_entry::JobEntry,
        winsize: Option<&crate::pty::WindowSize>,
    ) -> Result<(std::os::fd::OwnedFd, i32, StepRootfsGuard), Status> {
        use std::os::fd::AsRawFd;

        let (gpu_devices, partition, nodelist) = {
            let jobs = self.running.lock().await;
            let tracked = jobs.get(&job_id).ok_or_else(|| {
                Status::not_found(format!("job {job_id} not running on this node"))
            })?;
            let nodelist = if tracked.nodelist.is_empty() {
                hostname::get()
                    .map(|h| h.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "localhost".into())
            } else {
                tracked.nodelist.clone()
            };
            (
                tracked.gpu_devices.clone(),
                tracked.partition.clone(),
                nodelist,
            )
        };

        // GPU device injection plan for the step's allocated GPUs (mirror Case 2).
        let (mut gpu_env, container_device_plan) = if gpu_devices.is_empty() {
            (HashMap::new(), None)
        } else {
            let (host_plan, container_plan) = self
                .device_registry
                .lock()
                .await
                .build_job_injection_plans("gpu", &gpu_devices, entry.uid, entry.gid)
                .map_err(|e| {
                    Status::failed_precondition(format!("GPU injection plan failed: {e}"))
                })?;
            (host_plan.env, Some(container_plan))
        };
        maybe_deny_gpu_env(&mut gpu_env, &gpu_devices);

        // User identity for the container shadow/home, resolved from the job's uid.
        let user = (entry.uid > 0)
            .then(|| {
                nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(entry.uid))
                    .ok()
                    .flatten()
            })
            .flatten();
        let username = user
            .as_ref()
            .map(|u| u.name.clone())
            .unwrap_or_else(|| "spur".to_string());
        let home_dir = user
            .as_ref()
            .map(|u| u.dir.to_string_lossy().to_string())
            .unwrap_or_else(|| format!("/home/{username}"));

        // Session env: job identity + GPU visibility + user identity, layered later
        // over the image's own config.Env.
        let mut senv = SpurEnv::new();
        senv.set_with_slurm_twin("SPUR_JOB_ID", job_id);
        senv.set_with_slurm_twin("SPUR_JOBID", job_id);
        senv.set_with_slurm_twin("SPUR_JOB_PARTITION", &partition);
        senv.set_with_slurm_twin("SPUR_NODELIST", &nodelist);
        senv.set_with_slurm_twin("SPUR_JOB_NODELIST", &nodelist);
        let mut env = senv.into_map();
        env.extend(gpu_env);
        env.entry("TERM".to_string())
            .or_insert_with(|| "xterm-256color".to_string());
        env.insert("HOME".to_string(), home_dir.clone());
        env.insert("USER".to_string(), username.clone());
        env.insert("LOGNAME".to_string(), username.clone());

        let mounts: Vec<crate::container::BindMount> = container
            .mounts
            .iter()
            .filter_map(|m| crate::container::parse_mount(m).ok())
            .collect();

        let container_cfg = crate::container::ContainerConfig {
            image: container.image.clone(),
            mounts,
            workdir: if !container.workdir.is_empty() {
                Some(container.workdir.clone())
            } else if !entry.work_dir.is_empty() {
                Some(entry.work_dir.clone())
            } else {
                None
            },
            name: (!container.name.is_empty()).then(|| container.name.clone()),
            readonly: container.readonly,
            mount_home: container.mount_home,
            remap_root: container.remap_root,
            gpu_devices: gpu_devices.clone(),
            environment: env.clone(),
            container_env: {
                let mut ce = container.env.clone();
                maybe_deny_gpu_env(&mut ce, &gpu_devices);
                ce
            },
            entrypoint: (!container.entrypoint.is_empty()).then(|| container.entrypoint.clone()),
            uid: entry.uid,
            gid: entry.gid,
            username,
            home_dir,
            device_plan: container_device_plan,
        };

        let image_path = crate::container::resolve_image(&container.image, None, Some(entry.uid))
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        let step_base = crate::container::step_rootfs_base(job_id, step_id);
        let (rootfs, rootfs_mode) =
            crate::container::setup_rootfs(&image_path, &step_base, container_cfg.name.as_deref())
                .map_err(|e| Status::internal(format!("step container setup failed: {e}")))?;
        let mut rootfs_guard = StepRootfsGuard {
            base: step_base,
            mode: rootfs_mode,
            pid: None,
        };

        // Interactive command: the image's default shell (empty argv) or the
        // requested command, exec'd in place so it owns the PTY.
        let cmdline = if argv.is_empty() {
            None
        } else {
            Some(
                shlex::try_join(argv.iter().map(String::as_str)).map_err(|e| {
                    Status::invalid_argument(format!("command is not shell-safe: {e}"))
                })?,
            )
        };
        let child_cmd = StepChildCommand::Shell {
            cmdline,
            entrypoint: container_cfg.entrypoint.clone(),
        };
        reject_nul_bytes(&child_cmd)?;

        let env_base = spur_net::oci::container_base_env(&rootfs, env);

        let (master, slave) = crate::pty::openpty_with_winsize(winsize)
            .map_err(|e| Status::internal(format!("openpty: {e}")))?;
        let raw_io = crate::executor::JobIoRaw::Pty {
            master: master.as_raw_fd(),
            slave: slave.as_raw_fd(),
        };

        let (ready_r, ready_w) =
            nix::unistd::pipe().map_err(|e| Status::internal(format!("pipe failed: {e}")))?;
        nix::fcntl::fcntl(
            &ready_r,
            nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
        )
        .ok();

        let cgroup_join = executor::CgroupJoin::for_cgroup(entry.cgroup_path.as_deref());
        let memlock = self.limits.memlock;

        match unsafe {
            nix::unistd::fork().map_err(|e| Status::internal(format!("fork failed: {e}")))?
        } {
            nix::unistd::ForkResult::Child => {
                // === CHILD PROCESS — synchronous only ===
                drop(ready_r);
                unsafe {
                    libc::signal(libc::SIGCHLD, libc::SIG_DFL);
                    libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                    // Wire the PTY slave as the controlling terminal (setsid +
                    // TIOCSCTTY + dup2 slave->0/1/2 + close master) before
                    // container_init pivots into the rootfs.
                    if raw_io.wire().is_err() {
                        std::process::exit(127);
                    }
                }
                container_child_exec(
                    &container_cfg,
                    &rootfs,
                    ready_w,
                    memlock,
                    &cgroup_join,
                    env_base,
                    child_cmd,
                );
            }
            nix::unistd::ForkResult::Parent { child: child_pid } => {
                // === PARENT ===
                drop(ready_w);
                drop(slave);

                container_parent_ready(
                    child_pid,
                    ready_r,
                    self.cgroup.required,
                    entry.cgroup_path.as_deref(),
                )?;

                // Non-blocking so the bridge's AsyncFd reads/writes are correct.
                nix::fcntl::fcntl(
                    &master,
                    nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
                )
                .map_err(|e| Status::internal(format!("fcntl O_NONBLOCK: {e}")))?;

                let raw_pid = child_pid.as_raw();
                rootfs_guard.pid = Some(raw_pid);

                // Register so scancel and the allocation-cancel path can signal it.
                self.active_steps.lock().await.insert(
                    (job_id, step_id),
                    ActiveStep {
                        epoch: next_step_epoch(),
                        pid: Some(raw_pid as u32),
                        ..Default::default()
                    },
                );

                Ok((master, raw_pid, rootfs_guard))
            }
        }
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

    /// The environment a session that *enters* a running job starts from: the
    /// job's own, never spurd's. For a container job the tracked pid is the
    /// shepherd (a spurd fork carrying spurd's env), so read the container's
    /// workload (PID 1) instead. Callers must `env_clear()` first.
    fn session_environ(entry: &crate::job_entry::JobEntry) -> Vec<(String, String)> {
        if entry.pid <= 0 {
            return Vec::new();
        }
        let target = if entry.has_namespaces() {
            Self::container_workload_pid(entry.pid as u32).unwrap_or(entry.pid as u32)
        } else {
            entry.pid as u32
        };
        Self::read_proc_environ(target)
    }

    /// Resolve a container's workload pid (PID 1 in its namespace) from the
    /// shepherd pid: the shepherd forks exactly one child to become the namespace
    /// init, so its sole entry in `children` is that workload.
    fn container_workload_pid(shepherd: u32) -> Option<u32> {
        let content =
            std::fs::read_to_string(format!("/proc/{shepherd}/task/{shepherd}/children")).ok()?;
        content
            .split_whitespace()
            .next()
            .and_then(|s| s.parse().ok())
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
            job: executor::RunningJob::Managed { child },
            ..Self::allocation_only(None)
        }
    }

    /// An allocation with no batch process, as salloc and standalone srun
    /// register it.
    fn allocation_only(cgroup_path: Option<std::path::PathBuf>) -> Self {
        Self {
            job: executor::RunningJob::AllocationOnly,
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
            cgroup_path,
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
    use std::collections::HashSet;
    use tonic::Request;

    #[test]
    fn spawn_stepd_process_reports_a_live_setsid_grandchild() {
        let marker = tempfile::NamedTempFile::new().expect("marker file");
        let marker_path = marker.path().to_path_buf();
        let executable = std::path::PathBuf::from("/bin/sh");
        let args = vec![
            std::path::PathBuf::from("-c"),
            std::path::PathBuf::from(format!("echo ready > {}; sleep 5", marker_path.display())),
        ];

        let pid = spawn_stepd_process(&executable, &args).expect("spawn detached process");

        for _ in 0..50 {
            if std::fs::read_to_string(&marker_path).is_ok_and(|s| !s.is_empty()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None),
            Ok(()),
            "reported pid must be a live process"
        );
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("proc stat");
        let (_, fields) = stat.rsplit_once(") ").expect("stat format");
        let sid: i32 = fields
            .split_ascii_whitespace()
            .nth(3)
            .expect("session id field")
            .parse()
            .expect("session id parses");
        assert_eq!(
            sid, pid as i32,
            "the spawned process must be its own session leader"
        );

        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
    }

    #[test]
    fn spawn_stepd_process_fails_fast_on_a_missing_executable() {
        let missing = std::path::PathBuf::from("/nonexistent/spur-test-missing-binary");
        let args: Vec<std::path::PathBuf> = Vec::new();

        let started = std::time::Instant::now();
        let error = spawn_stepd_process(&missing, &args)
            .expect_err("a missing exec target must be reported as an error");

        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "must fail fast via the exec-error pipe, not the full readiness timeout"
        );
        assert!(error.to_string().contains("failed to exec"));
    }

    fn descriptor_for_workload(pid: u32) -> crate::stepd::StepdDescriptor {
        let mut descriptor = crate::stepd::StepdDescriptor::new(
            91,
            1,
            spur_core::step::STEP_BATCH,
            0,
            0,
            std::path::PathBuf::from("/tmp/unused.sock"),
            std::path::PathBuf::new(),
        );
        descriptor.workload_pid = pid;
        descriptor.workload_start_ticks =
            crate::stepd::process_start_ticks(pid).unwrap_or_default();
        descriptor
    }

    #[tokio::test]
    async fn fencing_without_a_cgroup_kills_a_workload_that_outlived_its_supervisor() {
        // Stopping the supervisor says nothing about the workload: it is a
        // separate process, and the signal never reached it.
        let mut child = std::process::Command::new("sleep")
            .arg("300")
            .spawn()
            .expect("spawn a stand-in workload");
        let descriptor = descriptor_for_workload(child.id());

        let confirmed = runtime_teardown_confirmed(&descriptor, &Ok(())).await;

        assert!(confirmed, "teardown must confirm once the workload is gone");
        assert!(
            !crate::stepd::process_is_live(
                descriptor.workload_pid,
                descriptor.workload_start_ticks
            ),
            "the workload must actually be dead, not merely reported so"
        );
        let _ = child.wait();
    }

    #[tokio::test]
    async fn fencing_without_a_cgroup_accepts_a_supervisor_that_held_no_workload() {
        // An allocation runs nothing of its own, and a descriptor written
        // before the workload identity existed cannot be improved on here.
        let descriptor = descriptor_for_workload(0);

        assert!(runtime_teardown_confirmed(&descriptor, &Ok(())).await);
    }

    #[tokio::test]
    async fn fencing_without_a_cgroup_does_not_kill_a_recycled_pid() {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn a short-lived process");
        let mut descriptor = descriptor_for_workload(child.id());
        let _ = child.wait();
        // Whoever holds this pid next is not our workload.
        descriptor.workload_start_ticks = descriptor.workload_start_ticks.wrapping_add(1);

        assert!(runtime_teardown_confirmed(&descriptor, &Ok(())).await);
    }

    #[tokio::test]
    async fn stop_stepd_process_is_a_noop_against_a_genuinely_stale_pid() {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn short-lived process");
        let pid = child.id();
        let start_ticks =
            crate::stepd::process_start_ticks(pid).expect("read start ticks before it exits");
        child
            .wait()
            .expect("reap the process so its pid is free to be stale");

        let descriptor = crate::stepd::StepdDescriptor::new(
            1,
            1,
            spur_core::step::STEP_BATCH,
            pid,
            start_ticks,
            std::path::PathBuf::from("/tmp/runtime.sock"),
            std::path::PathBuf::new(),
        );

        assert_eq!(
            crate::stepd::stepd_liveness(&descriptor).expect("liveness check"),
            crate::stepd::StepdLiveness::Stale,
            "an exited pid with its old start-ticks recorded must read as stale"
        );
        assert!(stop_stepd_process(&descriptor).await.is_ok());
    }

    #[test]
    fn unstarted_runtime_cleanup_removes_only_the_failed_attempt() {
        let state = tempfile::tempdir().expect("runtime state directory");
        let store = crate::stepd::StepdStore::new(state.path());
        let failed = store
            .prepare_session_dir(42, 7, spur_core::step::STEP_BATCH)
            .expect("failed attempt directory");
        let retained = store
            .prepare_session_dir(42, 8, spur_core::step::STEP_BATCH)
            .expect("retained attempt directory");

        cleanup_unstarted_stepd(&store, 42, 7, spur_core::step::STEP_BATCH);

        assert!(!failed.exists());
        assert!(retained.exists());
    }

    // Nothing else reaches an interactive shell: it has no `active_steps` entry,
    // and `cancel_active_steps_for_job` skips supervised steps by design.
    #[tokio::test]
    async fn a_cancel_reaches_a_terminal_tracked_the_way_its_launch_tracks_it() {
        let terminal_step = 7;
        let sessions: Arc<Mutex<StepdMap>> = Arc::new(Mutex::new(HashMap::new()));
        for step_id in [spur_core::step::STEP_BATCH, terminal_step] {
            claim_stepd_slot(
                &sessions,
                crate::stepd::StepdDescriptor::new(
                    42,
                    3,
                    step_id,
                    0,
                    0,
                    std::path::PathBuf::from("/tmp/runtime.sock"),
                    std::path::PathBuf::new(),
                ),
            )
            .await
            .expect("a terminal must claim its slot");
        }

        let reached: Vec<u32> = stepds_for_attempt(&*sessions.lock().await, 42, 3)
            .into_iter()
            .map(|descriptor| descriptor.step_id)
            .collect();
        assert!(
            reached.contains(&terminal_step),
            "a cancel must reach the terminal: {reached:?}"
        );
        assert!(stepds_for_attempt(&*sessions.lock().await, 42, 4).is_empty());
    }

    // A second client must be refused rather than handed the same master: two
    // bridges on one terminal interleave the user's keystrokes.
    #[tokio::test]
    async fn a_second_client_cannot_take_over_a_terminal_that_still_has_one() {
        let completions = crate::step_completion::StepCompletions::new();
        let first = completions
            .reregister(42, 3, 7)
            .await
            .expect("the first client takes the terminal");

        assert!(
            completions.reregister(42, 3, 7).await.is_none(),
            "a second client must not displace the first"
        );

        drop(first);
        assert!(
            completions.reregister(42, 3, 7).await.is_some(),
            "the terminal is free again once its client is gone"
        );
    }

    // An unreadable supervisor is not a dead one. Treating it as dead makes the
    // fresh launch fence it, killing the shell the client asked to resume.
    #[test]
    fn only_a_supervisor_confirmed_dead_is_launched_over() {
        assert!(confirmed_gone(Ok(crate::stepd::StepdLiveness::Stale)));
        assert!(!confirmed_gone(Ok(crate::stepd::StepdLiveness::Live)));
        assert!(!confirmed_gone(Err(std::io::Error::other("unreadable"))));
    }

    #[test]
    fn a_live_supervisor_reads_as_live_through_the_descriptor() {
        let live = crate::stepd::StepdDescriptor::new(
            42,
            3,
            7,
            std::process::id(),
            crate::stepd::process_start_ticks(std::process::id()).unwrap_or(0),
            std::path::PathBuf::from("/tmp/runtime.sock"),
            std::path::PathBuf::new(),
        );
        assert!(!terminal_supervisor_is_gone(&live));
    }

    #[test]
    fn runtime_displacement_requires_a_strictly_newer_attempt() {
        let displaced = crate::stepd::StepdDescriptor::new(
            42,
            7,
            spur_core::step::STEP_BATCH,
            0,
            0,
            std::path::PathBuf::from("/tmp/runtime.sock"),
            std::path::PathBuf::new(),
        );

        assert_eq!(displaced_runtime_attempt(&displaced, 8).unwrap(), 7);
        // A same-attempt retry (e.g. spurctld re-dispatching after losing the
        // ack for a LaunchJob it already delivered) is idempotent, not a
        // displacement: it must not fence/stop the still-current attempt.
        assert_eq!(displaced_runtime_attempt(&displaced, 7).unwrap(), 7);
        assert_eq!(
            displaced_runtime_attempt(&displaced, 6)
                .expect_err("an older attempt must not replace a newer one")
                .kind(),
            std::io::ErrorKind::AlreadyExists
        );
    }

    #[tokio::test]
    async fn runtime_attempt_already_tracked_detects_only_an_exact_match() {
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        let descriptor = crate::stepd::StepdDescriptor::new(
            42,
            7,
            spur_core::step::STEP_BATCH,
            0,
            0,
            std::path::PathBuf::from("/tmp/runtime.sock"),
            std::path::PathBuf::new(),
        );
        sessions
            .lock()
            .await
            .insert(stepd_key(&descriptor), descriptor);

        assert!(
            !runtime_attempt_already_tracked(&sessions, 42, spur_core::step::STEP_BATCH, 6).await
        );
        assert!(
            runtime_attempt_already_tracked(&sessions, 42, spur_core::step::STEP_BATCH, 7).await
        );
        assert!(
            !runtime_attempt_already_tracked(&sessions, 42, spur_core::step::STEP_BATCH, 8).await
        );
        assert!(
            !runtime_attempt_already_tracked(&sessions, 99, spur_core::step::STEP_BATCH, 7).await
        );
    }

    #[tokio::test]
    async fn claim_stepd_slot_refuses_to_clobber_a_newer_attempt() {
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        let newer = crate::stepd::StepdDescriptor::new(
            42,
            8,
            spur_core::step::STEP_BATCH,
            0,
            0,
            std::path::PathBuf::from("/tmp/runtime.sock"),
            std::path::PathBuf::new(),
        );
        sessions
            .lock()
            .await
            .insert(stepd_key(&newer), newer.clone());

        // A slower, now-superseded launch for an older attempt loses the race
        // and must not overwrite the already-tracked newer session.
        let older = crate::stepd::StepdDescriptor::new(
            42,
            7,
            spur_core::step::STEP_BATCH,
            0,
            0,
            std::path::PathBuf::from("/tmp/runtime.sock"),
            std::path::PathBuf::new(),
        );
        let result = claim_stepd_slot(&sessions, older.clone()).await;
        assert_eq!(result, Err(older));
        assert_eq!(
            sessions
                .lock()
                .await
                .get(&(42, spur_core::step::STEP_BATCH)),
            Some(&newer)
        );

        // A same-or-newer claim succeeds and updates the tracked descriptor.
        let same = crate::stepd::StepdDescriptor::new(
            42,
            8,
            spur_core::step::STEP_BATCH,
            0,
            0,
            std::path::PathBuf::from("/tmp/runtime.sock"),
            std::path::PathBuf::new(),
        );
        assert!(claim_stepd_slot(&sessions, same.clone()).await.is_ok());
        assert_eq!(
            sessions
                .lock()
                .await
                .get(&(42, spur_core::step::STEP_BATCH)),
            Some(&same)
        );
    }

    // A bare unit stop doesn't kill the job's cgroup — the displaced attempt's
    // cgroup must be reaped directly before a new attempt can launch on top of it.
    #[tokio::test]
    async fn fence_displaced_stepd_reaps_its_cgroup() {
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        let cgroup_root = tempfile::tempdir().expect("cgroup root");
        // Named as the real thing: cleanup refuses a path that is not ours.
        let cgroup = TestCgroup::new(&cgroup_root);
        let blocker = cgroup.path().join("cgroup.kill");
        std::fs::create_dir(&blocker).expect("seed cgroup.kill blocker");
        let blocker_removed = blocker.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(25));
            std::fs::remove_dir(&blocker_removed).expect("clear blocker");
        });
        let displaced = crate::stepd::StepdDescriptor::new(
            42,
            1,
            spur_core::step::STEP_BATCH,
            0,
            0,
            std::path::PathBuf::from("/nonexistent/runtime.sock"),
            cgroup.path().to_path_buf(),
        );
        sessions
            .lock()
            .await
            .insert(stepd_key(&displaced), displaced.clone());

        // pid 0 isn't a live process, so the stop signal is a trivial no-op —
        // cgroup confirmation alone must still be enough to proceed.
        let result = fence_displaced_stepd(&sessions, 42, spur_core::step::STEP_BATCH, 2).await;

        assert!(
            result.is_ok(),
            "cgroup confirmation must be enough even when the unit stop fails: {result:?}"
        );
        assert!(
            !cgroup.path().exists(),
            "the displaced attempt's cgroup must be reaped, not left for the new attempt to share"
        );
        assert!(!sessions
            .lock()
            .await
            .contains_key(&(42, spur_core::step::STEP_BATCH)));
    }

    /// A superseded attempt can never finalize, so without this reap a
    /// relaunching job leaves one directory per attempt behind.
    #[tokio::test]
    async fn fence_displaced_stepd_reaps_a_superseded_attempts_session_dir() {
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        let cgroup_root = tempfile::tempdir().expect("cgroup root");
        let cgroup = TestCgroup::new(&cgroup_root);
        let state = tempfile::tempdir().expect("runtime state directory");
        let store = crate::stepd::StepdStore::new(state.path());
        let session_dir = store
            .prepare_session_dir(42, 1, spur_core::step::STEP_BATCH)
            .expect("session dir");
        std::fs::write(session_dir.join("launch.json"), b"{}").expect("seed session contents");

        let displaced = crate::stepd::StepdDescriptor::new(
            42,
            1,
            spur_core::step::STEP_BATCH,
            0,
            0,
            session_dir.join("runtime.sock"),
            cgroup.path().to_path_buf(),
        );
        sessions
            .lock()
            .await
            .insert(stepd_key(&displaced), displaced.clone());

        fence_displaced_stepd(&sessions, 42, spur_core::step::STEP_BATCH, 2)
            .await
            .expect("fencing a dead supervisor succeeds");

        assert!(
            !session_dir.exists(),
            "attempt 1's session directory must not outlive the attempt that displaced it"
        );
    }

    #[tokio::test]
    async fn a_relaunch_of_the_same_attempt_keeps_the_session_dir_it_reuses() {
        let state = tempfile::tempdir().expect("runtime state directory");
        let store = crate::stepd::StepdStore::new(state.path());
        let session_dir = store
            .prepare_session_dir(42, 3, spur_core::step::STEP_BATCH)
            .expect("session dir");
        let displaced = crate::stepd::StepdDescriptor::new(
            42,
            3,
            spur_core::step::STEP_BATCH,
            0,
            0,
            session_dir.join("runtime.sock"),
            std::path::PathBuf::new(),
        );

        discard_superseded_session(&displaced, 3);

        assert!(
            session_dir.exists(),
            "the incoming attempt writes into this very directory"
        );
    }

    // A successful unit stop must NOT substitute for cgroup confirmation: the
    // supervisor exiting says nothing about whether its job's cgroup is empty.
    #[tokio::test]
    async fn runtime_teardown_confirmed_requires_the_cgroup_even_when_stop_succeeded() {
        let cgroup_root = tempfile::tempdir().expect("cgroup root");
        // Named as the real thing: cleanup refuses a path that is not ours.
        let cgroup = TestCgroup::new(&cgroup_root);
        // A leftover file fails rmdir the way a busy cgroup does. It must not be a
        // directory: reaping clears child cgroups, so a dir would be removed.
        std::fs::write(cgroup.path().join("cgroup.procs"), "not-a-pid\n")
            .expect("seed a permanent removal blocker");

        let mut descriptor = crate::stepd::StepdDescriptor::new(
            42,
            7,
            spur_core::step::STEP_BATCH,
            0,
            0,
            std::path::PathBuf::from("/tmp/unused.sock"),
            std::path::PathBuf::new(),
        );
        descriptor.cgroup_path = cgroup.path().to_path_buf();

        let confirmed = runtime_teardown_confirmed(&descriptor, &Ok(())).await;

        assert!(
            !confirmed,
            "a successful unit stop must not override an unconfirmed cgroup"
        );
    }

    // A descriptor read before the supervisor published its cgroup carries none.
    // The derived path's absence proves nothing, so the stop decides.
    #[tokio::test]
    async fn runtime_teardown_confirmed_trusts_stop_result_when_there_is_no_cgroup() {
        let descriptor = crate::stepd::StepdDescriptor::new(
            42,
            7,
            spur_core::step::STEP_BATCH,
            0,
            0,
            std::path::PathBuf::from("/tmp/unused.sock"),
            std::path::PathBuf::new(),
        );
        assert!(descriptor.cgroup_path.as_os_str().is_empty());

        assert!(runtime_teardown_confirmed(&descriptor, &Ok(())).await);
        assert!(
            !runtime_teardown_confirmed(
                &descriptor,
                &Err(std::io::Error::other("systemctl stop failed"))
            )
            .await
        );
    }

    #[test]
    fn runtime_sigkill_escalation_requires_the_exact_session_attempt() {
        let descriptor = crate::stepd::StepdDescriptor::new(
            42,
            7,
            spur_core::step::STEP_BATCH,
            101,
            202,
            std::path::PathBuf::from("/tmp/runtime.sock"),
            std::path::PathBuf::new(),
        );
        assert!(stepd_is_current(&descriptor, &descriptor));

        let mut replacement = descriptor.clone();
        replacement.run_attempt = 8;
        assert!(!stepd_is_current(&replacement, &descriptor));

        let mut restarted = descriptor.clone();
        restarted.process_start_ticks = 203;
        assert!(!stepd_is_current(&restarted, &descriptor));
    }

    #[test]
    fn runtime_completion_requires_a_durable_exit_obligation() {
        let state = tempfile::tempdir().expect("runtime state directory");
        let store = crate::stepd::StepdStore::new(state.path());
        let descriptor = crate::stepd::StepdDescriptor::new(
            42,
            7,
            spur_core::step::STEP_BATCH,
            0,
            0,
            store
                .session_dir(42, 7, spur_core::step::STEP_BATCH)
                .join("runtime.sock"),
            std::path::PathBuf::new(),
        );
        store.publish(&descriptor).expect("publish descriptor");

        assert_eq!(
            durable_runtime_exit(&store, &descriptor).expect("read missing exit"),
            None
        );

        store
            .obligations(42, 7, spur_core::step::STEP_BATCH)
            .append(&crate::stepd::StepdObligation::ExitObserved {
                exit_code: 9,
                signal: 15,
            })
            .expect("record exit");
        assert_eq!(
            durable_runtime_exit(&store, &descriptor).expect("read durable exit"),
            Some((9, 15))
        );
    }

    #[tokio::test]
    async fn rejected_recovery_releases_only_the_matching_attempt() {
        let running = new_running_jobs();
        let allocation = Arc::new(Mutex::new(NodeAllocation::new(
            "test-node".into(),
            &ResourceSet::default(),
        )));
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        let cleanup = StepdRecoveryCleanup {
            running: running.clone(),
            allocation,
            stepds: sessions.clone(),
        };
        let state = tempfile::tempdir().expect("runtime state directory");
        let mut descriptor = crate::stepd::StepdDescriptor::new(
            42,
            7,
            spur_core::step::STEP_BATCH,
            0,
            0,
            state.path().join("runtime.sock"),
            std::path::PathBuf::new(),
        );
        descriptor.capability = "test-capability".into();
        sessions
            .lock()
            .await
            .insert(stepd_key(&descriptor), descriptor.clone());
        let mut tracked = TrackedJob::dummy(0);
        tracked.run_attempt = 7;
        running.lock().await.insert(42, tracked);

        cleanup.release_tracking(&descriptor).await;

        assert!(!sessions
            .lock()
            .await
            .contains_key(&(42, spur_core::step::STEP_BATCH)));
        assert!(!running.lock().await.contains_key(&42));

        let newer = crate::stepd::StepdDescriptor::new(
            42,
            8,
            spur_core::step::STEP_BATCH,
            0,
            0,
            state.path().join("newer.sock"),
            std::path::PathBuf::new(),
        );
        sessions
            .lock()
            .await
            .insert(stepd_key(&newer), newer.clone());
        let mut tracked = TrackedJob::dummy(0);
        tracked.run_attempt = 8;
        running.lock().await.insert(42, tracked);

        cleanup.release_tracking(&descriptor).await;

        assert_eq!(
            sessions
                .lock()
                .await
                .get(&(42, spur_core::step::STEP_BATCH)),
            Some(&newer)
        );
        assert_eq!(
            running.lock().await.get(&42).map(|job| job.run_attempt),
            Some(8)
        );
    }

    #[tokio::test]
    async fn rejected_recovery_keeps_tracking_when_the_runtime_unit_does_not_stop() {
        let running = new_running_jobs();
        let allocation = Arc::new(Mutex::new(NodeAllocation::new(
            "test-node".into(),
            &ResourceSet::default(),
        )));
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        let cleanup = StepdRecoveryCleanup {
            running: running.clone(),
            allocation,
            stepds: sessions.clone(),
        };
        let state = tempfile::tempdir().expect("runtime state directory");
        let descriptor = crate::stepd::StepdDescriptor::new(
            42,
            7,
            spur_core::step::STEP_BATCH,
            0,
            0,
            state.path().join("runtime.sock"),
            std::path::PathBuf::new(),
        );
        sessions
            .lock()
            .await
            .insert(stepd_key(&descriptor), descriptor.clone());
        let mut tracked = TrackedJob::dummy(0);
        tracked.run_attempt = 7;
        running.lock().await.insert(42, tracked);

        cleanup
            .finish_rejection(
                &descriptor,
                Err(std::io::Error::other("unit remains active")),
            )
            .await;

        assert_eq!(
            sessions
                .lock()
                .await
                .get(&(42, spur_core::step::STEP_BATCH)),
            Some(&descriptor)
        );
        assert_eq!(
            running.lock().await.get(&42).map(|job| job.run_attempt),
            Some(7)
        );
    }

    // A bare `systemctl stop` only signals the supervisor, which by design
    // ignores a raw SIGTERM for its job's cgroup — so the unit stop failing
    // must not be the only signal consulted. If the cgroup kill itself
    // confirms the job is gone, tracking must still be released.
    #[tokio::test]
    async fn rejected_recovery_releases_tracking_when_the_cgroup_confirms_the_job_is_gone() {
        let running = new_running_jobs();
        let allocation = Arc::new(Mutex::new(NodeAllocation::new(
            "test-node".into(),
            &ResourceSet::default(),
        )));
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        let cleanup = StepdRecoveryCleanup {
            running: running.clone(),
            allocation,
            stepds: sessions.clone(),
        };
        let state = tempfile::tempdir().expect("runtime state directory");
        let cgroup_root = tempfile::tempdir().expect("cgroup root");
        // Named as the real thing: cleanup refuses a path that is not ours.
        let cgroup = TestCgroup::new(&cgroup_root);
        // A plain tempdir can't model real cgroupfs, where writing to the
        // kernel-provided "cgroup.kill" never leaves a stray directory
        // entry. A directory at that path makes the write fail cleanly
        // instead (no file created), and removing it shortly after — same
        // technique as cleanup_cgroup_retries_past_a_transient_removal_failure
        // — lets the directory end up genuinely empty once retried.
        let blocker = cgroup.path().join("cgroup.kill");
        std::fs::create_dir(&blocker).expect("seed cgroup.kill blocker");
        let blocker_removed = blocker.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(25));
            std::fs::remove_dir(&blocker_removed).expect("clear blocker");
        });
        let descriptor = crate::stepd::StepdDescriptor::new(
            42,
            7,
            spur_core::step::STEP_BATCH,
            0,
            0,
            state.path().join("runtime.sock"),
            cgroup.path().to_path_buf(),
        );
        sessions
            .lock()
            .await
            .insert(stepd_key(&descriptor), descriptor.clone());
        let mut tracked = TrackedJob::dummy(0);
        tracked.run_attempt = 7;
        running.lock().await.insert(42, tracked);

        cleanup
            .finish_rejection(
                &descriptor,
                Err(std::io::Error::other("unit remains active")),
            )
            .await;

        assert!(
            !sessions
                .lock()
                .await
                .contains_key(&(42, spur_core::step::STEP_BATCH)),
            "an empty, confirmed-dead cgroup must release tracking even if the unit stop failed"
        );
        assert!(!running.lock().await.contains_key(&42));
    }

    #[tokio::test]
    async fn runtime_completion_releases_the_exact_attempt_and_finalizes_its_state() {
        let state = tempfile::tempdir().expect("runtime state directory");
        let store = crate::stepd::StepdStore::new(state.path());
        let mut descriptor = crate::stepd::StepdDescriptor::new(
            42,
            7,
            spur_core::step::STEP_BATCH,
            0,
            0,
            store
                .session_dir(42, 7, spur_core::step::STEP_BATCH)
                .join("runtime.sock"),
            std::path::PathBuf::new(),
        );
        descriptor.capability = "test-capability".into();
        store.publish(&descriptor).expect("publish descriptor");
        store
            .obligations(42, 7, spur_core::step::STEP_BATCH)
            .append(&crate::stepd::StepdObligation::ExitObserved {
                exit_code: 0,
                signal: 0,
            })
            .expect("record exit");
        store
            .acknowledge_completion(&crate::stepd::PendingStepdCompletion {
                job_id: 42,
                run_attempt: 7,
                step_id: spur_core::step::STEP_BATCH,
                exit_code: 0,
                signal: 0,
                epilog_failed: false,
            })
            .expect("acknowledge completion");

        let running = new_running_jobs();
        let allocation = Arc::new(Mutex::new(NodeAllocation::new(
            "test-node".into(),
            &ResourceSet {
                cpus: 2,
                memory_mb: 1024,
                ..Default::default()
            },
        )));
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        allocation
            .lock()
            .await
            .allocate_for_job(42, 1, 1, 128, &[])
            .expect("reserve allocation");
        assert!(allocation.lock().await.commit_job(42, 1));
        let mut tracked = TrackedJob::dummy(0);
        tracked.run_attempt = 7;
        running.lock().await.insert(42, tracked);
        sessions
            .lock()
            .await
            .insert(stepd_key(&descriptor), descriptor.clone());

        assert!(
            release_stepd_tracking(
                &running,
                &allocation,
                &sessions,
                &descriptor,
                "runtime completion",
            )
            .await
        );

        assert!(!running.lock().await.contains_key(&42));
        assert!(!sessions
            .lock()
            .await
            .contains_key(&(42, spur_core::step::STEP_BATCH)));
        // The exit stops the tracking; only an acknowledged completion frees
        // the slice, because until then nothing but this node knows it stopped.
        assert_eq!(allocation.lock().await.allocated_memory_mb, 128);
        assert!(!store
            .session_dir(42, 7, spur_core::step::STEP_BATCH)
            .exists());
    }

    #[tokio::test]
    async fn releasing_one_step_keeps_the_allocation_for_its_running_sibling() {
        let step = |step_id| {
            crate::stepd::StepdDescriptor::new(
                42,
                7,
                step_id,
                0,
                0,
                std::path::PathBuf::from("/tmp/runtime.sock"),
                std::path::PathBuf::new(),
            )
        };
        let first = step(spur_core::step::STEP_BATCH);
        let second = step(9);

        let running = new_running_jobs();
        let allocation = Arc::new(Mutex::new(NodeAllocation::new(
            "test-node".into(),
            &ResourceSet {
                cpus: 2,
                memory_mb: 1024,
                ..Default::default()
            },
        )));
        allocation
            .lock()
            .await
            .allocate_for_job(42, 1, 1, 128, &[])
            .expect("reserve allocation");
        assert!(allocation.lock().await.commit_job(42, 1));
        let mut tracked = TrackedJob::dummy(0);
        tracked.run_attempt = 7;
        running.lock().await.insert(42, tracked);
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        for descriptor in [&first, &second] {
            sessions
                .lock()
                .await
                .insert(stepd_key(descriptor), descriptor.clone());
        }

        release_stepd_tracking(&running, &allocation, &sessions, &first, "step exit").await;

        assert!(
            running.lock().await.contains_key(&42),
            "the job outlives the first of its steps to exit"
        );
        assert_eq!(
            allocation.lock().await.allocated_memory_mb,
            128,
            "the sibling step is still drawing on the allocation"
        );

        release_stepd_tracking(&running, &allocation, &sessions, &second, "step exit").await;

        assert!(!running.lock().await.contains_key(&42));
        assert_eq!(
            allocation.lock().await.allocated_memory_mb,
            128,
            "even the last step out only stops tracking; the slice waits on the controller"
        );
    }

    #[test]
    fn an_interactive_allocation_is_recorded_as_the_extern_step() {
        assert_eq!(launch_step_id(true), spur_core::step::STEP_EXTERN);
        assert_eq!(launch_step_id(false), spur_core::step::STEP_BATCH);
    }

    // A supervised job has no tracked pid, so exec and attach enter through the
    // process at the root of its cgroup rather than being refused.
    #[test]
    fn the_cgroup_root_pid_is_the_member_whose_parent_is_outside() {
        let dir = tempfile::tempdir().expect("tempdir");
        // This process's parent is outside the set; the other member sits above
        // pid_max, so its parentage cannot be read and it is not the root.
        let me = std::process::id() as i32;
        std::fs::write(dir.path().join("cgroup.procs"), format!("9999999\n{me}\n"))
            .expect("write procs");
        assert_eq!(cgroup_root_pid(dir.path()), Some(me));
    }

    #[test]
    fn a_cgroup_with_no_members_yields_no_pid() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("cgroup.procs"), b"").expect("write procs");
        assert_eq!(cgroup_root_pid(dir.path()), None);
        assert_eq!(cgroup_root_pid(std::path::Path::new("/nonexistent")), None);
    }

    #[test]
    fn an_allocation_supervisor_does_not_own_the_jobs_processes() {
        let descriptor = |step_id| {
            crate::stepd::StepdDescriptor::new(
                42,
                1,
                step_id,
                0,
                0,
                std::path::PathBuf::from("/tmp/runtime.sock"),
                std::path::PathBuf::new(),
            )
        };
        assert!(!owns_job_processes(&[descriptor(
            spur_core::step::STEP_EXTERN
        )]));
        assert!(owns_job_processes(&[descriptor(
            spur_core::step::STEP_BATCH
        )]));
        // A supervised step speaks only for itself: the allocation's own steps
        // may still be running under the agent.
        assert!(!owns_job_processes(&[
            descriptor(spur_core::step::STEP_EXTERN),
            descriptor(7),
        ]));
        assert!(owns_job_processes(&[
            descriptor(spur_core::step::STEP_BATCH),
            descriptor(7),
        ]));
    }

    #[tokio::test]
    async fn a_superseded_attempts_session_does_not_strand_the_allocation() {
        let current = crate::stepd::StepdDescriptor::new(
            42,
            7,
            spur_core::step::STEP_BATCH,
            0,
            0,
            std::path::PathBuf::from("/tmp/runtime.sock"),
            std::path::PathBuf::new(),
        );
        let superseded = crate::stepd::StepdDescriptor::new(
            42,
            6,
            9,
            0,
            0,
            std::path::PathBuf::from("/tmp/runtime.sock"),
            std::path::PathBuf::new(),
        );

        let running = new_running_jobs();
        let allocation = Arc::new(Mutex::new(NodeAllocation::new(
            "test-node".into(),
            &ResourceSet {
                cpus: 2,
                memory_mb: 1024,
                ..Default::default()
            },
        )));
        allocation
            .lock()
            .await
            .allocate_for_job(42, 1, 1, 128, &[])
            .expect("reserve allocation");
        assert!(allocation.lock().await.commit_job(42, 1));
        let mut tracked = TrackedJob::dummy(0);
        tracked.run_attempt = 7;
        running.lock().await.insert(42, tracked);
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        for descriptor in [&current, &superseded] {
            sessions
                .lock()
                .await
                .insert(stepd_key(descriptor), descriptor.clone());
        }

        release_stepd_tracking(&running, &allocation, &sessions, &current, "step exit").await;

        assert_eq!(
            allocation.lock().await.allocated_memory_mb,
            128,
            "a session left by an older attempt cannot free the current one's slice"
        );
    }

    #[tokio::test]
    async fn release_runtime_tracking_spares_a_reused_job_ids_allocation() {
        let stale = crate::stepd::StepdDescriptor::new(
            42,
            7,
            spur_core::step::STEP_BATCH,
            0,
            0,
            std::path::PathBuf::from("/tmp/runtime.sock"),
            std::path::PathBuf::new(),
        );
        let running = new_running_jobs();
        let allocation = Arc::new(Mutex::new(NodeAllocation::new(
            "test-node".into(),
            &ResourceSet {
                cpus: 2,
                memory_mb: 1024,
                ..Default::default()
            },
        )));
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        // A redispatch already retracked job 42 under a newer attempt with its
        // own committed allocation before this stale (attempt 7) report lands.
        allocation
            .lock()
            .await
            .allocate_for_job(42, 1, 1, 128, &[])
            .expect("reserve allocation");
        assert!(allocation.lock().await.commit_job(42, 1));
        let mut newer = TrackedJob::dummy(0);
        newer.run_attempt = 8;
        running.lock().await.insert(42, newer);

        assert!(
            !release_stepd_tracking(&running, &allocation, &sessions, &stale, "stale report",)
                .await
        );

        assert_eq!(
            running.lock().await.get(&42).map(|job| job.run_attempt),
            Some(8),
            "a stale report must not evict the current attempt's tracking"
        );
        assert_eq!(
            allocation.lock().await.allocated_memory_mb,
            128,
            "a stale report must not release the current attempt's allocation"
        );
    }

    #[tokio::test]
    async fn liveness_watchdog_fences_a_stepd_whose_process_is_gone() {
        let state = tempfile::tempdir().expect("runtime state directory");
        let store = crate::stepd::StepdStore::new(state.path());
        let pid = std::process::id();
        let mut descriptor = crate::stepd::StepdDescriptor::new(
            42,
            7,
            spur_core::step::STEP_BATCH,
            pid,
            crate::stepd::process_start_ticks(pid).expect("start ticks") + 1,
            store
                .session_dir(42, 7, spur_core::step::STEP_BATCH)
                .join("runtime.sock"),
            std::path::PathBuf::new(),
        );
        descriptor.capability = "test-capability".into();
        store.publish(&descriptor).expect("publish descriptor");

        let running = new_running_jobs();
        let allocation = Arc::new(Mutex::new(NodeAllocation::new(
            "test-node".into(),
            &ResourceSet {
                cpus: 2,
                memory_mb: 1024,
                ..Default::default()
            },
        )));
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        allocation
            .lock()
            .await
            .allocate_for_job(42, 1, 1, 128, &[])
            .expect("reserve allocation");
        assert!(allocation.lock().await.commit_job(42, 1));
        let mut tracked = TrackedJob::dummy(0);
        tracked.run_attempt = 7;
        running.lock().await.insert(42, tracked);
        sessions
            .lock()
            .await
            .insert(stepd_key(&descriptor), descriptor.clone());

        fence_dead_stepd(
            &fence_context(
                &running,
                &allocation,
                &sessions,
                &crate::step_completion::StepCompletions::new(),
                &store,
            ),
            descriptor,
        )
        .await;

        assert!(!running.lock().await.contains_key(&42));
        assert!(!sessions
            .lock()
            .await
            .contains_key(&(42, spur_core::step::STEP_BATCH)));
        assert_eq!(
            store
                .observed_exit(42, 7, spur_core::step::STEP_BATCH)
                .expect("read exit"),
            Some((0, nix::sys::signal::Signal::SIGKILL as i32))
        );
    }

    // A supervisor dying without pushing completion leaves the RPC that launched
    // its step parked forever, so releasing tracking has to deliver the exit.
    #[tokio::test]
    async fn fencing_a_dead_stepd_wakes_the_parked_step_caller() {
        let state = tempfile::tempdir().expect("runtime state directory");
        let store = crate::stepd::StepdStore::new(state.path());
        let pid = std::process::id();
        let step_id = 0;
        let mut descriptor = crate::stepd::StepdDescriptor::new(
            43,
            7,
            step_id,
            pid,
            crate::stepd::process_start_ticks(pid).expect("start ticks") + 1,
            store.session_dir(43, 7, step_id).join("runtime.sock"),
            std::path::PathBuf::new(),
        );
        descriptor.capability = "test-capability".into();
        store.publish(&descriptor).expect("publish descriptor");

        let running = new_running_jobs();
        let allocation = Arc::new(Mutex::new(NodeAllocation::new(
            "test-node".into(),
            &ResourceSet {
                cpus: 2,
                memory_mb: 1024,
                ..Default::default()
            },
        )));
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        sessions
            .lock()
            .await
            .insert(stepd_key(&descriptor), descriptor.clone());

        let completions = crate::step_completion::StepCompletions::new();
        let mut waiter = completions.register(43, 7, step_id).await;

        fence_dead_stepd(
            &fence_context(&running, &allocation, &sessions, &completions, &store),
            descriptor,
        )
        .await;

        // try_recv, not await: an undelivered outcome must fail the test rather
        // than park it on a supervisor that is already gone.
        let outcome = waiter
            .try_recv()
            .expect("a fenced step must report an outcome to its parked caller");
        assert_eq!(outcome.signal, nix::sys::signal::Signal::SIGKILL as i32);
    }

    /// Build a published, tracked session for a dead supervisor, ready to fence.
    fn fenced_session(store: &crate::stepd::StepdStore) -> crate::stepd::StepdDescriptor {
        let pid = std::process::id();
        let mut descriptor = crate::stepd::StepdDescriptor::new(
            42,
            7,
            spur_core::step::STEP_BATCH,
            pid,
            crate::stepd::process_start_ticks(pid).expect("start ticks") + 1,
            store
                .session_dir(42, 7, spur_core::step::STEP_BATCH)
                .join("runtime.sock"),
            std::path::PathBuf::new(),
        );
        descriptor.capability = "test-capability".into();
        store.publish(&descriptor).expect("publish descriptor");
        descriptor
    }

    /// The same session, naming a real running process as its workload.
    fn fenced_session_holding(
        store: &crate::stepd::StepdStore,
        workload_pid: u32,
    ) -> crate::stepd::StepdDescriptor {
        let mut descriptor = fenced_session(store);
        descriptor.workload_pid = workload_pid;
        descriptor.workload_start_ticks =
            crate::stepd::process_start_ticks(workload_pid).expect("workload start ticks");
        store.publish(&descriptor).expect("publish descriptor");
        descriptor
    }

    // Reaping only the cgroup leaves the workload running wherever cgroups are
    // unavailable, which is every job on a node that cannot use them.
    #[tokio::test]
    async fn fencing_a_dead_stepd_reaps_the_workload_it_left_behind() {
        let (controller_addr, reports) = spawn_mock_controller();
        let state = tempfile::tempdir().expect("runtime state directory");
        let store = crate::stepd::StepdStore::new(state.path());
        let mut child = std::process::Command::new("sleep")
            .arg("300")
            .spawn()
            .expect("spawn a stand-in workload");
        let descriptor = fenced_session_holding(&store, child.id());

        let running = new_running_jobs();
        let allocation = Arc::new(Mutex::new(NodeAllocation::new(
            "test-node".into(),
            &ResourceSet::default(),
        )));
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        sessions
            .lock()
            .await
            .insert(stepd_key(&descriptor), descriptor.clone());
        let mut context = fence_context(
            &running,
            &allocation,
            &sessions,
            &crate::step_completion::StepCompletions::new(),
            &store,
        );
        context.controller_addr = controller_addr;

        fence_dead_stepd(&context, descriptor.clone()).await;

        assert!(
            !crate::stepd::process_is_live(
                descriptor.workload_pid,
                descriptor.workload_start_ticks
            ),
            "the fenced workload must actually be dead, not merely reported so"
        );
        assert_eq!(
            reports.lock().expect("completion reports").len(),
            1,
            "with the workload gone the run's completion must reach the controller"
        );
        let _ = child.wait();
    }

    // The agent's tracked copy is written before the launch returns, so it names
    // no workload. Answering from it declares a live payload dead.
    #[tokio::test]
    async fn fencing_reads_the_workload_the_supervisor_published_not_the_stale_copy() {
        let (controller_addr, reports) = spawn_mock_controller();
        let state = tempfile::tempdir().expect("runtime state directory");
        let store = crate::stepd::StepdStore::new(state.path());
        let mut child = std::process::Command::new("sleep")
            .arg("300")
            .spawn()
            .expect("spawn a stand-in workload");
        let published = fenced_session_holding(&store, child.id());
        let mut stale = published.clone();
        stale.workload_pid = 0;
        stale.workload_start_ticks = 0;

        let running = new_running_jobs();
        let allocation = Arc::new(Mutex::new(NodeAllocation::new(
            "test-node".into(),
            &ResourceSet::default(),
        )));
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        sessions
            .lock()
            .await
            .insert(stepd_key(&stale), stale.clone());
        let mut context = fence_context(
            &running,
            &allocation,
            &sessions,
            &crate::step_completion::StepCompletions::new(),
            &store,
        );
        context.controller_addr = controller_addr;

        fence_dead_stepd(&context, stale).await;

        assert!(
            !crate::stepd::process_is_live(published.workload_pid, published.workload_start_ticks),
            "the fence must kill the workload the supervisor published, not the one it was handed"
        );
        assert_eq!(
            reports.lock().expect("completion reports").len(),
            1,
            "with the workload gone the run's completion must reach the controller"
        );
        let _ = child.wait();
    }

    // A run-ending report discharges every sibling's obligation, so the owner
    // step's own evidence is not enough to license it.
    #[tokio::test]
    async fn a_sibling_step_still_running_withholds_the_runs_completion() {
        let state = tempfile::tempdir().expect("runtime state directory");
        let store = crate::stepd::StepdStore::new(state.path());
        let owner = fenced_session(&store);
        let mut child = std::process::Command::new("sleep")
            .arg("300")
            .spawn()
            .expect("spawn a stand-in workload");

        let sibling_step = 0;
        let mut sibling = crate::stepd::StepdDescriptor::new(
            owner.job_id,
            owner.run_attempt,
            sibling_step,
            owner.pid,
            owner.process_start_ticks,
            store
                .session_dir(owner.job_id, owner.run_attempt, sibling_step)
                .join("runtime.sock"),
            std::path::PathBuf::new(),
        );
        sibling.workload_pid = child.id();
        sibling.workload_start_ticks =
            crate::stepd::process_start_ticks(child.id()).expect("workload start ticks");
        store
            .publish(&sibling)
            .expect("publish the sibling session");

        assert!(
            payload_still_executing(&CompletionReport {
                job_id: owner.job_id,
                exit_code: 0,
                signal: 0,
                run_attempt: owner.run_attempt,
                reporting_node: "test-node",
                drain: None,
                step_id: Some(owner.step_id),
                payload: PayloadEvidence::Supervised(&store),
            }),
            "the owner step is done, but the run's payload is not"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    // The gate that keeps a slice from being freed under a running payload. It
    // is the report that must not go: an acknowledgement is what frees cores.
    #[tokio::test]
    async fn a_run_whose_payload_is_still_executing_reports_nothing() {
        let (controller_addr, reports) = spawn_mock_controller();
        let state = tempfile::tempdir().expect("runtime state directory");
        let store = crate::stepd::StepdStore::new(state.path());
        let mut child = std::process::Command::new("sleep")
            .arg("300")
            .spawn()
            .expect("spawn a stand-in workload");
        let descriptor = fenced_session_holding(&store, child.id());

        let outcome = report_completion(
            &controller_addr,
            CompletionReport {
                job_id: descriptor.job_id,
                exit_code: 0,
                signal: 0,
                run_attempt: descriptor.run_attempt,
                reporting_node: "test-node",
                drain: None,
                step_id: Some(descriptor.step_id),
                payload: PayloadEvidence::Supervised(&store),
            },
        )
        .await;

        assert_eq!(outcome, CompletionOutcome::Undelivered);
        assert_eq!(
            reports.lock().expect("completion reports").len(),
            0,
            "a report the controller would acknowledge is a slice freed under a live payload"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    async fn fence_against_controller(controller_addr: String, store: &crate::stepd::StepdStore) {
        let descriptor = fenced_session(store);
        let running = new_running_jobs();
        let allocation = Arc::new(Mutex::new(NodeAllocation::new(
            "test-node".into(),
            &ResourceSet::default(),
        )));
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        sessions
            .lock()
            .await
            .insert(stepd_key(&descriptor), descriptor.clone());
        let mut context = fence_context(
            &running,
            &allocation,
            &sessions,
            &crate::step_completion::StepCompletions::new(),
            store,
        );
        context.controller_addr = controller_addr;
        fence_dead_stepd(&context, descriptor).await;
    }

    fn pending_replays(store: &crate::stepd::StepdStore) -> usize {
        store
            .discover_unacknowledged_completions()
            .expect("discover unacknowledged completions")
            .len()
    }

    // Unreported, a dead supervisor's job stays Running on the controller and
    // holds its allocation forever.
    #[tokio::test]
    async fn fencing_a_dead_stepd_reports_the_death_to_the_controller() {
        let (controller_addr, reports) = spawn_mock_controller();
        let state = tempfile::tempdir().expect("runtime state directory");
        let store = crate::stepd::StepdStore::new(state.path());

        fence_against_controller(controller_addr, &store).await;

        let reports = reports.lock().expect("completion reports").clone();
        assert_eq!(reports.len(), 1, "fencing must report exactly one exit");
        assert_eq!(reports[0].job_id, 42);
        assert_eq!(reports[0].run_attempt, 7);
        assert_eq!(reports[0].step_id, Some(spur_core::step::STEP_BATCH));
        assert_eq!(reports[0].signal, nix::sys::signal::Signal::SIGKILL as i32);
        assert_eq!(
            pending_replays(&store),
            0,
            "an acknowledged report leaves nothing for the replay loop"
        );
    }

    /// Fence a run that really holds two of this node's four cores, so the
    /// assertion is about the slice and not only about the report.
    async fn fence_a_slice_holding_run(
        controller_addr: String,
        store: &crate::stepd::StepdStore,
        admissions: &crate::admission::AdmissionStore,
    ) -> Arc<Mutex<NodeAllocation>> {
        let descriptor = fenced_session(store);
        let allocation = Arc::new(Mutex::new(NodeAllocation::new(
            "test-node".into(),
            &ResourceSet {
                cpus: 4,
                memory_mb: 8_000,
                ..Default::default()
            },
        )));
        let reserved = allocation
            .lock()
            .await
            .allocate_for_job(42, 7, 2, 1_000, &[])
            .expect("reserve the run's cores");
        allocation.lock().await.commit_job(42, 7);

        let mut run = crate::admission::RunAdmission::new(
            42,
            7,
            "test-node",
            crate::admission::AdmittedResources {
                cpu_ids: reserved.cpu_ids.clone(),
                memory_mb: reserved.memory_mb,
                gpu_devices: Vec::new(),
            },
            crate::admission::now_unix_ms(),
        );
        run.lifecycle_owner_step = Some(spur_core::step::STEP_BATCH);
        admissions.admit_run(&run).expect("admit the run");
        let mut participant = crate::admission::ParticipantAdmission::new(
            42,
            7,
            spur_core::step::STEP_BATCH,
            "test-node",
            run.allocation.clone(),
        );
        participant.final_report.required = true;
        admissions
            .admit_participant(&participant)
            .expect("admit the participant");

        let sessions = Arc::new(Mutex::new(HashMap::new()));
        sessions
            .lock()
            .await
            .insert(stepd_key(&descriptor), descriptor.clone());
        let mut context = fence_context(
            &new_running_jobs(),
            &allocation,
            &sessions,
            &crate::step_completion::StepCompletions::new(),
            store,
        );
        context.controller_addr = controller_addr;
        context.admissions = admissions.clone();
        fence_dead_stepd(&context, descriptor).await;
        allocation
    }

    /// This process's pid with the wrong start time, so the liveness check reads
    /// it as gone without depending on any other pid.
    fn a_gone_supervisor() -> crate::admission::SupervisorRef {
        let pid = std::process::id();
        crate::admission::SupervisorRef {
            pid,
            start_ticks: crate::stepd::process_start_ticks(pid).expect("start ticks") + 1,
            boot_id: crate::admission::current_boot_id(),
        }
    }

    fn a_live_supervisor() -> crate::admission::SupervisorRef {
        let pid = std::process::id();
        crate::admission::SupervisorRef {
            pid,
            start_ticks: crate::stepd::process_start_ticks(pid).expect("start ticks"),
            boot_id: crate::admission::current_boot_id(),
        }
    }

    /// A run holding the named cores, whose step owes a report and whose
    /// supervisor is whatever the caller wants it to be.
    fn admit_a_supervised_run(
        admissions: &crate::admission::AdmissionStore,
        node: &str,
        job_id: u32,
        cpu_ids: Vec<u32>,
        supervisor: Option<crate::admission::SupervisorRef>,
    ) {
        let mut run = crate::admission::RunAdmission::new(
            job_id,
            1,
            node,
            crate::admission::AdmittedResources {
                cpu_ids,
                memory_mb: 1_000,
                gpu_devices: Vec::new(),
            },
            crate::admission::now_unix_ms(),
        );
        run.lifecycle_owner_step = Some(spur_core::step::STEP_BATCH);
        admissions.admit_run(&run).expect("admit the run");
        let mut participant = crate::admission::ParticipantAdmission::new(
            job_id,
            1,
            spur_core::step::STEP_BATCH,
            node,
            run.allocation.clone(),
        );
        participant.final_report.required = true;
        participant.lifecycle = crate::admission::ParticipantLifecycle::Running;
        participant.supervisor = supervisor;
        admissions
            .admit_participant(&participant)
            .expect("admit the participant");
    }

    fn four_core_node() -> Arc<Mutex<NodeAllocation>> {
        Arc::new(Mutex::new(NodeAllocation::new(
            "test-node".into(),
            &ResourceSet {
                cpus: 4,
                memory_mb: 8_000,
                ..Default::default()
            },
        )))
    }

    // A supervisor that died without reporting left nothing to report, so the
    // agent never asked and the record charged cores across every restart.
    #[tokio::test]
    async fn a_run_nothing_can_speak_for_is_reported_and_its_slice_freed() {
        let (controller_addr, reports) =
            spawn_mock_controller_answering(Some(tonic::Code::NotFound));
        let state = tempfile::tempdir().expect("runtime state directory");
        let spool = tempfile::tempdir().expect("admission state directory");
        let store = crate::stepd::StepdStore::new(state.path());
        let admissions = crate::admission::AdmissionStore::new(spool.path(), "test-node");
        let allocation = four_core_node();
        admit_a_supervised_run(
            &admissions,
            "test-node",
            42,
            vec![0, 1],
            Some(a_gone_supervisor()),
        );
        allocation
            .lock()
            .await
            .restore_for_job(42, 1, &[0, 1], 1_000, &[])
            .expect("charge the recorded slice");

        replay_unacknowledged_stepd_completions(
            &store,
            &admissions,
            &allocation,
            &controller_addr,
            "test-node",
        )
        .await
        .expect("replay");

        assert_eq!(
            reports.lock().expect("completion reports").len(),
            1,
            "the loss has to be reported before anything is concluded"
        );
        assert_eq!(
            allocation.lock().await.free_cpus(),
            4,
            "the slice goes back on the acknowledgement, as on every other path"
        );
        assert!(
            admissions.ledger_cut("session-a").entries.is_empty(),
            "and the cut stops advertising a claim the node no longer holds"
        );
        assert_eq!(
            admissions
                .sweep(crate::admission::now_unix_ms(), 1, &HashSet::new())
                .expect("sweep"),
            1,
            "nothing is owed any more, so the record is collectable"
        );
    }

    // Three of these covering the same cores took a four-core node to nothing
    // usable, and a restart made it worse rather than better.
    #[tokio::test]
    async fn a_restart_holding_records_nothing_can_speak_for_recovers_the_pool() {
        let (controller_addr, _reports) =
            spawn_mock_controller_answering(Some(tonic::Code::NotFound));
        let state = tempfile::tempdir().expect("runtime state directory");
        let spool = tempfile::tempdir().expect("admission state directory");
        let store = crate::stepd::StepdStore::new(state.path());
        let admissions = crate::admission::AdmissionStore::new(spool.path(), "test-node");
        let allocation = four_core_node();
        for job_id in [30_866, 30_867, 30_868] {
            admit_a_supervised_run(
                &admissions,
                "test-node",
                job_id,
                vec![0, 1, 2, 3],
                Some(a_gone_supervisor()),
            );
            let _ = allocation
                .lock()
                .await
                .restore_for_job(job_id, 1, &[0, 1, 2, 3], 1_000, &[]);
        }
        assert_eq!(
            allocation.lock().await.free_cpus(),
            0,
            "the restart charges every record, which is what wedged the node"
        );

        replay_unacknowledged_stepd_completions(
            &store,
            &admissions,
            &allocation,
            &controller_addr,
            "test-node",
        )
        .await
        .expect("replay");

        assert_eq!(
            allocation.lock().await.free_cpus(),
            4,
            "every one of them is reported and acknowledged, so the pool comes back"
        );
        allocation
            .lock()
            .await
            .allocate_for_job(99, 1, 4, 1_000, &[])
            .expect("the node must be able to take work again");
    }

    // The failure that matters most: a report would have the controller free
    // the cores of a job that is still running on them.
    #[tokio::test]
    async fn a_run_whose_supervisor_is_alive_is_never_reported_lost() {
        let (controller_addr, reports) =
            spawn_mock_controller_answering(Some(tonic::Code::NotFound));
        let state = tempfile::tempdir().expect("runtime state directory");
        let spool = tempfile::tempdir().expect("admission state directory");
        let store = crate::stepd::StepdStore::new(state.path());
        let admissions = crate::admission::AdmissionStore::new(spool.path(), "test-node");
        let allocation = four_core_node();
        admit_a_supervised_run(
            &admissions,
            "test-node",
            42,
            vec![0, 1],
            Some(a_live_supervisor()),
        );
        allocation
            .lock()
            .await
            .restore_for_job(42, 1, &[0, 1], 1_000, &[])
            .expect("charge the recorded slice");

        replay_unacknowledged_stepd_completions(
            &store,
            &admissions,
            &allocation,
            &controller_addr,
            "test-node",
        )
        .await
        .expect("replay");

        assert!(reports.lock().expect("completion reports").is_empty());
        assert_eq!(allocation.lock().await.free_cpus(), 2);
    }

    // A run still mid-launch has recorded no supervisor at all, and a missing
    // one is not a dead one.
    #[tokio::test]
    async fn a_run_with_no_recorded_supervisor_is_never_reported_lost() {
        let (controller_addr, reports) =
            spawn_mock_controller_answering(Some(tonic::Code::NotFound));
        let state = tempfile::tempdir().expect("runtime state directory");
        let spool = tempfile::tempdir().expect("admission state directory");
        let store = crate::stepd::StepdStore::new(state.path());
        let admissions = crate::admission::AdmissionStore::new(spool.path(), "test-node");
        let allocation = four_core_node();
        admit_a_supervised_run(&admissions, "test-node", 42, vec![0, 1], None);
        allocation
            .lock()
            .await
            .restore_for_job(42, 1, &[0, 1], 1_000, &[])
            .expect("charge the recorded slice");

        replay_unacknowledged_stepd_completions(
            &store,
            &admissions,
            &allocation,
            &controller_addr,
            "test-node",
        )
        .await
        .expect("replay");

        assert!(reports.lock().expect("completion reports").is_empty());
        assert_eq!(allocation.lock().await.free_cpus(), 2);
    }

    /// The session a supervisor leaves behind, naming `workload` as the process
    /// it launched. The supervisor itself is recorded as one already gone.
    fn publish_session_for_workload(
        store: &crate::stepd::StepdStore,
        job_id: u32,
        workload: &crate::admission::SupervisorRef,
    ) {
        let gone = a_gone_supervisor();
        let mut descriptor = crate::stepd::StepdDescriptor::new(
            job_id,
            1,
            spur_core::step::STEP_BATCH,
            gone.pid,
            gone.start_ticks,
            store
                .session_dir(job_id, 1, spur_core::step::STEP_BATCH)
                .join("stepd.sock"),
            std::path::PathBuf::new(),
        );
        descriptor.workload_pid = workload.pid;
        descriptor.workload_start_ticks = workload.start_ticks;
        descriptor.boot_id = workload.boot_id.clone();
        store.publish(&descriptor).expect("publish the session");
    }

    // A supervisor killed outright leaves its payload running. Reporting that
    // run over frees cores the old work is still executing on.
    #[tokio::test]
    async fn a_run_whose_processes_outlived_its_supervisor_is_not_reported_lost() {
        let (controller_addr, reports) = spawn_mock_controller();
        let state = tempfile::tempdir().expect("runtime state directory");
        let spool = tempfile::tempdir().expect("admission state directory");
        let store = crate::stepd::StepdStore::new(state.path());
        let admissions = crate::admission::AdmissionStore::new(spool.path(), "test-node");
        let allocation = four_core_node();
        admit_a_supervised_run(
            &admissions,
            "test-node",
            42,
            vec![0, 1],
            Some(a_gone_supervisor()),
        );
        allocation
            .lock()
            .await
            .restore_for_job(42, 1, &[0, 1], 1_000, &[])
            .expect("charge the recorded slice");
        publish_session_for_workload(&store, 42, &a_live_supervisor());

        replay_unacknowledged_stepd_completions(
            &store,
            &admissions,
            &allocation,
            &controller_addr,
            "test-node",
        )
        .await
        .expect("replay");

        assert!(
            reports.lock().expect("completion reports").is_empty(),
            "the payload is still on these cores, so nothing may report the run over"
        );
        assert_eq!(allocation.lock().await.free_cpus(), 2);
    }

    // The same run once its payload is gone: the report it owes must still go.
    #[tokio::test]
    async fn a_lost_run_is_reported_once_its_payload_is_gone() {
        let (controller_addr, reports) = spawn_mock_controller();
        let state = tempfile::tempdir().expect("runtime state directory");
        let spool = tempfile::tempdir().expect("admission state directory");
        let store = crate::stepd::StepdStore::new(state.path());
        let admissions = crate::admission::AdmissionStore::new(spool.path(), "test-node");
        let allocation = four_core_node();
        admit_a_supervised_run(
            &admissions,
            "test-node",
            42,
            vec![0, 1],
            Some(a_gone_supervisor()),
        );
        allocation
            .lock()
            .await
            .restore_for_job(42, 1, &[0, 1], 1_000, &[])
            .expect("charge the recorded slice");
        publish_session_for_workload(&store, 42, &a_gone_supervisor());

        replay_unacknowledged_stepd_completions(
            &store,
            &admissions,
            &allocation,
            &controller_addr,
            "test-node",
        )
        .await
        .expect("replay");

        assert_eq!(
            reports.lock().expect("completion reports").len(),
            1,
            "a run nothing is left to speak for still owes the controller a report"
        );
        assert_eq!(allocation.lock().await.free_cpus(), 4);
    }

    // The session carries the real exit status; a synthetic loss reported over
    // it would record the wrong outcome for the job.
    #[tokio::test]
    async fn a_run_whose_session_still_holds_an_exit_is_left_to_that_session() {
        let (controller_addr, reports) =
            spawn_mock_controller_answering(Some(tonic::Code::NotFound));
        let state = tempfile::tempdir().expect("runtime state directory");
        let spool = tempfile::tempdir().expect("admission state directory");
        let store = crate::stepd::StepdStore::new(state.path());
        let admissions = crate::admission::AdmissionStore::new(spool.path(), "test-node");
        let allocation = four_core_node();
        admit_a_supervised_run(
            &admissions,
            "test-node",
            42,
            vec![0, 1],
            Some(a_gone_supervisor()),
        );
        let gone = a_gone_supervisor();
        store
            .publish(&crate::stepd::StepdDescriptor::new(
                42,
                1,
                spur_core::step::STEP_BATCH,
                gone.pid,
                gone.start_ticks,
                store
                    .session_dir(42, 1, spur_core::step::STEP_BATCH)
                    .join("runtime.sock"),
                std::path::PathBuf::new(),
            ))
            .expect("publish the session the supervisor left behind");
        store
            .obligations(42, 1, spur_core::step::STEP_BATCH)
            .append(&crate::stepd::StepdObligation::ExitObserved {
                exit_code: 3,
                signal: 0,
            })
            .expect("record the exit the supervisor left behind");

        report_runs_nothing_can_speak_for(
            &store,
            &admissions,
            &allocation,
            &controller_addr,
            "test-node",
        )
        .await;
        assert!(
            reports.lock().expect("completion reports").is_empty(),
            "an exit left on disk belongs to the session that recorded it"
        );

        replay_unacknowledged_stepd_completions(
            &store,
            &admissions,
            &allocation,
            &controller_addr,
            "test-node",
        )
        .await
        .expect("replay");

        let sent = reports.lock().expect("completion reports").clone();
        assert_eq!(sent.len(), 1, "the session reports it, and only once");
        assert_eq!(
            sent[0].exit_code, 3,
            "the recorded status, not a synthetic loss"
        );
    }

    #[test]
    fn a_liveness_read_that_could_not_complete_never_proves_a_supervisor_gone() {
        use crate::stepd::StepdLiveness;
        assert_eq!(liveness_of(&[Ok(StepdLiveness::Stale)]), Liveness::Gone);
        assert_eq!(
            liveness_of(&[]),
            Liveness::CannotTell,
            "nothing read proves nothing"
        );
        assert_eq!(
            liveness_of(&[Ok(StepdLiveness::Stale), Ok(StepdLiveness::Live)]),
            Liveness::Live
        );
        assert_eq!(
            liveness_of(&[
                Ok(StepdLiveness::Stale),
                Err(std::io::Error::other("proc unreadable")),
            ]),
            Liveness::CannotTell,
            "an unreadable process is not a gone one"
        );
    }

    /// A controller that lost its state answers NotFound forever. Treating that
    /// as unfinished business held the cores of a job nothing would ever ack.
    #[tokio::test]
    async fn a_completion_the_controller_has_no_record_of_frees_the_slice() {
        let (controller_addr, reports) =
            spawn_mock_controller_answering(Some(tonic::Code::NotFound));
        let state = tempfile::tempdir().expect("runtime state directory");
        let spool = tempfile::tempdir().expect("admission state directory");
        let store = crate::stepd::StepdStore::new(state.path());
        let admissions = crate::admission::AdmissionStore::new(spool.path(), "test-node");

        let allocation = fence_a_slice_holding_run(controller_addr, &store, &admissions).await;

        assert_eq!(
            reports.lock().expect("completion reports").len(),
            1,
            "the death still has to be reported before anything is concluded"
        );
        assert_eq!(
            allocation.lock().await.free_cpus(),
            4,
            "a run the controller has no record of must not hold cores forever"
        );
        assert_eq!(
            pending_replays(&store),
            0,
            "replaying a report to a controller that has no record of it is futile"
        );
    }

    // A supervisor that ran its hook before dying still said how it ended. Taken
    // as unknowable, that answer never reaches the record the gate reads.
    #[tokio::test]
    async fn fencing_keeps_the_epilog_outcome_the_dead_supervisor_recorded() {
        let (controller_addr, _reports) = spawn_mock_controller();
        let state = tempfile::tempdir().expect("runtime state directory");
        let spool = tempfile::tempdir().expect("admission state directory");
        let store = crate::stepd::StepdStore::new(state.path());
        let admissions = crate::admission::AdmissionStore::new(spool.path(), "test-node");
        store
            .prepare_session_dir(42, 7, spur_core::step::STEP_BATCH)
            .expect("session dir");
        let obligations = store.obligations(42, 7, spur_core::step::STEP_BATCH);
        obligations
            .append(&crate::stepd::StepdObligation::ExitObserved {
                exit_code: 0,
                signal: 9,
            })
            .expect("record the observed exit");
        obligations
            .append(&crate::stepd::StepdObligation::EpilogCompleted { failed: true })
            .expect("record the failed epilog");

        let allocation = fence_a_slice_holding_run(controller_addr, &store, &admissions).await;

        assert_eq!(
            admissions
                .load_run(key(42, 7))
                .expect("record")
                .cleanup
                .epilog,
            crate::admission::HookState::Failed,
            "the ledger's own word on the hook must survive the fence"
        );
        assert_eq!(
            allocation.lock().await.free_cpus(),
            4,
            "a hook that has answered holds nothing, so the cores still go back"
        );
    }

    /// The other half of the invariant: only the controller's own definite "no
    /// such job" settles a run. Silence must leave the slice exactly as it was.
    #[tokio::test]
    async fn an_unanswered_completion_still_holds_the_slice() {
        let state = tempfile::tempdir().expect("runtime state directory");
        let spool = tempfile::tempdir().expect("admission state directory");
        let store = crate::stepd::StepdStore::new(state.path());
        let admissions = crate::admission::AdmissionStore::new(spool.path(), "test-node");

        let allocation =
            fence_a_slice_holding_run("http://127.0.0.1:1".into(), &store, &admissions).await;

        assert_eq!(
            allocation.lock().await.free_cpus(),
            2,
            "an unreachable controller is not evidence that it stopped tracking the run"
        );
        assert_eq!(pending_replays(&store), 1);
    }

    #[tokio::test]
    async fn fencing_leaves_an_unacknowledged_completion_for_the_retry_loop() {
        let state = tempfile::tempdir().expect("runtime state directory");
        let store = crate::stepd::StepdStore::new(state.path());

        fence_against_controller("http://127.0.0.1:1".into(), &store).await;

        assert_eq!(
            pending_replays(&store),
            1,
            "a report the controller never took must stay replayable"
        );
    }

    #[tokio::test]
    async fn liveness_watchdog_skips_a_session_someone_else_already_resolved() {
        let state = tempfile::tempdir().expect("runtime state directory");
        let store = crate::stepd::StepdStore::new(state.path());
        let pid = std::process::id();
        let mut descriptor = crate::stepd::StepdDescriptor::new(
            42,
            7,
            spur_core::step::STEP_BATCH,
            pid,
            crate::stepd::process_start_ticks(pid).expect("start ticks") + 1,
            store
                .session_dir(42, 7, spur_core::step::STEP_BATCH)
                .join("runtime.sock"),
            std::path::PathBuf::new(),
        );
        descriptor.capability = "test-capability".into();
        store.publish(&descriptor).expect("publish descriptor");

        let running = new_running_jobs();
        let allocation = Arc::new(Mutex::new(NodeAllocation::new(
            "test-node".into(),
            &ResourceSet::default(),
        )));
        // Nothing tracked under job_id 42: a completion push already won the race.
        let sessions = Arc::new(Mutex::new(HashMap::new()));

        fence_dead_stepd(
            &fence_context(
                &running,
                &allocation,
                &sessions,
                &crate::step_completion::StepCompletions::new(),
                &store,
            ),
            descriptor,
        )
        .await;

        assert_eq!(
            store
                .observed_exit(42, 7, spur_core::step::STEP_BATCH)
                .expect("read exit"),
            None
        );
    }

    #[tokio::test]
    async fn liveness_watchdog_cleans_up_the_orphaned_cgroup() {
        let state = tempfile::tempdir().expect("runtime state directory");
        let store = crate::stepd::StepdStore::new(state.path());
        let cgroup_root = tempfile::tempdir().expect("cgroup root");
        // Named as the real thing: cleanup refuses a path that is not ours.
        let cgroup = TestCgroup::new(&cgroup_root);
        let pid = std::process::id();
        let mut descriptor = crate::stepd::StepdDescriptor::new(
            42,
            7,
            spur_core::step::STEP_BATCH,
            pid,
            crate::stepd::process_start_ticks(pid).expect("start ticks") + 1,
            store
                .session_dir(42, 7, spur_core::step::STEP_BATCH)
                .join("runtime.sock"),
            cgroup.path().to_path_buf(),
        );
        descriptor.capability = "test-capability".into();
        store.publish(&descriptor).expect("publish descriptor");

        let running = new_running_jobs();
        let allocation = Arc::new(Mutex::new(NodeAllocation::new(
            "test-node".into(),
            &ResourceSet::default(),
        )));
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        sessions
            .lock()
            .await
            .insert(stepd_key(&descriptor), descriptor.clone());

        fence_dead_stepd(
            &fence_context(
                &running,
                &allocation,
                &sessions,
                &crate::step_completion::StepCompletions::new(),
                &store,
            ),
            descriptor,
        )
        .await;

        assert!(
            !cgroup.path().exists(),
            "an orphaned cgroup left by a crashed session must be cleaned up"
        );
        // Cgroup reaping happens before tracking is released (not after), so
        // a new attempt can never be handed this node's resources while the
        // old orphan might still be occupying them.
        assert!(!sessions
            .lock()
            .await
            .contains_key(&(42, spur_core::step::STEP_BATCH)));
    }

    async fn completion_listener_fixture_for_step(
        controller_addr: &str,
        step_id: spur_core::step::StepId,
    ) -> (
        CompletionListenerContext,
        RunningJobs,
        Arc<Mutex<StepdMap>>,
        tempfile::TempDir,
        crate::step_completion::StepCompletions,
    ) {
        let running = new_running_jobs();
        let completions = crate::step_completion::StepCompletions::new();
        let allocation = Arc::new(Mutex::new(NodeAllocation::new(
            "test-node".into(),
            &ResourceSet {
                cpus: 2,
                memory_mb: 1024,
                ..Default::default()
            },
        )));
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        let mut descriptor = crate::stepd::StepdDescriptor::new(
            42,
            7,
            step_id,
            0,
            0,
            std::path::PathBuf::from("/tmp/runtime.sock"),
            std::path::PathBuf::new(),
        );
        descriptor.capability = "test-capability".into();
        allocation
            .lock()
            .await
            .allocate_for_job(42, 1, 1, 128, &[])
            .expect("reserve allocation");
        assert!(allocation.lock().await.commit_job(42, 1));
        let mut tracked = TrackedJob::dummy(0);
        tracked.run_attempt = 7;
        running.lock().await.insert(42, tracked);
        sessions
            .lock()
            .await
            .insert(stepd_key(&descriptor), descriptor);
        let state_dir = tempfile::tempdir().expect("state dir");
        let context = CompletionListenerContext {
            admissions: crate::admission::AdmissionStore::new(
                std::env::temp_dir().join(format!("spur-test-adm-{}", uuid::Uuid::new_v4())),
                "test-node",
            ),
            running: running.clone(),
            step_completions: completions.clone(),
            allocation,
            stepds: sessions.clone(),
            stepds_store: crate::stepd::StepdStore::new(state_dir.path()),
            controller_addr: controller_addr.into(),
            hostname: "test-node".into(),
        };
        (context, running, sessions, state_dir, completions)
    }

    /// A tempdir-backed cgroup with a realistic `job_<id>_<attempt>` name, so
    /// reaping it exercises the same path a real one would.
    struct TestCgroup {
        _root: (),
        path: std::path::PathBuf,
    }

    impl TestCgroup {
        fn new(root: &tempfile::TempDir) -> Self {
            let path = root.path().join("job_42_1");
            std::fs::create_dir(&path).expect("job cgroup");
            Self { _root: (), path }
        }

        fn path(&self) -> &std::path::Path {
            &self.path
        }
    }

    fn plain_job_entry() -> crate::job_entry::JobEntry {
        crate::job_entry::JobEntry {
            pid: 0,
            has_pid_namespace: false,
            has_user_namespace: false,
            has_mount_namespace: false,
            uid: 1000,
            gid: 1000,
            work_dir: "/tmp".into(),
            cgroup_path: None,
        }
    }

    #[test]
    fn a_plain_host_step_is_supervisable() {
        assert!(step_can_be_supervised(""));
    }

    #[test]
    fn a_step_building_its_own_container_is_not_supervisable() {
        assert!(!step_can_be_supervised("docker://alpine"));
    }

    fn proto_pmix_plan(
        job_id: u32,
        step_id: u32,
        local_count: u32,
    ) -> spur_proto::proto::PmixLaunchPlan {
        spur_proto::proto::PmixLaunchPlan {
            job_id,
            step_id,
            namespace: String::new(),
            universe_size: local_count,
            task_offset: 0,
            local_procs: (0..local_count)
                .map(|local_rank| spur_proto::proto::PmixLocalProc {
                    rank: local_rank,
                    local_rank,
                })
                .collect(),
            tmpdir: "/tmp/spur-pmix".into(),
            job_uid: 1000,
            job_gid: 1000,
            num_nodes: 1,
            node_index: 0,
            peer_hosts: Vec::new(),
            modex_connect_timeout_secs: 5,
            modex_fence_timeout_secs: 60,
            modex_verify_timeout_secs: 5,
        }
    }

    // The controller keys the plan; the launching step is the authority. A
    // disagreement puts the server and its peers on different modex ports.
    #[test]
    fn a_supervised_steps_plan_is_rekeyed_to_the_step_that_launches_it() {
        let plan = supervised_step_pmix(&proto_pmix_plan(42, 0, 2), MpiConfig::default(), 7, 2)
            .expect("a well-formed plan")
            .plan;

        assert_eq!(plan.step_id, 7);
        assert_eq!(plan.namespace, "spur.42.7");
    }

    // The supervisor sizes the rank wrapper from the plan, so a disagreement
    // here would start the wrong number of ranks with nothing to report it.
    #[test]
    fn a_supervised_steps_plan_must_agree_with_the_launch_on_rank_count() {
        let error = supervised_step_pmix(&proto_pmix_plan(42, 0, 2), MpiConfig::default(), 7, 4)
            .expect_err("the plan carries two ranks, the launch asks for four");

        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
        assert!(
            error.message().contains("2 local ranks"),
            "unexpected error: {}",
            error.message()
        );
    }

    /// An agent that takes the supervised dispatch, with the MPI plugin named
    /// explicitly so an outcome never depends on what the runner has installed.
    fn supervised_agent(pmix_plugin: &str) -> AgentService {
        AgentService::with_cluster_config(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            &spur_core::config::ClusterConfig::default(),
            spur_core::config::JobLimits {
                memlock: spur_core::config::MemlockLimit::Unlimited,
            },
            CgroupConfig {
                enabled: false,
                ..CgroupConfig::default()
            },
            MpiConfig {
                pmix_plugin: pmix_plugin.into(),
                ..MpiConfig::default()
            },
            new_running_jobs(),
            spur_core::config::AuthConfig::default().allow_root_jobs,
        )
        .with_root_override(false)
        .with_runtime_state_dir(
            std::env::temp_dir().join(format!("spur-test-runtime-{}", uuid::Uuid::new_v4())),
        )
        .with_supervised_launch()
    }

    /// Pins the attempt the supervised dispatch carries. It keys the session
    /// directory and the cgroup leaf, so 0 escapes the job's enforced limits.
    #[tokio::test]
    async fn a_supervised_step_takes_the_attempt_of_the_job_it_joins() {
        let svc = supervised_agent("/nonexistent/spur/spur_mpi_pmix.so");
        let job_id = 7755;
        let step_id = 3;
        svc.insert_test_job(
            job_id,
            TrackedJob {
                run_attempt: 5,
                ..TrackedJob::allocation_only(None)
            },
        )
        .await;

        // A session left by an earlier attempt of this step. Displacing it is
        // refused unless the launch names an attempt that supersedes it.
        let sockets = tempfile::tempdir().expect("socket dir");
        let displaced = crate::stepd::StepdDescriptor::new(
            job_id,
            3,
            step_id,
            0,
            0,
            sockets.path().join("displaced.sock"),
            std::path::PathBuf::new(),
        );
        svc.stepds
            .lock()
            .await
            .insert(stepd_key(&displaced), displaced);

        let work_dir = tempfile::tempdir().expect("work dir");
        let error = svc
            .run_command(Request::new(RunCommandRequest {
                command: vec!["true".into()],
                work_dir: work_dir.path().to_string_lossy().into_owned(),
                job_id,
                step_id,
                num_tasks: 1,
                ..Default::default()
            }))
            .await
            .expect_err("spurstepd is not built beside the test binary");

        assert!(
            !error
                .message()
                .contains("fence a displaced step supervisor"),
            "a step keyed to attempt 0 cannot supersede attempt 3, got: {}",
            error.message()
        );
        assert!(
            error.message().contains("step supervisor failed to start"),
            "the launch must reach the spawn, got: {}",
            error.message()
        );
    }

    // The supervised dispatch is unreachable without `with_supervised_launch`,
    // so this is the only unit-level cover for who hosts a step's PMIx server.
    #[tokio::test]
    async fn a_supervised_pmix_step_leaves_the_agent_hosting_no_server() {
        let svc = supervised_agent("/nonexistent/spur/spur_mpi_pmix.so");

        let job_id = 7754;
        svc.insert_test_job(job_id, TrackedJob::dummy(0)).await;
        let work_dir = tempfile::tempdir().unwrap();
        let step_id = 3;

        let req = Request::new(RunCommandRequest {
            command: vec!["true".into()],
            uid: 0,
            gid: 0,
            work_dir: work_dir.path().to_string_lossy().into_owned(),
            environment: HashMap::new(),
            job_id,
            step_id,
            num_tasks: 2,
            mpi: MPI_PMIX.into(),
            pmix_plan: Some(proto_pmix_plan(job_id, step_id, 2)),
            ..Default::default()
        });

        let error = svc
            .run_command(req)
            .await
            .expect_err("the plugin is absent");

        // The pre-flight only proves this node could serve the plan; validating
        // is not hosting, and the server still belongs to the supervisor.
        assert!(
            !svc.mpi_host.has_active_pmix(job_id, step_id),
            "a supervised step's server belongs to its supervisor, not the agent"
        );
        assert!(
            error.message().contains("MPI plugin not found"),
            "the operator's reason must reach the caller, got: {}",
            error.message()
        );
    }

    // The reason only reaches srun's terminal through the buffered RunStep
    // response, which srun suppresses whenever its live tail of the step's spool
    // file reached a clean eof. Rejecting before that file exists is what keeps
    // the tail from claiming the output.
    #[tokio::test]
    async fn a_rejected_pmix_step_never_opens_the_spool_that_would_swallow_its_reason() {
        let svc = supervised_agent("/nonexistent/spur/spur_mpi_pmix.so");

        let job_id = 7755;
        svc.insert_test_job(job_id, TrackedJob::dummy(0)).await;
        let work_dir = tempfile::tempdir().unwrap();
        let step_id = 4;
        // The spool path is keyed by job and step alone, so a file left by an
        // earlier run of this test would read as one this run opened.
        crate::executor::cleanup_step_spool(job_id, step_id);

        let req = Request::new(RunCommandRequest {
            command: vec!["true".into()],
            uid: 0,
            gid: 0,
            work_dir: work_dir.path().to_string_lossy().into_owned(),
            environment: HashMap::new(),
            job_id,
            step_id,
            num_tasks: 1,
            mpi: MPI_PMIX.into(),
            pmix_plan: Some(proto_pmix_plan(job_id, step_id, 1)),
            ..Default::default()
        });

        svc.run_command(req)
            .await
            .expect_err("the plugin is absent");

        for stderr in [false, true] {
            assert_eq!(
                crate::executor::existing_step_output_path(job_id, step_id, stderr),
                None,
                "the step was rejected before its spool file could be opened"
            );
        }
    }

    // The pre-flight is scoped to PMIx: a step that asked for no MPI must still
    // be handed to its supervisor rather than judged by the agent.
    #[tokio::test]
    async fn a_supervised_step_without_mpi_still_reaches_its_supervisor() {
        let svc = supervised_agent("/nonexistent/spur/spur_mpi_pmix.so");

        let job_id = 7756;
        svc.insert_test_job(job_id, TrackedJob::dummy(0)).await;
        let work_dir = tempfile::tempdir().unwrap();
        let step_id = 5;

        let req = Request::new(RunCommandRequest {
            command: vec!["true".into()],
            uid: 0,
            gid: 0,
            work_dir: work_dir.path().to_string_lossy().into_owned(),
            environment: HashMap::new(),
            job_id,
            step_id,
            num_tasks: 1,
            ..Default::default()
        });

        let error = svc
            .run_command(req)
            .await
            .expect_err("no spurstepd is built alongside the test binary");

        assert!(
            error.message().contains("step supervisor"),
            "a non-MPI step must not be stopped by the PMIx pre-flight, got: {}",
            error.message()
        );
    }

    // Concurrent steps of one job share a scratch dir, and a supervised step's
    // wrapper is written after its launch, so a shared name is a silent swap.
    #[test]
    fn concurrent_steps_on_one_node_get_their_own_scratch_scripts() {
        let (first_script, first_wrapper) = step_scratch_names(0, 1);
        let (second_script, second_wrapper) = step_scratch_names(0, 2);

        assert_ne!(first_script, second_script);
        assert_ne!(first_wrapper, second_wrapper);
        assert_ne!(first_script, first_wrapper);
    }

    #[test]
    fn a_plain_steps_script_runs_the_command_directly() {
        let script = supervised_step_script(&plain_job_entry(), 1000, 1000, &["true".to_string()])
            .expect("script");

        assert!(
            !script.contains("nsenter"),
            "nothing to enter without a namespaced parent:\n{script}"
        );
    }

    #[test]
    fn a_namespaced_steps_script_enters_the_parent_from_inside() {
        let mut entry = plain_job_entry();
        entry.has_pid_namespace = true;
        entry.has_mount_namespace = true;
        entry.pid = 4242;

        let script =
            supervised_step_script(&entry, 1000, 1000, &["true".to_string()]).expect("script");

        // Inside the script, so the supervisor itself stays out of the job's
        // namespaces and outlives it.
        assert!(script.contains("nsenter"), "{script}");
        assert!(script.contains("4242"), "{script}");
    }

    #[test]
    fn a_namespaced_parent_with_no_live_pid_runs_the_command_directly() {
        let mut entry = plain_job_entry();
        entry.has_pid_namespace = true;

        let script =
            supervised_step_script(&entry, 1000, 1000, &["true".to_string()]).expect("script");

        assert!(!script.contains("nsenter"), "{script}");
    }

    #[test]
    fn an_exit_code_survives_the_wait_encoding() {
        use std::os::unix::process::ExitStatusExt;
        let status = step_exit_status(crate::step_completion::StepOutcome {
            exit_code: 7,
            signal: 0,
        });

        assert_eq!(status.code(), Some(7));
        assert_eq!(status.signal(), None);
    }

    #[test]
    fn a_signalled_step_reports_its_signal_not_an_exit_code() {
        use std::os::unix::process::ExitStatusExt;
        let status = step_exit_status(crate::step_completion::StepOutcome {
            exit_code: 0,
            signal: nix::sys::signal::Signal::SIGTERM as i32,
        });

        assert_eq!(
            status.signal(),
            Some(nix::sys::signal::Signal::SIGTERM as i32)
        );
        assert_eq!(status.code(), None);
    }

    #[tokio::test]
    async fn a_user_steps_completion_wakes_the_rpc_that_launched_it() {
        const STEP: spur_core::step::StepId = 3;
        let (context, _running, _sessions, _state_dir, completions) =
            completion_listener_fixture_for_step("http://127.0.0.1:1", STEP).await;
        let mut waiter = completions.register(42, 7, STEP).await;
        let (server_stream, client_stream) = tokio::net::UnixStream::pair().expect("socket pair");
        let handler =
            tokio::spawn(
                async move { handle_completion_notification(server_stream, &context).await },
            );
        let (reader, mut writer) = client_stream.into_split();
        let notification = crate::stepd::AgentNotification::StepdCompleted {
            job_id: 42,
            run_attempt: 7,
            step_id: STEP,
            exit_code: 9,
            signal: 0,
            epilog_failed: false,
            capability: "test-capability".into(),
        };
        writer
            .write_all(&serde_json::to_vec(&notification).expect("encode notification"))
            .await
            .expect("write notification");
        writer.write_all(b"\n").await.expect("write newline");
        drop(writer);
        let mut reader = tokio::io::BufReader::new(reader);
        let mut line = String::new();
        crate::stepd::read_line_bounded(&mut reader, &mut line)
            .await
            .expect("read response");
        handler
            .await
            .expect("handler task")
            .expect("handle notification");

        // try_recv, not await: the handler has already returned, so a woken
        // waiter is ready now and an unwoken one fails instead of hanging.
        assert_eq!(
            waiter.try_recv().expect("the launching RPC must be woken"),
            crate::step_completion::StepOutcome {
                exit_code: 9,
                signal: 0,
            }
        );
    }

    async fn completion_listener_fixture(
        controller_addr: &str,
    ) -> (
        CompletionListenerContext,
        RunningJobs,
        Arc<Mutex<StepdMap>>,
        tempfile::TempDir,
    ) {
        let (context, running, sessions, state_dir, _) =
            completion_listener_fixture_for_step(controller_addr, spur_core::step::STEP_BATCH)
                .await;
        (context, running, sessions, state_dir)
    }

    #[tokio::test]
    async fn completion_notification_releases_local_tracking_even_when_controller_is_unreachable() {
        let (context, running, sessions, _state_dir) =
            completion_listener_fixture("http://127.0.0.1:1").await;
        let (server_stream, client_stream) = tokio::net::UnixStream::pair().expect("socket pair");
        let handler =
            tokio::spawn(
                async move { handle_completion_notification(server_stream, &context).await },
            );
        let (reader, mut writer) = client_stream.into_split();
        let notification = crate::stepd::AgentNotification::StepdCompleted {
            job_id: 42,
            run_attempt: 7,
            step_id: spur_core::step::STEP_BATCH,
            exit_code: 0,
            signal: 0,
            epilog_failed: false,
            capability: "test-capability".into(),
        };
        writer
            .write_all(&serde_json::to_vec(&notification).expect("encode notification"))
            .await
            .expect("write notification");
        writer.write_all(b"\n").await.expect("write newline");
        drop(writer);
        let mut reader = tokio::io::BufReader::new(reader);
        let mut line = String::new();
        crate::stepd::read_line_bounded(&mut reader, &mut line)
            .await
            .expect("read response");
        let response: crate::stepd::AgentNotificationResponse =
            serde_json::from_str(&line).expect("decode response");
        handler
            .await
            .expect("handler task")
            .expect("handle notification");

        assert_eq!(response, crate::stepd::AgentNotificationResponse::Deferred);
        assert!(
            !running.lock().await.contains_key(&42),
            "local tracking must be released regardless of controller reachability"
        );
        assert!(!sessions
            .lock()
            .await
            .contains_key(&(42, spur_core::step::STEP_BATCH)));
    }

    #[tokio::test]
    async fn an_unclaimed_completion_is_deferred_rather_than_acknowledged_away() {
        let (context, _running, sessions, _state_dir) =
            completion_listener_fixture("http://127.0.0.1:1").await;
        // Finished before the agent claimed its slot: nothing tracked locally,
        // but the exit is durably recorded and unreported.
        sessions
            .lock()
            .await
            .remove(&(42, spur_core::step::STEP_BATCH));
        let mut descriptor = crate::stepd::StepdDescriptor::new(
            42,
            7,
            spur_core::step::STEP_BATCH,
            0,
            0,
            std::path::PathBuf::from("/tmp/runtime.sock"),
            std::path::PathBuf::new(),
        );
        descriptor.capability = "test-capability".into();
        context
            .stepds_store
            .publish(&descriptor)
            .expect("publish descriptor");
        context
            .stepds_store
            .obligations(42, 7, spur_core::step::STEP_BATCH)
            .append(&crate::stepd::StepdObligation::ExitObserved {
                exit_code: 0,
                signal: 0,
            })
            .expect("record exit");

        let (server_stream, client_stream) = tokio::net::UnixStream::pair().expect("socket pair");
        let handler =
            tokio::spawn(
                async move { handle_completion_notification(server_stream, &context).await },
            );
        let (reader, mut writer) = client_stream.into_split();
        let notification = crate::stepd::AgentNotification::StepdCompleted {
            job_id: 42,
            run_attempt: 7,
            step_id: spur_core::step::STEP_BATCH,
            exit_code: 0,
            signal: 0,
            epilog_failed: false,
            capability: "test-capability".into(),
        };
        writer
            .write_all(&serde_json::to_vec(&notification).expect("encode notification"))
            .await
            .expect("write notification");
        writer.write_all(b"\n").await.expect("write newline");

        use tokio::io::AsyncBufReadExt;
        let mut reader = tokio::io::BufReader::new(reader);
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("read response");
        handler.await.expect("handler task").expect("handle push");
        assert_eq!(
            serde_json::from_str::<crate::stepd::AgentNotificationResponse>(&line)
                .expect("decode response"),
            crate::stepd::AgentNotificationResponse::Deferred
        );
    }

    #[tokio::test]
    async fn a_completion_push_racing_the_liveness_watchdog_never_double_reports() {
        let (context, _running, sessions, _state_dir) =
            completion_listener_fixture("http://127.0.0.1:1").await;
        let state = tempfile::tempdir().expect("runtime state directory");
        let store = crate::stepd::StepdStore::new(state.path());
        let mut descriptor = crate::stepd::StepdDescriptor::new(
            42,
            7,
            spur_core::step::STEP_BATCH,
            0,
            0,
            std::path::PathBuf::from("/tmp/runtime.sock"),
            std::path::PathBuf::new(),
        );
        descriptor.capability = "test-capability".into();
        store.publish(&descriptor).expect("publish descriptor");

        let (server_stream, client_stream) = tokio::net::UnixStream::pair().expect("socket pair");
        let handler =
            tokio::spawn(
                async move { handle_completion_notification(server_stream, &context).await },
            );
        let (reader, mut writer) = client_stream.into_split();
        let notification = crate::stepd::AgentNotification::StepdCompleted {
            job_id: 42,
            run_attempt: 7,
            step_id: spur_core::step::STEP_BATCH,
            exit_code: 0,
            signal: 0,
            epilog_failed: false,
            capability: "test-capability".into(),
        };
        writer
            .write_all(&serde_json::to_vec(&notification).expect("encode notification"))
            .await
            .expect("write notification");
        writer.write_all(b"\n").await.expect("write newline");

        // Wait for the push to actually claim the session (removing it from
        // `sessions`) before the watchdog races it, instead of guessing a delay.
        for _ in 0..1000 {
            if !sessions
                .lock()
                .await
                .contains_key(&(42, spur_core::step::STEP_BATCH))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            !sessions
                .lock()
                .await
                .contains_key(&(42, spur_core::step::STEP_BATCH)),
            "push must claim the session before the watchdog races it"
        );
        let running = new_running_jobs();
        let allocation = Arc::new(Mutex::new(NodeAllocation::new(
            "test-node".into(),
            &ResourceSet::default(),
        )));
        fence_dead_stepd(
            &fence_context(
                &running,
                &allocation,
                &sessions,
                &crate::step_completion::StepCompletions::new(),
                &store,
            ),
            descriptor,
        )
        .await;

        let mut reader = tokio::io::BufReader::new(reader);
        let mut line = String::new();
        crate::stepd::read_line_bounded(&mut reader, &mut line)
            .await
            .expect("read response");
        handler
            .await
            .expect("handler task")
            .expect("handle notification");

        assert_eq!(
            store.observed_exit(42, 7, spur_core::step::STEP_BATCH).expect("read exit"),
            None,
            "the watchdog must not write a synthetic exit once the push already claimed the session"
        );
    }

    #[tokio::test]
    async fn a_push_arriving_after_the_watchdog_already_fenced_it_just_acks() {
        let (context, running, sessions, _state_dir) =
            completion_listener_fixture("http://127.0.0.1:1").await;
        let descriptor = sessions
            .lock()
            .await
            .get(&(42, spur_core::step::STEP_BATCH))
            .cloned()
            .expect("fixture session");
        let state = tempfile::tempdir().expect("runtime state directory");
        let store = crate::stepd::StepdStore::new(state.path());
        store.publish(&descriptor).expect("publish descriptor");

        let allocation = Arc::new(Mutex::new(NodeAllocation::new(
            "test-node".into(),
            &ResourceSet::default(),
        )));
        fence_dead_stepd(
            &fence_context(
                &running,
                &allocation,
                &sessions,
                &crate::step_completion::StepCompletions::new(),
                &store,
            ),
            descriptor,
        )
        .await;
        assert!(!sessions
            .lock()
            .await
            .contains_key(&(42, spur_core::step::STEP_BATCH)));

        let (server_stream, client_stream) = tokio::net::UnixStream::pair().expect("socket pair");
        let handler =
            tokio::spawn(
                async move { handle_completion_notification(server_stream, &context).await },
            );
        let (reader, mut writer) = client_stream.into_split();
        let notification = crate::stepd::AgentNotification::StepdCompleted {
            job_id: 42,
            run_attempt: 7,
            step_id: spur_core::step::STEP_BATCH,
            exit_code: 0,
            signal: 0,
            epilog_failed: false,
            capability: "test-capability".into(),
        };
        writer
            .write_all(&serde_json::to_vec(&notification).expect("encode notification"))
            .await
            .expect("write notification");
        writer.write_all(b"\n").await.expect("write newline");
        drop(writer);
        let mut reader = tokio::io::BufReader::new(reader);
        let mut line = String::new();
        // If this reached report_completion it would block for seconds retrying
        // against the unreachable controller; a fast reply proves it did not.
        let read = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            crate::stepd::read_line_bounded(&mut reader, &mut line),
        )
        .await
        .expect("push must not retry against the controller once fenced")
        .expect("read response");
        let _ = read;
        let response: crate::stepd::AgentNotificationResponse =
            serde_json::from_str(&line).expect("decode response");
        handler
            .await
            .expect("handler task")
            .expect("handle notification");

        assert_eq!(
            response,
            crate::stepd::AgentNotificationResponse::Acknowledged
        );
    }

    #[tokio::test(start_paused = true)]
    async fn completion_notification_acks_immediately_when_nothing_is_tracked() {
        let (context, _running, sessions, _state_dir) =
            completion_listener_fixture("http://127.0.0.1:1").await;
        sessions
            .lock()
            .await
            .remove(&(42, spur_core::step::STEP_BATCH));
        let (server_stream, client_stream) = tokio::net::UnixStream::pair().expect("socket pair");
        let handler =
            tokio::spawn(
                async move { handle_completion_notification(server_stream, &context).await },
            );
        let (reader, mut writer) = client_stream.into_split();
        let notification = crate::stepd::AgentNotification::StepdCompleted {
            job_id: 42,
            run_attempt: 7,
            step_id: spur_core::step::STEP_BATCH,
            exit_code: 0,
            signal: 0,
            epilog_failed: false,
            capability: "test-capability".into(),
        };
        writer
            .write_all(&serde_json::to_vec(&notification).expect("encode notification"))
            .await
            .expect("write notification");
        writer.write_all(b"\n").await.expect("write newline");
        drop(writer);
        let mut reader = tokio::io::BufReader::new(reader);
        let mut line = String::new();
        crate::stepd::read_line_bounded(&mut reader, &mut line)
            .await
            .expect("read response");
        let response: crate::stepd::AgentNotificationResponse =
            serde_json::from_str(&line).expect("decode response");
        handler
            .await
            .expect("handler task")
            .expect("handle notification");

        assert_eq!(
            response,
            crate::stepd::AgentNotificationResponse::Acknowledged
        );
    }

    #[tokio::test(start_paused = true)]
    async fn completion_notification_rejects_a_capability_mismatch() {
        let (context, running, sessions, _state_dir) =
            completion_listener_fixture("http://127.0.0.1:1").await;
        let (server_stream, client_stream) = tokio::net::UnixStream::pair().expect("socket pair");
        let handler =
            tokio::spawn(
                async move { handle_completion_notification(server_stream, &context).await },
            );
        let (reader, mut writer) = client_stream.into_split();
        let notification = crate::stepd::AgentNotification::StepdCompleted {
            job_id: 42,
            run_attempt: 7,
            step_id: spur_core::step::STEP_BATCH,
            exit_code: 0,
            signal: 0,
            epilog_failed: false,
            capability: "forged-capability".into(),
        };
        writer
            .write_all(&serde_json::to_vec(&notification).expect("encode notification"))
            .await
            .expect("write notification");
        writer.write_all(b"\n").await.expect("write newline");
        drop(writer);
        let mut reader = tokio::io::BufReader::new(reader);
        let mut line = String::new();
        let read_result = crate::stepd::read_line_bounded(&mut reader, &mut line).await;

        assert!(
            read_result.is_err() || line.is_empty(),
            "a forged capability must not get a usable response"
        );
        assert!(handler.await.expect("handler task").is_err());
        assert!(
            running.lock().await.contains_key(&42),
            "a rejected notification must not release tracking for the real session"
        );
        assert!(sessions
            .lock()
            .await
            .contains_key(&(42, spur_core::step::STEP_BATCH)));
    }

    #[tokio::test(start_paused = true)]
    async fn completion_notification_from_a_superseded_attempt_does_not_release_the_current_one() {
        let (context, running, sessions, _state_dir) =
            completion_listener_fixture("http://127.0.0.1:1").await;
        // A redispatch bumped this job to run_attempt 8 after the fixture's
        // run_attempt-7 session was tracked; the old attempt's own (valid,
        // but now-stale) capability must not be able to touch the new one.
        let current = sessions
            .lock()
            .await
            .get(&(42, spur_core::step::STEP_BATCH))
            .cloned()
            .expect("fixture session");
        let mut newer = current.clone();
        newer.run_attempt = 8;
        newer.capability = "newer-capability".into();
        sessions
            .lock()
            .await
            .insert(stepd_key(&newer), newer.clone());

        let (server_stream, client_stream) = tokio::net::UnixStream::pair().expect("socket pair");
        let handler =
            tokio::spawn(
                async move { handle_completion_notification(server_stream, &context).await },
            );
        let (reader, mut writer) = client_stream.into_split();
        let notification = crate::stepd::AgentNotification::StepdCompleted {
            job_id: 42,
            run_attempt: 7,
            step_id: spur_core::step::STEP_BATCH,
            exit_code: 0,
            signal: 0,
            epilog_failed: false,
            capability: current.capability.clone(),
        };
        writer
            .write_all(&serde_json::to_vec(&notification).expect("encode notification"))
            .await
            .expect("write notification");
        writer.write_all(b"\n").await.expect("write newline");
        drop(writer);
        let mut reader = tokio::io::BufReader::new(reader);
        let mut line = String::new();
        crate::stepd::read_line_bounded(&mut reader, &mut line)
            .await
            .expect("read response");
        let response: crate::stepd::AgentNotificationResponse =
            serde_json::from_str(&line).expect("decode response");
        handler
            .await
            .expect("handler task")
            .expect("handle notification");

        assert_eq!(
            response,
            crate::stepd::AgentNotificationResponse::Acknowledged,
            "a stale attempt's own report is harmless to acknowledge"
        );
        assert!(
            running.lock().await.contains_key(&42),
            "the current attempt's tracking must survive a superseded attempt's report"
        );
        assert_eq!(
            sessions
                .lock()
                .await
                .get(&(42, spur_core::step::STEP_BATCH)),
            Some(&newer)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn notify_agent_completion_gives_up_after_the_retry_budget() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("nobody-listens.sock");
        let notification = crate::stepd::AgentNotification::StepdCompleted {
            job_id: 42,
            run_attempt: 7,
            step_id: spur_core::step::STEP_BATCH,
            exit_code: 0,
            signal: 0,
            epilog_failed: false,
            capability: "test-capability".into(),
        };
        let response = crate::stepd::notify_agent_completion(&socket_path, &notification).await;
        assert_eq!(response, None);
    }

    #[tokio::test]
    async fn completion_notification_round_trips_over_a_real_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("agent.sock");
        let (context, running, _sessions, _state_dir) =
            completion_listener_fixture("http://127.0.0.1:1").await;
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("bind socket");
        tokio::spawn(serve_completion_notifications(listener, context));
        let notification = crate::stepd::AgentNotification::StepdCompleted {
            job_id: 42,
            run_attempt: 7,
            step_id: spur_core::step::STEP_BATCH,
            exit_code: 0,
            signal: 0,
            epilog_failed: false,
            capability: "test-capability".into(),
        };
        let response = crate::stepd::notify_agent_completion(&socket_path, &notification).await;
        assert_eq!(
            response,
            Some(crate::stepd::AgentNotificationResponse::Deferred)
        );
        assert!(!running.lock().await.contains_key(&42));
    }

    /// A namespaced job whose pid cannot exist, so `/proc/<pid>/...` probes and
    /// `/proc/<pid>/environ` reads resolve the same way on every host.
    fn dead_pid_job_entry(uid: u32, gid: u32) -> crate::job_entry::JobEntry {
        crate::job_entry::JobEntry {
            pid: i32::MAX,
            ..nsenter_job_entry(uid, gid)
        }
    }

    fn nsenter_job_entry(uid: u32, gid: u32) -> crate::job_entry::JobEntry {
        crate::job_entry::JobEntry {
            pid: 1234,
            has_pid_namespace: true,
            has_user_namespace: false,
            has_mount_namespace: true,
            uid,
            gid,
            work_dir: "/home/user".into(),
            cgroup_path: None,
        }
    }

    fn terminal_job_facts() -> TerminalJobFacts {
        TerminalJobFacts {
            run_attempt: 3,
            cpus: 4,
            memory_mb: 2048,
            gpu_devices: vec![1],
            partition: "batch".into(),
            nodelist: "node-a".into(),
        }
    }

    fn terminal_config(
        entry: &crate::job_entry::JobEntry,
        step_id: u32,
        winsize: Option<crate::pty::WindowSize>,
    ) -> executor::JobLaunchConfig {
        terminal_launch_config(
            entry,
            TerminalIdentity {
                job_id: 42,
                step_id,
                node: "node-a".into(),
            },
            &["/bin/bash".to_string()],
            winsize,
            terminal_job_facts(),
            spur_core::config::MemlockLimit::Unlimited,
            CgroupConfig::default(),
        )
        .expect("a terminal launch config")
    }

    // An interactive shell is a supervised launch on a terminal, so the size
    // the client asked for has to survive into the launch config.
    #[test]
    fn a_terminal_launches_on_a_pty_sized_the_way_the_client_asked() {
        let winsize = crate::pty::WindowSize {
            rows: 50,
            cols: 200,
            xpixel: 0,
            ypixel: 0,
        };
        let config = terminal_config(&nsenter_job_entry(1000, 1000), 7, Some(winsize));

        assert_eq!(config.io_mode, executor::LaunchIo::Pty(Some(winsize)));
    }

    // A reserved step id sends setup_cgroup down the job-creation branch, where
    // it would claim the job node the allocation already owns.
    #[test]
    fn a_terminal_lands_in_a_cgroup_leaf_below_the_job() {
        let config = terminal_config(&nsenter_job_entry(1000, 1000), 7, None);
        let job = executor::reapable_cgroup_path(
            config.job_id,
            config.run_attempt,
            spur_core::step::STEP_BATCH,
        );
        let leaf =
            executor::reapable_cgroup_path(config.job_id, config.run_attempt, config.step_id);

        assert_ne!(leaf, job, "a terminal must not claim the job's own node");
        assert_eq!(
            leaf.parent(),
            Some(job.as_path()),
            "a terminal's cgroup must sit under the job's"
        );
        assert_eq!(config.run_attempt, terminal_job_facts().run_attempt);
    }

    #[test]
    fn a_terminal_into_a_namespaced_job_enters_it_rather_than_unsharing_again() {
        let config = terminal_config(&nsenter_job_entry(1000, 1000), 7, None);

        assert!(config.joins_parent_namespaces);
        assert!(
            config.script.contains("nsenter"),
            "the terminal must enter the job: {}",
            config.script
        );
    }

    #[test]
    fn a_terminal_on_a_plain_job_runs_the_shell_without_nsenter() {
        let entry = crate::job_entry::JobEntry {
            pid: 0,
            has_pid_namespace: false,
            has_user_namespace: false,
            has_mount_namespace: false,
            uid: 1000,
            gid: 1000,
            work_dir: "/home/user".into(),
            cgroup_path: None,
        };
        let config = terminal_config(&entry, 7, None);

        assert!(!config.joins_parent_namespaces);
        assert!(!config.script.contains("nsenter"), "{}", config.script);
        assert!(config.script.contains("/bin/bash"), "{}", config.script);
    }

    // spurd's own environment may hold daemon secrets, so a session starts from
    // the job's and nothing else.
    #[test]
    fn a_terminal_carries_the_jobs_environment_and_a_term() {
        let config = terminal_config(&dead_pid_job_entry(1000, 1000), 7, None);

        assert_eq!(
            config.environment.get("TERM").map(String::as_str),
            Some("xterm-256color")
        );
        assert_eq!(
            config.environment.get("SPUR_JOB_ID").map(String::as_str),
            Some("42")
        );
        assert_eq!(
            config.environment.get("SLURM_JOB_ID").map(String::as_str),
            Some("42")
        );
    }

    // An empty argv is the bare `srun --pty`; the shell is resolved inside the
    // job's mount namespace, not against the host's filesystem.
    #[test]
    fn a_session_with_no_command_resolves_a_shell() {
        let entry = dead_pid_job_entry(1000, 1000);
        assert_eq!(
            session_shell(&entry, &[]),
            vec!["/bin/sh".to_string()],
            "a namespace whose /proc root cannot be read falls back to sh"
        );
        assert_eq!(
            session_shell(&entry, &["id".to_string()]),
            vec!["id".to_string()]
        );
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
        // An interactive session passes the resolved shell as the command.
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
            cgroup_path: None,
        };
        let pd = crate::privdrop::PrivDrop::for_test(1000, 1000);
        let plan = build_launch_plan(&entry, Some(&pd), &["echo".to_string(), "hi".to_string()]);

        // No namespaces → spawn the command directly and drop privilege in the
        // child via pre_exec.
        assert_eq!(plan.program, "echo");
        assert_eq!(plan.args, vec!["hi".to_string()]);
        assert!(plan.apply_priv_in_child);
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
            cgroup_path: None,
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

    #[test]
    fn configured_runtime_state_dir_overrides_the_default() {
        let configured = std::path::PathBuf::from("/var/lib/spur-runtime-test");
        let service = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        )
        .with_runtime_state_dir(configured.clone());

        assert_eq!(service.stepd_state_dir, configured);
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
            std::path::PathBuf::from("/etc/wireguard"),
            new_running_jobs(),
            false,
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
                run_attempt: 1,
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
                run_attempt: 1,
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
                run_attempt: 1,
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
                run_attempt: 1,
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
                start_offset: 0,
                step: None,
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

    /// A restart leaves a supervised step running with nothing in `active_steps`
    /// to say so, so a tail that reconnects to it must take the adopted session
    /// as proof the step is live. Reading the empty spool as a finished step
    /// eofs the stream, and srun suppresses its buffered fallback on a clean
    /// eof, so everything the step prints afterwards is lost.
    #[tokio::test]
    async fn stream_job_output_keeps_tailing_a_step_the_restart_adopted() {
        use tokio_stream::StreamExt as _;
        let state_dir = tempfile::tempdir().expect("state dir");
        let svc = restarted_agent(state_dir.path()).await;
        let job_id = 44;
        let step_id = 3;
        let store = crate::stepd::StepdStore::new(state_dir.path());
        svc.adopt_stepds(&[publish_session(&store, step_id, None)])
            .await;

        // The spool file the step wrote before the restart, at the path the
        // agent rebuilds from the job and step ids alone.
        let dir = std::env::temp_dir()
            .join("spur")
            .join(format!("job{job_id}"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("step{step_id}.out"));
        std::fs::write(&path, b"before-restart\n").unwrap();

        let mut stream = svc
            .stream_job_output(Request::new(StreamJobOutputRequest {
                start_offset: 0,
                step: None,
                job_id,
                step_id,
                stream: "stdout".into(),
                user: "testuser".into(),
            }))
            .await
            .unwrap()
            .into_inner();

        let first = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("a chunk should arrive")
            .expect("stream still open")
            .unwrap();
        assert_eq!(first.data, b"before-restart\n");
        assert!(!first.eof);

        // Nothing more is due while the step is merely quiet, so a chunk
        // arriving here is the premature eof: it is the only thing the tail
        // sends when it reads the adopted step as finished. Asserting on the
        // silence is what makes this fail against that bug — waiting for the
        // next write instead lets the eof-and-final-read path serve it and pass.
        let quiet =
            tokio::time::timeout(std::time::Duration::from_millis(500), stream.next()).await;
        assert!(
            quiet.is_err(),
            "the tail ended an adopted step that is still running: {:?}",
            quiet.ok().flatten()
        );

        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(b"after-restart\n").unwrap();
        }

        let next = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("the tail must deliver what the step wrote after the restart")
            .expect("stream still open")
            .unwrap();
        assert_eq!(next.data, b"after-restart\n");
        assert!(!next.eof);

        let _ = std::fs::remove_file(&path);
    }

    /// An srun tail that lost its agent mid-step reconnects carrying the byte
    /// count it already printed. Honoring it is what keeps the reconnect from
    /// reprinting the step's early output; clamping a nonsensical offset back to
    /// the start is what keeps it from dropping output it never printed.
    #[tokio::test]
    async fn stream_job_output_resumes_a_reconnecting_reader_at_its_offset() {
        use tokio_stream::StreamExt as _;
        for (start_offset, expected) in [(6u64, &b"part2\n"[..]), (999, &b"part1\npart2\n"[..])] {
            let svc = AgentService::new(
                test_reporter(),
                HooksConfig::default(),
                Arc::new(Mutex::new(DeviceRegistry::new())),
                spur_core::config::MemlockLimit::Unlimited,
            );
            let job_id = 78;
            svc.insert_test_job(job_id, TrackedJob::dummy(std::process::id()))
                .await;

            // The step's spool survived the agent restart with all its output,
            // and the step has since finished (so the tail eofs promptly).
            let step_id = 6;
            let dir = std::env::temp_dir().join(format!(
                "spur-resume-test-{}-{start_offset}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("step6.out");
            std::fs::write(&path, b"part1\npart2\n").unwrap();
            svc.active_steps.lock().await.insert(
                (job_id, step_id),
                ActiveStep {
                    stdout_path: path.to_string_lossy().into_owned(),
                    ..Default::default()
                },
            );

            let mut stream = svc
                .stream_job_output(Request::new(StreamJobOutputRequest {
                    start_offset,
                    step: None,
                    job_id,
                    step_id,
                    stream: "stdout".into(),
                    user: "testuser".into(),
                }))
                .await
                .unwrap()
                .into_inner();

            // Let the tail resolve the spool file through active_steps (as it
            // does for a step the restarted agent has re-adopted) before the
            // step settles and the stream eofs.
            let first = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
                .await
                .expect("a chunk should arrive")
                .expect("stream still open")
                .unwrap();
            assert!(!first.eof);
            svc.active_steps.lock().await.remove(&(job_id, step_id));

            let mut resumed = first.data;
            loop {
                let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
                    .await
                    .expect("a chunk should arrive")
                    .expect("stream still open")
                    .unwrap();
                if chunk.eof {
                    break;
                }
                resumed.extend_from_slice(&chunk.data);
            }
            assert_eq!(resumed, expected, "start_offset {start_offset}");

            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn a_named_step_zero_is_not_the_jobs_own_output() {
        // step_id cannot express step 0: there, 0 has always meant the job.
        let named = spur_proto::proto::StreamJobOutputRequest {
            job_id: 1,
            step_id: 0,
            step: Some(0),
            ..Default::default()
        };
        let legacy = spur_proto::proto::StreamJobOutputRequest {
            job_id: 1,
            step_id: 0,
            step: None,
            ..Default::default()
        };

        assert_eq!(requested_step_of(&named), Some(0));
        assert_eq!(requested_step_of(&legacy), None);
    }

    #[test]
    fn a_legacy_client_still_names_its_step() {
        let legacy = spur_proto::proto::StreamJobOutputRequest {
            job_id: 1,
            step_id: 4,
            step: None,
            ..Default::default()
        };

        assert_eq!(requested_step_of(&legacy), Some(4));
    }

    fn namespace_test_config() -> executor::JobLaunchConfig {
        executor::JobLaunchConfig {
            job_id: 1,
            run_attempt: 1,
            step_id: spur_core::step::STEP_BATCH,
            joins_parent_namespaces: false,
            allocation_holder: false,
            script: String::new(),
            work_dir: "/tmp".into(),
            name: String::new(),
            user: "testuser".into(),
            node: "test-node".into(),
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
            uid: 1000,
            gid: 1000,
            container: None,
            prolog_script: None,
            partition: String::new(),
            nodelist: String::new(),
            mpi: String::new(),
            host_device_plan: None,
            memlock: spur_core::config::MemlockLimit::Unlimited,
            cgroup: spur_core::config::CgroupConfig::default(),
            io_mode: executor::LaunchIo::File,
            pmix_multi_task: false,
        }
    }

    // Reaping recurses and SIGKILLs, so doing it here would cut short the grace
    // period the supervisor's own teardown is in the middle of serving.
    #[test]
    fn a_cancel_leaves_a_supervised_jobs_cgroup_to_its_teardown() {
        assert_eq!(
            cgroup_to_reap_on_cancel(Some(7), false),
            Some(7),
            "an unsupervised allocation has nothing else to reap its cgroup"
        );
        assert_eq!(
            cgroup_to_reap_on_cancel(Some(7), true),
            None,
            "a supervised job's last step reaps the node after the grace period"
        );
        assert_eq!(cgroup_to_reap_on_cancel(None, false), None);
    }

    #[test]
    fn a_root_launch_lands_the_job_in_namespaces() {
        let ns = launch_namespaces(&namespace_test_config(), true);

        assert!(ns.pid && ns.mount, "a root launch unshares pid and mount");
        assert!(!ns.user, "no user namespace without a container");
    }

    // The launch skips unshare for these, so recording namespaces here would make
    // adoption re-enter ones that were never created.
    #[test]
    fn a_launch_that_skips_unshare_records_no_namespaces() {
        for cfg in [
            executor::JobLaunchConfig {
                pmix_multi_task: true,
                ..namespace_test_config()
            },
            executor::JobLaunchConfig {
                allocation_holder: true,
                ..namespace_test_config()
            },
            executor::JobLaunchConfig {
                joins_parent_namespaces: true,
                ..namespace_test_config()
            },
        ] {
            let ns = launch_namespaces(&cfg, true);
            assert!(
                !ns.pid && !ns.mount && !ns.user,
                "a launch that does not unshare must record no namespaces"
            );
        }
    }

    #[test]
    fn an_unprivileged_plain_launch_lands_nowhere() {
        let ns = launch_namespaces(&namespace_test_config(), false);

        assert!(!ns.pid && !ns.user && !ns.mount);
    }

    #[test]
    fn an_unprivileged_container_needs_a_user_namespace() {
        let container = crate::container::ContainerConfig {
            image: "test.sqsh".into(),
            mounts: Vec::new(),
            workdir: None,
            name: None,
            readonly: false,
            mount_home: false,
            remap_root: false,
            gpu_devices: Vec::new(),
            environment: HashMap::new(),
            container_env: HashMap::new(),
            entrypoint: None,
            uid: 1000,
            gid: 1000,
            username: "testuser".into(),
            home_dir: "/home/testuser".into(),
            device_plan: None,
        };
        let cfg = executor::JobLaunchConfig {
            container: Some(executor::ContainerLaunchConfig {
                config: container,
                rootfs: std::path::PathBuf::from("/tmp/rootfs"),
            }),
            ..namespace_test_config()
        };
        let ns = launch_namespaces(&cfg, false);

        assert!(ns.user, "rootless containers need one");
        assert!(ns.pid && ns.mount);
    }

    #[tokio::test]
    async fn an_adopted_job_keeps_the_namespaces_it_was_launched_into() {
        // Otherwise exec and attach run beside the job on the host instead of
        // inside it, outside its cgroup and device filter.
        let running = new_running_jobs();
        let mut descriptor = crate::stepd::StepdDescriptor::new(
            77,
            1,
            spur_core::step::STEP_BATCH,
            0,
            0,
            std::path::PathBuf::from("/tmp/unused.sock"),
            std::path::PathBuf::from("/sys/fs/cgroup/spur/job_77_1"),
        );
        descriptor.has_pid_namespace = true;
        descriptor.has_mount_namespace = true;

        recover_stepds(&running, vec![descriptor]).await;

        let jobs = running.lock().await;
        let adopted = jobs.get(&77).expect("adopted");
        assert!(adopted.has_pid_namespace);
        assert!(adopted.has_mount_namespace);
        assert_eq!(
            adopted.cgroup_path.as_deref(),
            Some(std::path::Path::new("/sys/fs/cgroup/spur/job_77_1"))
        );
    }

    #[tokio::test]
    async fn an_adopted_job_keeps_the_resources_a_step_launches_against() {
        // A step reads its GPU set from the job; recovering an empty one denies
        // the step every device instead of the ones the job holds.
        let running = new_running_jobs();
        let mut descriptor = crate::stepd::StepdDescriptor::new(
            78,
            1,
            spur_core::step::STEP_BATCH,
            0,
            0,
            std::path::PathBuf::from("/tmp/unused.sock"),
            std::path::PathBuf::new(),
        );
        descriptor.resources = crate::stepd::StepdJobResources {
            cpus: 8,
            memory_mb: 4096,
            gpu_devices: vec![2, 3],
            cpu_ids: vec![0, 1, 2, 3, 4, 5, 6, 7],
            partition: "gpu".into(),
            nodelist: "node-a,node-b".into(),
            mpi: "pmix".into(),
        };

        recover_stepds(&running, vec![descriptor]).await;

        let jobs = running.lock().await;
        let adopted = jobs.get(&78).expect("adopted");
        assert_eq!(adopted.gpu_devices, vec![2, 3]);
        assert_eq!(adopted.cpus, 8);
        assert_eq!(adopted.memory_mb, 4096);
        assert_eq!(adopted.partition, "gpu");
        assert_eq!(adopted.nodelist, "node-a,node-b");
        assert_eq!(adopted.mpi, "pmix");
    }

    #[tokio::test]
    async fn await_step_rejects_non_owner() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        svc.insert_test_job(44, TrackedJob::dummy(std::process::id()))
            .await;

        let err = svc
            .await_step(Request::new(AwaitStepRequest {
                job_id: 44,
                step_id: 0,
                user: "intruder".into(),
                run_attempt: 0,
            }))
            .await
            .expect_err("a non-owner must not await another user's step");

        assert_eq!(err.code(), tonic::Code::PermissionDenied);
    }

    #[tokio::test]
    async fn await_step_reports_a_step_this_node_does_not_run() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        // The job runs here; only the step is unknown, so the lookup under test
        // is reached rather than the job-access check in front of it.
        svc.insert_test_job(44, TrackedJob::dummy(std::process::id()))
            .await;

        let err = svc
            .await_step(Request::new(AwaitStepRequest {
                job_id: 44,
                step_id: 3,
                user: "testuser".into(),
                run_attempt: 0,
            }))
            .await
            .expect_err("an unknown step must not park the caller forever");

        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn await_step_returns_an_exit_recorded_while_nobody_was_listening() {
        let state_dir = tempfile::tempdir().expect("state dir");
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        )
        .with_runtime_state_dir(state_dir.path().to_path_buf());
        svc.insert_test_job(44, TrackedJob::dummy(std::process::id()))
            .await;
        let store = crate::stepd::StepdStore::new(state_dir.path());
        let mut descriptor = crate::stepd::StepdDescriptor::new(
            44,
            1,
            3,
            0,
            0,
            std::path::PathBuf::new(),
            std::path::PathBuf::new(),
        );
        descriptor.socket_path = store.session_dir(44, 1, 3).join("runtime.sock");
        store.publish(&descriptor).expect("publish descriptor");
        store
            .obligations(44, 1, 3)
            .append(&crate::stepd::StepdObligation::ExitObserved {
                exit_code: 5,
                signal: 0,
            })
            .expect("record the step's exit");
        svc.stepds
            .lock()
            .await
            .insert(stepd_key(&descriptor), descriptor);

        let response = svc
            .await_step(Request::new(AwaitStepRequest {
                job_id: 44,
                step_id: 3,
                user: "testuser".into(),
                run_attempt: 0,
            }))
            .await
            .expect("a recorded exit must be returned, not parked on");

        assert_eq!(response.into_inner().exit_code, 5);
    }

    /// A batch session as its supervisor published it, recording where the
    /// job's output landed so an agent that restarts can still find it.
    fn published_batch_descriptor(
        store: &crate::stepd::StepdStore,
        stdout_path: &str,
    ) -> crate::stepd::StepdDescriptor {
        let step_id = spur_core::step::STEP_BATCH;
        let mut descriptor = crate::stepd::StepdDescriptor::new(
            44,
            1,
            step_id,
            0,
            0,
            std::path::PathBuf::new(),
            std::path::PathBuf::new(),
        );
        descriptor.socket_path = store.session_dir(44, 1, step_id).join("runtime.sock");
        descriptor.owner = "testuser".into();
        descriptor.stdout_path = stdout_path.into();
        store.publish(&descriptor).expect("publish descriptor");
        descriptor
    }

    // Adoption rebuilds the tracked job from the descriptor alone; a field it
    // leaves at a default is one every consumer of `running` then reads wrong.
    #[tokio::test]
    async fn adoption_restores_the_output_paths_and_rootfs_mode_it_recorded() {
        let mut descriptor = crate::stepd::StepdDescriptor::new(
            44,
            1,
            spur_core::step::STEP_BATCH,
            0,
            0,
            std::path::PathBuf::new(),
            std::path::PathBuf::new(),
        );
        descriptor.stdout_path = "/spool/job44.out".into();
        descriptor.stderr_path = "/spool/job44.err".into();
        descriptor.container_rootfs_mode = Some(crate::container::RootfsMode::Overlay);

        let running = new_running_jobs();
        recover_stepds(&running, vec![descriptor]).await;

        let jobs = running.lock().await;
        let tracked = jobs.get(&44).expect("the adopted job is tracked");
        assert_eq!(tracked.stdout_path, "/spool/job44.out");
        assert_eq!(tracked.stderr_path, "/spool/job44.err");
        assert_eq!(
            tracked.rootfs_mode,
            crate::container::RootfsMode::Overlay,
            "guessing Extracted would skip the unmount and leak the overlay"
        );
    }

    // Sessions are adopted in directory order, so a job whose step supervisor
    // is enumerated first must still report the job's own output, not the step's.
    #[tokio::test]
    async fn a_steps_session_never_supplies_the_jobs_output_paths() {
        let mut step = crate::stepd::StepdDescriptor::new(
            44,
            1,
            0,
            0,
            0,
            std::path::PathBuf::new(),
            std::path::PathBuf::new(),
        );
        step.stdout_path = "/spool/job44/step0.out".into();
        step.stderr_path = "/spool/job44/step0.err".into();
        let mut batch = crate::stepd::StepdDescriptor::new(
            44,
            1,
            spur_core::step::STEP_BATCH,
            0,
            0,
            std::path::PathBuf::new(),
            std::path::PathBuf::new(),
        );
        batch.stdout_path = "/spool/job44.out".into();
        batch.stderr_path = "/spool/job44.err".into();
        batch.container_rootfs_mode = Some(crate::container::RootfsMode::Overlay);

        for order in [
            vec![step.clone(), batch.clone()],
            vec![batch.clone(), step.clone()],
        ] {
            let seen: Vec<_> = order.iter().map(|d| d.step_id).collect();
            let running = new_running_jobs();
            recover_stepds(&running, order).await;

            let jobs = running.lock().await;
            let tracked = jobs.get(&44).expect("the adopted job is tracked");
            assert_eq!(
                tracked.stdout_path, "/spool/job44.out",
                "a step's spool file is not the job's output (order {seen:?})"
            );
            assert_eq!(tracked.stderr_path, "/spool/job44.err", "order {seen:?}");
            assert_eq!(
                tracked.rootfs_mode,
                crate::container::RootfsMode::Overlay,
                "a step carries no rootfs mode and must not reset the job's (order {seen:?})"
            );
        }
    }

    #[tokio::test]
    async fn stream_job_output_tails_a_job_the_restart_adopted() {
        use tokio_stream::StreamExt as _;
        let state_dir = tempfile::tempdir().expect("state dir");
        let output = state_dir.path().join("job44.out");
        std::fs::write(&output, b"before-restart\n").expect("seed the job's output");
        let store = crate::stepd::StepdStore::new(state_dir.path());
        let descriptor = published_batch_descriptor(&store, &output.to_string_lossy());

        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        )
        .with_runtime_state_dir(state_dir.path().to_path_buf());
        recover_stepds(&svc.running, vec![descriptor.clone()]).await;
        svc.adopt_stepds(&[descriptor]).await;

        let mut stream = svc
            .stream_job_output(Request::new(StreamJobOutputRequest {
                start_offset: 0,
                step: None,
                job_id: 44,
                step_id: 0,
                stream: "stdout".into(),
                user: "testuser".into(),
            }))
            .await
            .expect("an adopted job's output must still be attachable")
            .into_inner();

        let first = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("a chunk should arrive")
            .expect("stream still open")
            .expect("chunk");
        assert_eq!(first.data, b"before-restart\n");
    }

    // Waiting forever on a path that cannot be resolved is the worst outcome:
    // the caller sees a banner, no output, no error, and never returns.
    #[tokio::test]
    async fn stream_job_output_refuses_a_job_whose_output_path_is_unknown() {
        let state_dir = tempfile::tempdir().expect("state dir");
        let store = crate::stepd::StepdStore::new(state_dir.path());
        let descriptor = published_batch_descriptor(&store, "");

        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        )
        .with_runtime_state_dir(state_dir.path().to_path_buf());
        recover_stepds(&svc.running, vec![descriptor.clone()]).await;
        svc.adopt_stepds(&[descriptor]).await;

        let status = svc
            .stream_job_output(Request::new(StreamJobOutputRequest {
                start_offset: 0,
                step: None,
                job_id: 44,
                step_id: 0,
                stream: "stdout".into(),
                user: "testuser".into(),
            }))
            .await
            .expect_err("an unresolvable output path must be an error, not a silent wait");
        assert_eq!(status.code(), tonic::Code::NotFound);
    }

    /// One session of job 44's second attempt, on disk the way a supervisor
    /// leaves it, with `exit` recorded when the supervisor observed one.
    fn publish_session(
        store: &crate::stepd::StepdStore,
        step_id: u32,
        exit: Option<i32>,
    ) -> crate::stepd::StepdDescriptor {
        let mut descriptor = crate::stepd::StepdDescriptor::new(
            44,
            1,
            step_id,
            0,
            0,
            std::path::PathBuf::new(),
            std::path::PathBuf::new(),
        );
        descriptor.socket_path = store.session_dir(44, 1, step_id).join("runtime.sock");
        store.publish(&descriptor).expect("publish descriptor");
        if let Some(exit_code) = exit {
            store
                .obligations(44, 1, step_id)
                .append(&crate::stepd::StepdObligation::ExitObserved {
                    exit_code,
                    signal: 0,
                })
                .expect("record the step's exit");
        }
        descriptor
    }

    /// A restarted agent serving job 44, whose batch supervisor came back and
    /// holds the allocation, so a step of it may still be awaited.
    async fn restarted_agent(state_dir: &std::path::Path) -> AgentService {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        )
        .with_runtime_state_dir(state_dir.to_path_buf());
        svc.insert_test_job(
            44,
            TrackedJob {
                run_attempt: 1,
                ..TrackedJob::dummy(std::process::id())
            },
        )
        .await;
        let store = crate::stepd::StepdStore::new(state_dir);
        let batch = publish_session(&store, spur_core::step::STEP_BATCH, None);
        svc.adopt_stepds(&[batch]).await;
        svc
    }

    #[tokio::test]
    async fn adoption_keeps_a_recorded_cgroup_only_while_it_still_exists() {
        let state_dir = tempfile::tempdir().expect("state dir");
        let svc = restarted_agent(state_dir.path()).await;
        let store = crate::stepd::StepdStore::new(state_dir.path());
        let live_cgroup = state_dir.path().join("live-cgroup");
        std::fs::create_dir_all(&live_cgroup).expect("create the surviving cgroup");

        let mut present = publish_session(&store, 5, None);
        present.cgroup_path = live_cgroup.clone();
        let mut vanished = publish_session(&store, 6, None);
        vanished.cgroup_path = state_dir.path().join("swept-cgroup");
        svc.adopt_stepds(&[present, vanished]).await;

        let sessions = svc.stepds.lock().await;
        assert_eq!(
            sessions
                .get(&(44, 5))
                .expect("the live session is adopted")
                .cgroup_path,
            live_cgroup,
            "a cgroup that still exists stays authoritative for teardown"
        );
        assert_eq!(
            sessions
                .get(&(44, 6))
                .expect("the stale session is adopted")
                .cgroup_path,
            std::path::PathBuf::new(),
            "a cgroup that is gone must not be read as a confirmed-empty one"
        );
    }

    #[tokio::test]
    async fn await_step_answers_a_recovery_that_consumed_the_exit_first() {
        let state_dir = tempfile::tempdir().expect("state dir");
        let svc = restarted_agent(state_dir.path()).await;
        let store = crate::stepd::StepdStore::new(state_dir.path());
        // The supervisor outlived its workload, so the restart adopts it live
        // and the recovery poll settles it before any re-attach lands.
        let descriptor = publish_session(&store, 3, Some(0));
        svc.adopt_stepds(std::slice::from_ref(&descriptor)).await;
        settle_recovered_stepd(
            &svc.running,
            &svc.allocation,
            &svc.stepds,
            &svc.step_completions,
            &store,
            &descriptor,
            0,
            0,
        )
        .await
        .expect("the recovery poll owns this session's completion");

        let response = svc
            .await_step(Request::new(AwaitStepRequest {
                job_id: 44,
                step_id: 3,
                user: "testuser".into(),
                run_attempt: 0,
            }))
            .await
            .expect("a step settled before the re-attach must still report its exit");

        assert_eq!(response.into_inner().exit_code, 0);
    }

    fn append_obligation(
        store: &crate::stepd::StepdStore,
        descriptor: &crate::stepd::StepdDescriptor,
        obligation: &crate::stepd::StepdObligation,
    ) {
        store
            .obligations(
                descriptor.job_id,
                descriptor.run_attempt,
                descriptor.step_id,
            )
            .append(obligation)
            .expect("append the session's obligation");
    }

    fn record_epilog_completed(
        store: &crate::stepd::StepdStore,
        descriptor: &crate::stepd::StepdDescriptor,
        failed: bool,
    ) {
        append_obligation(
            store,
            descriptor,
            &crate::stepd::StepdObligation::EpilogCompleted { failed },
        );
    }

    /// The batch step, the one whose supervisor actually runs the node epilog.
    fn recovered_batch_session(
        store: &crate::stepd::StepdStore,
        exit: Option<i32>,
    ) -> crate::stepd::StepdDescriptor {
        publish_session(store, spur_core::step::STEP_BATCH, exit)
    }

    #[test]
    fn a_recovered_session_with_no_recorded_exit_is_unfinished() {
        let state_dir = tempfile::tempdir().expect("state dir");
        let store = crate::stepd::StepdStore::new(state_dir.path());
        let descriptor = recovered_batch_session(&store, None);

        assert_eq!(
            recovered_session_progress(&store, &descriptor).expect("read the session's ledger"),
            RecoveredSession::Unfinished,
            "a session that recorded no exit has not reached its hook"
        );
    }

    #[test]
    fn a_recovered_session_is_not_reportable_while_its_epilog_runs() {
        let state_dir = tempfile::tempdir().expect("state dir");
        let store = crate::stepd::StepdStore::new(state_dir.path());
        let descriptor = recovered_batch_session(&store, Some(0));

        assert_eq!(
            recovered_session_progress(&store, &descriptor).expect("read the session's ledger"),
            RecoveredSession::Hooking,
            "the exit is recorded before the hook runs, so an exit alone must not be reported"
        );
    }

    #[test]
    fn a_recovered_session_reports_once_its_epilog_has_ended() {
        let state_dir = tempfile::tempdir().expect("state dir");
        let store = crate::stepd::StepdStore::new(state_dir.path());
        let descriptor = recovered_batch_session(&store, Some(4));
        record_epilog_completed(&store, &descriptor, false);

        assert_eq!(
            recovered_session_progress(&store, &descriptor).expect("read the session's ledger"),
            RecoveredSession::Ended {
                exit_code: 4,
                signal: 0
            },
            "a hook that has said how it ended is no longer on the cores"
        );
    }

    #[test]
    fn a_recovered_session_reports_an_epilog_that_failed() {
        let state_dir = tempfile::tempdir().expect("state dir");
        let store = crate::stepd::StepdStore::new(state_dir.path());
        let descriptor = recovered_batch_session(&store, Some(0));
        record_epilog_completed(&store, &descriptor, true);

        assert_eq!(
            recovered_session_progress(&store, &descriptor).expect("read the session's ledger"),
            RecoveredSession::Ended {
                exit_code: 0,
                signal: 0
            },
            "a failed hook has returned; holding its report would strand the run"
        );
    }

    #[test]
    fn an_epilog_recorded_before_a_newer_exit_does_not_report_that_exit() {
        let state_dir = tempfile::tempdir().expect("state dir");
        let store = crate::stepd::StepdStore::new(state_dir.path());
        let descriptor = recovered_batch_session(&store, Some(0));
        record_epilog_completed(&store, &descriptor, false);
        append_obligation(
            &store,
            &descriptor,
            &crate::stepd::StepdObligation::ExitObserved {
                exit_code: 9,
                signal: 0,
            },
        );

        assert_eq!(
            recovered_session_progress(&store, &descriptor).expect("read the session's ledger"),
            RecoveredSession::Hooking,
            "an older hook result cannot speak for an exit recorded after it"
        );
    }

    #[tokio::test]
    async fn await_step_reports_the_exit_a_stale_session_recorded() {
        let state_dir = tempfile::tempdir().expect("state dir");
        let svc = restarted_agent(state_dir.path()).await;
        let store = crate::stepd::StepdStore::new(state_dir.path());
        let descriptor = publish_session(&store, 3, Some(7));
        svc.settle_stale_stepds(&[descriptor]).await;

        let response = svc
            .await_step(Request::new(AwaitStepRequest {
                job_id: 44,
                step_id: 3,
                user: "testuser".into(),
                run_attempt: 0,
            }))
            .await
            .expect("a step that exited during the outage must still report its exit");

        assert_eq!(response.into_inner().exit_code, 7);
    }

    /// The startup replay reports a stale session's exit before anything else
    /// runs, so by the time it is settled the release is its last open
    /// obligation. Returns the descriptor and its directory.
    fn acknowledged_stale_session(
        store: &crate::stepd::StepdStore,
        exit_code: i32,
    ) -> (crate::stepd::StepdDescriptor, std::path::PathBuf) {
        let descriptor = publish_session(store, 3, Some(exit_code));
        store
            .obligations(44, 1, 3)
            .append(&crate::stepd::StepdObligation::CompletionAcknowledged)
            .expect("record the completion acknowledgement");
        (descriptor, store.session_dir(44, 1, 3))
    }

    fn age_beyond_retention(session_dir: &std::path::Path) {
        let aged = std::time::SystemTime::now()
            - crate::step_completion::SETTLED_RETENTION
            - std::time::Duration::from_secs(1);
        std::fs::OpenOptions::new()
            .append(true)
            .open(session_dir.join("obligations.jsonl"))
            .expect("open the obligation log")
            .set_modified(aged)
            .expect("age the obligation log");
    }

    #[tokio::test]
    async fn a_settled_session_is_kept_so_a_later_restart_can_still_answer() {
        let state_dir = tempfile::tempdir().expect("state dir");
        let svc = restarted_agent(state_dir.path()).await;
        let store = crate::stepd::StepdStore::new(state_dir.path());
        let (descriptor, session_dir) = acknowledged_stale_session(&store, 7);

        svc.settle_stale_stepds(&[descriptor]).await;

        assert!(
            session_dir.exists(),
            "a settled step's only surviving answer must outlive the report nobody reads"
        );
    }

    #[tokio::test]
    async fn a_settled_session_is_pruned_once_its_answer_has_aged_out() {
        let state_dir = tempfile::tempdir().expect("state dir");
        let svc = restarted_agent(state_dir.path()).await;
        let store = crate::stepd::StepdStore::new(state_dir.path());
        let (descriptor, session_dir) = acknowledged_stale_session(&store, 7);
        svc.settle_stale_stepds(&[descriptor]).await;
        age_beyond_retention(&session_dir);

        store.prune_finalized().expect("sweep finalized sessions");

        assert!(
            !session_dir.exists(),
            "a retained answer past its window must be swept, not left to accumulate"
        );
    }

    #[tokio::test]
    async fn await_step_answers_from_the_ledger_after_the_memo_is_gone() {
        let state_dir = tempfile::tempdir().expect("state dir");
        let store = crate::stepd::StepdStore::new(state_dir.path());
        let (descriptor, _) = acknowledged_stale_session(&store, 7);
        restarted_agent(state_dir.path())
            .await
            .settle_stale_stepds(&[descriptor])
            .await;
        // A second restart: this agent never saw the step settle, so only the
        // record the first one deliberately kept can answer for it.
        let svc = restarted_agent(state_dir.path()).await;

        let response = svc
            .await_step(Request::new(AwaitStepRequest {
                job_id: 44,
                step_id: 3,
                user: "testuser".into(),
                run_attempt: 0,
            }))
            .await
            .expect("a restart must not lose a step's exit");

        assert_eq!(response.into_inner().exit_code, 7);
    }

    #[tokio::test]
    async fn await_step_does_not_hand_a_caller_the_next_attempts_exit() {
        let state_dir = tempfile::tempdir().expect("state dir");
        let svc = restarted_agent(state_dir.path()).await;
        let store = crate::stepd::StepdStore::new(state_dir.path());

        // The redispatch after a requeue. Step ids restart, so attempt 2 has a
        // step 3 of its own, and its exit is the newest one on disk.
        let mut redispatch = crate::stepd::StepdDescriptor::new(
            44,
            2,
            3,
            0,
            0,
            Default::default(),
            Default::default(),
        );
        redispatch.socket_path = store.session_dir(44, 2, 3).join("runtime.sock");
        store.publish(&redispatch).expect("publish the redispatch");
        store
            .obligations(44, 2, 3)
            .append(&crate::stepd::StepdObligation::ExitObserved {
                exit_code: 9,
                signal: 0,
            })
            .expect("record the redispatch's exit");

        let err = svc
            .await_step(Request::new(AwaitStepRequest {
                job_id: 44,
                step_id: 3,
                user: "testuser".into(),
                run_attempt: 1,
            }))
            .await
            .expect_err("a superseded attempt must not be answered with the redispatch's exit");
        assert_eq!(err.code(), tonic::Code::NotFound);

        let response = svc
            .await_step(Request::new(AwaitStepRequest {
                job_id: 44,
                step_id: 3,
                user: "testuser".into(),
                run_attempt: 2,
            }))
            .await
            .expect("the attempt that ran it is still answerable");
        assert_eq!(response.into_inner().exit_code, 9);
    }

    #[tokio::test]
    async fn await_step_still_rejects_a_step_with_no_recorded_exit() {
        let state_dir = tempfile::tempdir().expect("state dir");
        let svc = restarted_agent(state_dir.path()).await;
        let store = crate::stepd::StepdStore::new(state_dir.path());
        // A supervisor that died before observing an exit: the outcome is
        // genuinely unknown, so it must not be answered as a success -- least
        // of all with the exit its sibling step happens to have left behind.
        let sibling = publish_session(&store, 4, Some(0));
        let descriptor = publish_session(&store, 3, None);
        svc.settle_stale_stepds(&[sibling, descriptor]).await;

        let err = svc
            .await_step(Request::new(AwaitStepRequest {
                job_id: 44,
                step_id: 3,
                user: "testuser".into(),
                run_attempt: 0,
            }))
            .await
            .expect_err("an unobserved exit must not be invented");

        assert_eq!(err.code(), tonic::Code::NotFound);
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

    // `spur exec` enters the job's namespaces; without this it keeps spurd's
    // cgroup and so escapes the job's device filter.
    #[tokio::test]
    async fn exec_into_a_job_joins_its_cgroup() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        // A plain file stands in for `cgroup.procs`: the child opens and writes
        // it exactly as it would the kernel's, so no root and no cgroupfs.
        let cgroup = tempfile::tempdir().expect("tempdir");
        std::fs::write(cgroup.path().join("cgroup.procs"), "").expect("cgroup.procs");
        svc.insert_test_job(
            46,
            TrackedJob::allocation_only(Some(cgroup.path().to_path_buf())),
        )
        .await;

        // `$$` is the exec'd shell's own pid, which is the pid the child wrote.
        let resp = svc
            .exec_in_job(Request::new(ExecInJobRequest {
                job_id: 46,
                command: vec!["sh".into(), "-c".into(), "echo $$".into()],
                user: "testuser".into(),
            }))
            .await
            .expect("the owner may exec into their own job")
            .into_inner();
        assert!(resp.success, "exec failed: {}", resp.stderr);

        let joined =
            std::fs::read_to_string(cgroup.path().join("cgroup.procs")).expect("read cgroup.procs");
        assert_eq!(
            joined.trim(),
            resp.stdout.trim(),
            "the exec'd child must join the job's cgroup"
        );
    }

    // run outside the job's device filter when `[cgroup] required` and the join fails.
    #[tokio::test]
    async fn a_required_join_that_cannot_land_aborts_the_exec() {
        let svc = AgentService::with_cluster_config(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            &spur_core::config::ClusterConfig::default(),
            spur_core::config::JobLimits::default(),
            CgroupConfig {
                enabled: false,
                required: true,
                ..CgroupConfig::default()
            },
            MpiConfig::default(),
            new_running_jobs(),
            false, // allow_root_jobs
        )
        .with_root_override(false);

        // A path with no cgroup.procs: the pre-exec join open fails.
        svc.insert_test_job(
            47,
            TrackedJob::allocation_only(Some("/nonexistent/spur-exec-required/job_47".into())),
        )
        .await;

        let err = svc
            .exec_in_job(Request::new(ExecInJobRequest {
                job_id: 47,
                command: vec!["true".into()],
                user: "testuser".into(),
            }))
            .await
            .expect_err("a required join that cannot land must fail the exec");
        assert_eq!(err.code(), tonic::Code::Internal);
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
    async fn start_job_rejects_a_non_controller_caller() {
        // Releasing the gate is what lets a job's workload run; a user token
        // reaching it directly would bypass the controller's commit entirely.
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        let mut req = Request::new(spur_proto::proto::AgentStartJobRequest {
            job_id: 48,
            run_attempt: 1,
        });
        req.extensions_mut().insert(user_identity("attacker"));
        let err = svc
            .start_job(req)
            .await
            .expect_err("a user token must not release a gated job");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
    }

    async fn svc_holding(descriptor: crate::stepd::StepdDescriptor) -> AgentService {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        svc.stepds
            .lock()
            .await
            .insert((descriptor.job_id, descriptor.step_id), descriptor);
        svc
    }

    fn gated_descriptor(
        job_id: u32,
        run_attempt: u32,
        step_id: spur_core::step::StepId,
    ) -> crate::stepd::StepdDescriptor {
        crate::stepd::StepdDescriptor::new(
            job_id,
            run_attempt,
            step_id,
            0,
            0,
            std::path::PathBuf::from("/tmp/unused.sock"),
            std::path::PathBuf::new(),
        )
    }

    fn controller_start(
        job_id: u32,
        run_attempt: u32,
    ) -> Request<spur_proto::proto::AgentStartJobRequest> {
        let mut req = Request::new(spur_proto::proto::AgentStartJobRequest {
            job_id,
            run_attempt,
        });
        req.extensions_mut().insert(controller_identity());
        req
    }

    #[tokio::test]
    async fn start_job_does_not_release_a_superseded_run() {
        // A late release for an earlier epoch would start a workload the
        // controller has already redispatched under a new one.
        let svc = svc_holding(gated_descriptor(49, 2, spur_core::step::STEP_BATCH)).await;

        let err = svc
            .start_job(controller_start(49, 1))
            .await
            .expect_err("an older epoch owns no supervisor here");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn start_job_ignores_a_jobs_numbered_steps() {
        // Only the job's own supervisor waits at a gate; a numbered step is
        // released as the agent launches it.
        let svc = svc_holding(gated_descriptor(50, 1, 0)).await;

        let err = svc
            .start_job(controller_start(50, 1))
            .await
            .expect_err("a numbered step is not a gated job");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

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
            run_attempt: 0,
        });
        req.extensions_mut().insert(user_identity("attacker"));
        let err = svc
            .cancel_job(req)
            .await
            .expect_err("a user token must not cancel jobs by dialing the agent directly");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
    }

    // The check inside teardown is only meaningful if a launch cannot land after it: the
    // id's cgroup, spool and rootfs are all derived names the successor would recreate.
    #[tokio::test]
    async fn teardown_cannot_start_while_a_launch_holds_the_job_id() {
        let svc = AgentService::new(
            test_reporter_with_gpus(&[0]),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        let job_id = 910;
        let dir = tempfile::tempdir().unwrap();
        let cgroup = dir.path().join("job_910");
        std::fs::create_dir(&cgroup).unwrap();
        let completed = completed_job(job_id, cgroup.clone());

        // Stands in for a launch of the same id: it owns the id's state until it is done.
        let launching = svc.lifecycle.acquire(job_id).await;

        let torn_down = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task = tokio::spawn({
            let (lifecycle, running, allocation, mpi_host) = (
                svc.lifecycle.clone(),
                svc.running.clone(),
                svc.allocation.clone(),
                svc.mpi_host.clone(),
            );
            let flag = Arc::clone(&torn_down);
            async move {
                teardown_completed_job(
                    &completed,
                    &lifecycle,
                    &running,
                    &allocation,
                    &mpi_host,
                    &svc.admissions(),
                    crate::admission::EpilogOwed::No,
                )
                .await;
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        });

        // Nudges rather than waits: the teardown body has no pending await of its own once
        // it holds the id, so if it were not parked on the id it would finish in these.
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert!(
            !torn_down.load(std::sync::atomic::Ordering::SeqCst),
            "teardown must wait rather than run while the launch owns the id"
        );
        assert!(
            cgroup.exists(),
            "teardown must not remove state the launch is building on"
        );

        drop(launching);
        task.await
            .expect("teardown runs once the launch releases the id");
        assert!(!cgroup.exists(), "teardown still runs after it waits");
    }

    /// The impersonation this gate exists to stop: `user` decides who owns the allocation and `uid`
    /// decides who its steps run as, so a caller who sets both reaches any account on the node.
    #[tokio::test]
    async fn register_job_allocation_rejects_a_non_controller_caller() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        let mut req = Request::new(RegisterJobAllocationRequest {
            run_attempt: 1,
            job_id: 49,
            uid: 1234,
            gid: 1234,
            user: "attacker".into(),
            cpus: 1,
            ..Default::default()
        });
        req.extensions_mut().insert(user_identity("attacker"));

        let err = svc
            .register_job_allocation(req)
            .await
            .expect_err("a user token must not register an allocation on the agent directly");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        assert!(
            svc.running.lock().await.is_empty(),
            "a refused registration must not leave a job the exec paths would trust"
        );
    }

    /// Driven through the real RPC: a refusal over resources this node already holds has to reach
    /// the controller as an answer it can classify, since a transport error carries no kind at all.
    #[tokio::test]
    async fn a_launch_refused_over_a_held_slice_answers_with_the_holder() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        // Job 99 holds the whole node and is not tracked as live: exactly the
        // claim the controller's own view has lost.
        {
            let mut alloc = svc.allocation.lock().await;
            alloc
                .allocate_for_job(99, 3, 4, 0, &[])
                .expect("fill the 4-cpu test node");
            alloc.commit_job(99, 3);
        }

        let refused = svc
            .launch_job(Request::new(LaunchJobRequest {
                run_attempt: 1,
                job_id: 7,
                spec: Some(JobSpec {
                    uid: 1000,
                    name: "j".into(),
                    script: "#!/bin/bash\ntrue\n".into(),
                    num_tasks: 1,
                    num_nodes: 1,
                    cpus_per_task: 2,
                    work_dir: std::env::temp_dir().to_string_lossy().into_owned(),
                    ..Default::default()
                }),
                ..Default::default()
            }))
            .await
            .expect("the refusal is an answer, not a transport failure")
            .into_inner();

        assert!(!refused.success);
        assert_eq!(
            refused.failure_kind,
            LaunchFailureKind::LaunchFailureLocalOverlap as i32,
            "a transport error reaches the controller with no kind, so no reconcile fires"
        );
        let conflict = refused
            .conflict
            .expect("the node knows which job holds the slice");
        assert_eq!((conflict.job_id, conflict.run_attempt), (99, 3));
        assert_eq!(conflict.cpu_ids, vec![0, 1, 2, 3]);
    }

    /// An unaccounted claim is only worth naming if it explains the shortfall. One holding no core
    /// does not explain a core shortfall, and naming it would send the controller after the wrong job.
    #[tokio::test]
    async fn a_core_shortfall_names_only_a_holder_that_holds_cores() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        svc.insert_test_job(99, TrackedJob::dummy(0)).await;
        {
            let mut alloc = svc.allocation.lock().await;
            // Unaccounted, but holds memory only: the lowest id, so the naive
            // scan would reach it first.
            alloc.allocate_for_job(50, 1, 0, 1_000, &[]).expect("held");
            alloc.commit_job(50, 1);
            // The cores are all held by a job the node is tracking as live.
            alloc.allocate_for_job(99, 1, 4, 0, &[]).expect("held");
            alloc.commit_job(99, 1);
        }

        let spec = JobSpec {
            cpus_per_task: 1,
            ..Default::default()
        };
        let refusal = held(
            svc.allocate_local_for_test(7, &spec, None)
                .await
                .expect_err("every core is held"),
        );

        assert_eq!(
            refusal.kind(),
            LaunchFailureKind::LaunchFailureResidualState,
            "job 50 holds no core, so it cannot be the claim behind a core shortfall"
        );
        assert!(refusal.conflict.is_none());
    }

    /// A conflict carries cores and devices, so it can never say whose memory is in the way. The
    /// node says that plainly rather than naming a job whose claim does not explain the shortfall.
    #[tokio::test]
    async fn a_memory_shortfall_refuses_without_naming_a_holder() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        // An unaccounted owner is present and still must not be named: it holds
        // one core, which says nothing about the memory that fell short.
        {
            let mut alloc = svc.allocation.lock().await;
            alloc
                .allocate_for_job(99, 1, 1, 8_000, &[])
                .expect("the 8192MB test node has room for this");
            alloc.commit_job(99, 1);
        }

        let spec = JobSpec {
            cpus_per_task: 1,
            ..Default::default()
        };
        let allocated = ResourceAllocations {
            cpus: 1,
            memory_mb: 4096,
            ..Default::default()
        };
        let refusal = held(
            svc.allocate_local_for_test(7, &spec, Some(&allocated))
                .await
                .expect_err("the node has no room for another 4096MB"),
        );

        assert_eq!(
            refusal.kind(),
            LaunchFailureKind::LaunchFailureResidualState
        );
        assert!(refusal.conflict.is_none());
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

        let refused = svc
            .launch_job(Request::new(LaunchJobRequest {
                run_attempt: 1,
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
            .expect("the refusal is an answer, not a transport failure")
            .into_inner();
        assert!(!refused.success);
        assert_eq!(
            refused.failure_kind,
            LaunchFailureKind::LaunchFailureTargetMismatch as i32
        );
        assert!(
            refused.conflict.is_none(),
            "a misaddressed launch says nothing about what this node holds"
        );
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
    async fn run_command_uses_step_nodelist_for_node_env() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        static NEXT_JOB_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(9000);
        let job_id = NEXT_JOB_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut tracked = TrackedJob::dummy(0);
        tracked.nodelist = "n1,n2,test-node".into();
        svc.insert_test_job(job_id, tracked).await;

        let req = Request::new(RunCommandRequest {
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf '%s %s %s %s %s' \"$SPUR_NODEID\" \"$SPUR_NNODES\" \"$SPUR_NODELIST\" \"$SPUR_JOB_NODELIST\" \"$SPUR_JOB_NUM_NODES\""
                    .into(),
            ],
            uid: 0,
            gid: 0,
            work_dir: String::new(),
            environment: HashMap::new(),
            job_id,
            nodelist: "test-node".into(),
            ..Default::default()
        });
        let resp = svc.run_command(req).await.unwrap().into_inner();
        assert_eq!(resp.exit_code, 0);
        assert_eq!(resp.stdout.trim(), "0 1 test-node n1,n2,test-node 3");
    }

    #[test]
    fn step_scripts_are_staged_at_their_container_paths() {
        let work = tempfile::TempDir::new().unwrap();
        let rootfs = tempfile::TempDir::new().unwrap();
        let dir = work.path().join(".spur_step_1");
        std::fs::create_dir(&dir).unwrap();
        let command = dir.join("cmd_0.sh");
        let wrapper = dir.join("wrapper_0.sh");
        std::fs::write(&command, "#!/bin/bash\necho staged\n").unwrap();
        std::fs::write(
            &wrapper,
            format!("#!/bin/bash\nbash {}\n", command.display()),
        )
        .unwrap();
        let scripts = StepScriptCleanup {
            dir,
            paths: vec![command.clone(), wrapper.clone()],
        };

        scripts.stage_in_rootfs(rootfs.path(), 0, 0).unwrap();

        for source in [&command, &wrapper] {
            let destination = rootfs.path().join(source.strip_prefix("/").unwrap());
            assert_eq!(
                std::fs::read_to_string(destination).unwrap(),
                std::fs::read_to_string(source).unwrap()
            );
        }
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
            run_attempt: 1,
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

        svc.graceful_cancel(77, 0).await;

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

    /// Unbound socket paths: the supervisors are unreachable on purpose, so a
    /// cancel fans out over them without the test hosting one.
    async fn track_test_stepds(
        svc: &AgentService,
        job_id: u32,
        run_attempt: u32,
        step_ids: &[spur_core::step::StepId],
        sockets: &std::path::Path,
    ) {
        for step_id in step_ids {
            let mut descriptor = crate::stepd::StepdDescriptor::new(
                job_id,
                run_attempt,
                *step_id,
                0,
                0,
                sockets.join(format!("{step_id}.sock")),
                std::path::PathBuf::new(),
            );
            descriptor.capability = "cancel-fanout-test".into();
            svc.stepds
                .lock()
                .await
                .insert(stepd_key(&descriptor), descriptor);
        }
    }

    /// The supervisor runs its own ordered shutdown, so the agent's escalation
    /// must not reach a step that has one.
    #[tokio::test]
    async fn a_cancel_leaves_a_supervised_step_to_its_own_supervisor() {
        use std::os::unix::process::CommandExt;
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        let state = tempfile::tempdir().expect("state dir");
        track_test_stepds(&svc, 79, 1, &[spur_core::step::STEP_BATCH, 4], state.path()).await;

        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("300");
        cmd.process_group(0);
        let mut child = cmd.spawn().expect("spawn sleep");
        svc.register_test_step(79, 4, Some(child.id())).await;

        svc.cancel_active_steps_for_job(79, nix::sys::signal::Signal::SIGTERM as i32)
            .await;

        assert!(
            !svc.step_cancel_requested(79, 4).await,
            "a supervised step's shutdown belongs to its supervisor"
        );
        assert!(
            child.try_wait().expect("try_wait").is_none(),
            "the agent must not signal a step its supervisor is shutting down"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    /// The job's own supervisor says nothing about a step that has none: a
    /// `--container-image` step still runs under the agent and must be reached.
    #[tokio::test]
    async fn a_supervised_batch_job_does_not_spare_its_unsupervised_step() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        svc.insert_test_job(
            80,
            TrackedJob {
                run_attempt: 1,
                ..TrackedJob::allocation_only(None)
            },
        )
        .await;
        let state = tempfile::tempdir().expect("state dir");
        track_test_stepds(&svc, 80, 1, &[spur_core::step::STEP_BATCH], state.path()).await;
        svc.register_test_step(80, 6, None).await;

        svc.graceful_cancel(80, 1).await;

        assert!(
            svc.step_cancel_requested(80, 6).await,
            "a batch job's supervisor does not own its unsupervised step"
        );
    }

    /// A `--container-image` step has no supervisor, so it runs under the agent
    /// beside supervised siblings. A cancel must reach both.
    #[tokio::test]
    async fn a_supervised_sibling_does_not_spare_an_unsupervised_step() {
        use std::os::unix::process::CommandExt;
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        svc.register_job_allocation(Request::new(RegisterJobAllocationRequest {
            job_id: 78,
            cpus: 1,
            run_attempt: 1,
            ..Default::default()
        }))
        .await
        .expect("register allocation");

        let state = tempfile::tempdir().expect("state dir");
        track_test_stepds(
            &svc,
            78,
            1,
            &[spur_core::step::STEP_EXTERN, 1],
            state.path(),
        )
        .await;

        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("300");
        cmd.process_group(0);
        let mut child = cmd.spawn().expect("spawn sleep");
        svc.register_test_step(78, 2, Some(child.id())).await;

        svc.graceful_cancel(78, 1).await;

        assert!(
            svc.step_cancel_requested(78, 2).await,
            "an unsupervised step must be cancelled beside its supervised sibling"
        );
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
        assert!(
            exited,
            "the unsupervised step's process must be signaled dead"
        );
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
        // only in the RPC response — this file is what StreamJobOutput follows.
        // step_id defaults to 0 here, so the file is step0.out.
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
            run_attempt: 0,
        }))
        .await
        .unwrap();
        assert!(svc.step_cancel_requested(10, 1).await);
    }

    #[tokio::test]
    async fn cancel_step_ignores_a_superseded_attempt() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        // The redispatch: step ids restart, so its step 1 wears the id the
        // superseded attempt's cancel names.
        let descriptor = crate::stepd::StepdDescriptor::new(
            10,
            4,
            1,
            0,
            0,
            Default::default(),
            Default::default(),
        );
        svc.stepds
            .lock()
            .await
            .insert(stepd_key(&descriptor), descriptor);
        svc.register_test_step(10, 1, None).await;

        svc.cancel_step(Request::new(CancelStepRequest {
            job_id: 10,
            step_id: 1,
            signal: 0,
            run_attempt: 3,
        }))
        .await
        .expect("a superseded cancel is dropped, not an error");

        assert!(
            !svc.step_cancel_requested(10, 1).await,
            "a cancel for a superseded attempt must not reach the redispatch's step"
        );
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
            run_attempt: 0,
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
            run_attempt: 0,
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

    // The cgroup-escape path relies on this: the wrapper's descendants hold the
    // user's work, and an escaped step is in no cgroup that could reach them.
    #[tokio::test]
    async fn kill_step_process_group_takes_descendants_with_the_wrapper() {
        use std::time::Duration;
        use tokio::io::AsyncReadExt;

        // `echo` runs after both forks, so a read of it proves the descendants exist.
        let mut child = tokio::process::Command::new("bash")
            .arg("-c")
            .arg("sleep 3600 & sleep 3600 & echo ready; wait")
            .process_group(0)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("failed to spawn process group");
        let mut stdout = child.stdout.take().expect("stdout was piped");
        let mut ready = [0u8; 6];
        stdout
            .read_exact(&mut ready)
            .await
            .expect("wrapper should report both children forked");

        kill_step_process_group(&mut child).await;

        // The `sleep`s inherited this pipe, so EOF means none of them is left.
        let mut remaining = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stdout.read_to_end(&mut remaining))
            .await
            .expect("descendants outlived the wrapper")
            .expect("read failed");
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
            .allocate_local_resources(7, 1, &spec, None, cpus, memory_mb)
            .await
            .expect("allocation succeeds");
        assert_eq!(result.cpu_ids, vec![0, 1, 2, 3]);
    }

    // An empty cpuset reserves nothing, so it is admitted on a full node and
    // only fails later, at dispatch confirmation, as a resource mismatch.
    #[tokio::test]
    async fn a_launch_that_would_reserve_no_cpu_is_refused_not_admitted() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        let spec = JobSpec::default();
        svc.allocation
            .lock()
            .await
            .allocate_for_job(1, 1, 4, 0, &[])
            .expect("fill the 4-cpu test node");

        let refused = svc
            .allocate_local_resources(7, 1, &spec, None, 0, 0)
            .await
            .expect_err("a launch reserving nothing must be refused");

        // A bad ask, not a claim this node is holding: nothing for the
        // controller to reconcile, so it stays a transport error.
        let LaunchRejection::Failed(status) = refused else {
            panic!("a zero-cpu ask is a bad launch, not an overlap: {refused:?}");
        };
        assert_eq!(status.code(), tonic::Code::ResourceExhausted);
        let reserved_anything = svc
            .allocation
            .lock()
            .await
            .release_job(ReleaseWarrant::controller_cancelled(key(7, 1)));
        assert!(
            !reserved_anything,
            "the refusal must not have reserved anything under job 7"
        );
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
            .allocate_local_resources(7, 1, &spec, Some(&allocated), cpus, memory_mb)
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
            std::path::PathBuf::from("/etc/wireguard"),
            new_running_jobs(),
            false,
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
            run_attempt: 1,
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
            run_attempt: 1,
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
            run_attempt: 1,
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
            run_attempt: 1,
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
            run_attempt: 1,
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
            run_attempt: 1,
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

    // The kind is derived from the conflict, so an overlap the controller is
    // told to look up can never arrive without the job to look up.
    #[test]
    fn a_refusal_that_names_nothing_never_claims_a_holder_to_look_up() {
        let held = AllocationResult {
            cpu_ids: vec![2, 3],
            gpu_ids: vec![1],
            memory_mb: 512,
        };
        let named = HeldRefusal::named(42, 7, &held).into_response();
        assert!(!named.success);
        assert_eq!(
            named.failure_kind,
            LaunchFailureKind::LaunchFailureLocalOverlap as i32
        );
        let conflict = named.conflict.expect("a named overlap must carry the job");
        assert_eq!((conflict.job_id, conflict.run_attempt), (42, 7));
        assert_eq!(conflict.cpu_ids, vec![2, 3]);
        assert_eq!(conflict.gpu_devices, vec![1]);

        let anonymous = HeldRefusal::unattributed("nothing to attribute").into_response();
        assert!(!anonymous.success);
        assert_eq!(
            anonymous.failure_kind,
            LaunchFailureKind::LaunchFailureResidualState as i32
        );
        assert!(anonymous.conflict.is_none());
    }

    #[test]
    fn the_completion_retry_is_capped_but_never_gives_up() {
        // Unbounded in attempts, bounded in rate: a long outage must not become
        // a hot loop, and no attempt count may end the obligation.
        let mut previous = std::time::Duration::ZERO;
        for attempt in 0..40u32 {
            let backoff = completion_backoff(attempt, 0);
            assert!(backoff >= previous || previous >= COMPLETION_RETRY_CAP);
            assert!(
                backoff <= COMPLETION_RETRY_CAP + std::time::Duration::from_secs(1),
                "attempt {attempt} backed off past the cap"
            );
            previous = backoff;
        }
        assert!(
            completion_backoff(1000, 0) <= COMPLETION_RETRY_CAP + std::time::Duration::from_secs(1)
        );
    }

    #[test]
    fn the_retry_is_jittered_so_nodes_do_not_all_return_together() {
        assert_ne!(completion_backoff(3, 1), completion_backoff(3, 2));
    }

    #[tokio::test]
    async fn a_step_exit_no_longer_frees_the_slice() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_state_dir(state.path()).await;
        {
            let mut alloc = svc.allocation.lock().await;
            alloc.allocate_for_job(7, 1, 2, 1000, &[]).unwrap();
            alloc.commit_job(7, 1);
        }
        let mut run = crate::admission::RunAdmission::new(
            7,
            1,
            "n1",
            crate::admission::AdmittedResources::default(),
            crate::admission::now_unix_ms(),
        );
        run.lifecycle_owner_step = Some(spur_core::step::STEP_BATCH);
        svc.admissions().admit_run(&run).expect("admit");

        // No acknowledgement yet: the exit tells this node the work stopped and
        // nothing else knows it.
        assert_eq!(
            release_acknowledged_allocation(
                &svc.allocation,
                &svc.admissions(),
                key(7, 1),
                spur_core::step::STEP_BATCH,
            )
            .await,
            ReleaseOutcome::NotDue
        );
        assert_eq!(svc.allocation.lock().await.free_cpus(), 6);

        svc.admissions()
            .record_controller_ack(key(7, 1), 42)
            .expect("ack");
        assert_eq!(
            release_acknowledged_allocation(
                &svc.allocation,
                &svc.admissions(),
                key(7, 1),
                spur_core::step::STEP_BATCH,
            )
            .await,
            ReleaseOutcome::Freed
        );
        assert_eq!(svc.allocation.lock().await.free_cpus(), 8);
    }

    // One owner nobody can name used to stop collection for the whole node, so
    // the spool grew for as long as that single job lasted.
    #[tokio::test]
    async fn an_unnameable_charge_does_not_stop_the_node_from_collecting() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_state_dir(state.path()).await;
        let admissions = svc.admissions();
        {
            let mut alloc = svc.allocation.lock().await;
            alloc
                .restore_for_job(88, 0, &[0, 1], 1000, &[])
                .expect("a charge that names no attempt");
        }

        let mut settled = crate::admission::RunAdmission::new(
            77,
            1,
            &svc.reporter.hostname,
            crate::admission::AdmittedResources::default(),
            crate::admission::now_unix_ms(),
        );
        settled.state = crate::admission::RunState::Cleaned;
        admissions.admit_run(&settled).expect("admit");

        let mut held = crate::admission::RunAdmission::new(
            88,
            4,
            &svc.reporter.hostname,
            crate::admission::AdmittedResources::default(),
            crate::admission::now_unix_ms(),
        );
        held.state = crate::admission::RunState::Cleaned;
        admissions.admit_run(&held).expect("admit");

        collect_settled_admissions(&svc.allocation, &admissions).await;

        assert!(
            admissions.load_run(key(77, 1)).is_err(),
            "an unrelated job's settled record must still be collected"
        );
        assert!(
            admissions.load_run(key(88, 4)).is_ok(),
            "the unnameable charge keeps its own records, its release unproven"
        );
    }

    // A completion the controller acknowledges late settles the record away from
    // the release. The sweep must not collect the slice's only instruction.
    #[tokio::test]
    async fn a_late_acknowledgement_still_gets_its_slice_back() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_state_dir(state.path()).await;
        let admissions = svc.admissions();
        {
            let mut alloc = svc.allocation.lock().await;
            alloc
                .allocate_for_job(77, 1, 2, 1000, &[])
                .expect("allocate");
            alloc.commit_job(77, 1);
        }
        let while_held = svc.allocation.lock().await.free_cpus();

        let mut run = crate::admission::RunAdmission::new(
            77,
            1,
            &svc.reporter.hostname,
            crate::admission::AdmittedResources::default(),
            crate::admission::now_unix_ms(),
        );
        run.lifecycle_owner_step = Some(spur_core::step::STEP_BATCH);
        admissions.admit_run(&run).expect("admit");
        let mut participant = crate::admission::ParticipantAdmission::new(
            77,
            1,
            spur_core::step::STEP_BATCH,
            &svc.reporter.hostname,
            crate::admission::AdmittedResources::default(),
        );
        participant.final_report.required = true;
        admissions
            .admit_participant(&participant)
            .expect("participant");

        // The controller is unreachable, so the report stays owed.
        collect_settled_admissions(&svc.allocation, &admissions).await;
        release_due_allocations(&svc.allocation, &admissions).await;
        assert!(
            admissions.load_run(key(77, 1)).is_ok(),
            "an owed report must keep its record"
        );
        assert_eq!(
            svc.allocation.lock().await.free_cpus(),
            while_held,
            "an unacknowledged completion must not free anything"
        );

        // The controller returns and the retry loop delivers. The slice comes
        // back here, on the path that settled it, not on some later sweep.
        assert!(
            settle_acknowledged_completion(
                &svc.allocation,
                &admissions,
                key(77, 1),
                spur_core::step::STEP_BATCH,
            )
            .await,
            "the late delivery must free the slice it settled"
        );
        assert_eq!(
            svc.allocation.lock().await.free_cpus(),
            while_held + 2,
            "the slice must come back on the delivery that settled the record"
        );

        // The record-driven safety net finds nothing left to do, and collects.
        release_due_allocations(&svc.allocation, &admissions).await;
        collect_settled_admissions(&svc.allocation, &admissions).await;
        assert!(
            admissions.load_run(key(77, 1)).is_err(),
            "and the released record must not become immortal"
        );
    }

    // A cancel is the controller resolving the hold. Nothing else clears one,
    // so without this the record outlives the claim it preserved.
    #[tokio::test]
    async fn cancelling_an_untracked_claim_settles_the_record_it_leaves_behind() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_state_dir(state.path()).await;
        let admissions = svc.admissions();

        {
            let mut alloc = svc.allocation.lock().await;
            alloc.allocate_for_job(6, 1, 2, 1000, &[]).unwrap();
            alloc.commit_job(6, 1);
        }
        let mut run = crate::admission::RunAdmission::new(
            6,
            1,
            &svc.reporter.hostname,
            crate::admission::AdmittedResources::default(),
            crate::admission::now_unix_ms(),
        );
        run.lifecycle_owner_step = Some(spur_core::step::STEP_BATCH);
        admissions.admit_run(&run).expect("admit");
        flag_unbacked_allocations(&[(6, 1)], &admissions).await;
        assert!(admissions
            .load_run(key(6, 1))
            .expect("record")
            .conflict_hold
            .is_some());

        let mut req = Request::new(AgentCancelJobRequest {
            job_id: 6,
            signal: 9,
            run_attempt: 1,
        });
        req.extensions_mut().insert(controller_identity());
        svc.cancel_job(req).await.expect("cancel");

        assert!(
            admissions
                .load_run(key(6, 1))
                .expect("record")
                .conflict_hold
                .is_none(),
            "a hold the controller has resolved must not outlive the cancel"
        );
        assert_eq!(
            admissions
                .sweep(crate::admission::now_unix_ms(), 3_600_000, &HashSet::new())
                .expect("sweep"),
            1,
            "a settled record must be collectable rather than immortal"
        );
    }

    /// Seed a record for `job_id` attempt 1 whose batch step ran `digest`,
    /// settled or still live, so the digest fence has something to compare to.
    fn seed_recorded_command(svc: &AgentService, job_id: u32, digest: &str, settled: bool) {
        let admissions = svc.admissions();
        let node = svc.reporter.hostname.clone();
        let mut run = crate::admission::RunAdmission::new(
            job_id,
            1,
            &node,
            crate::admission::AdmittedResources::default(),
            crate::admission::now_unix_ms(),
        );
        if settled {
            run.state = crate::admission::RunState::Cleaned;
            run.controller_ack.release_raft_index = Some(1);
        }
        admissions.admit_run(&run).expect("admit the run");
        let mut participant = crate::admission::ParticipantAdmission::new(
            job_id,
            1,
            spur_core::step::STEP_BATCH,
            &node,
            crate::admission::AdmittedResources::default(),
        );
        participant.command_digest = digest.into();
        admissions
            .admit_participant(&participant)
            .expect("admit the participant");
    }

    fn launch_of(digest: &str) -> crate::admission::LaunchFences {
        let now = crate::admission::now_unix_ms();
        crate::admission::LaunchFences {
            issued_at_unix_ms: now,
            expires_at_unix_ms: now + 60_000,
            command_digest: digest.into(),
        }
    }

    /// A controller that lost its state reissues job id 1, which can land on a
    /// node still holding the finished predecessor that wore that id.
    #[tokio::test]
    async fn a_settled_record_does_not_refuse_a_job_that_reuses_its_identity() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        )
        .with_runtime_state_dir(state.path());
        seed_recorded_command(&svc, 1, "the-dead-predecessor", true);

        assert_eq!(
            svc.check_launch_fences(
                1,
                1,
                spur_core::step::STEP_BATCH,
                &launch_of("the-fresh-job")
            ),
            None,
            "a dead predecessor's command says nothing about a new job's"
        );
    }

    /// The check the exclusion must not cost us: one live identity being asked
    /// to run two different commands is still a refusal.
    #[tokio::test]
    async fn a_live_record_still_refuses_a_second_command_for_one_identity() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        )
        .with_runtime_state_dir(state.path());
        seed_recorded_command(&svc, 1, "the-running-command", false);

        assert_eq!(
            svc.check_launch_fences(
                1,
                1,
                spur_core::step::STEP_BATCH,
                &launch_of("a-different-command")
            ),
            Some(crate::admission::LaunchRefusal::ConflictingDigest)
        );
    }

    #[tokio::test]
    async fn a_contested_gpu_is_refused_rather_than_taken_from_its_holder() {
        // The worst form of the inference: it did not merely free a claim it
        // could not account for, it handed the device to a different job.
        let state = tempfile::tempdir().expect("state dir");
        let svc = AgentService::new(
            test_reporter_with_gpus(&[0]),
            HooksConfig::default(),
            Arc::new(Mutex::new(test_gpu_registry())),
            spur_core::config::MemlockLimit::Unlimited,
        )
        .with_runtime_state_dir(state.path());

        // A committed holder of GPU 0 that the agent no longer tracks.
        {
            let mut alloc = svc.allocation.lock().await;
            alloc.allocate_for_job(1, 1, 1, 0, &[0]).unwrap();
            alloc.commit_job(1, 1);
        }

        let spec = JobSpec {
            gres: vec!["gpu:1".into()],
            cpus_per_task: 1,
            ..Default::default()
        };
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
        let allocated = ResourceAllocations {
            cpus: 1,
            memory_mb: 0,
            devices,
        };

        let refused = svc
            .allocate_local_for_test(2, &spec, Some(&allocated))
            .await;
        assert!(refused.is_err(), "the dispatch must be refused");
        assert_eq!(
            svc.free_gpu_count().await,
            0,
            "the holder's GPU must still be held"
        );
        assert_eq!(
            svc.allocation.lock().await.conflicting_owners(&[0]),
            vec![1],
            "job 1 must still own it"
        );
    }

    #[tokio::test]
    async fn an_untracked_claim_is_flagged_and_still_held() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = AgentService::new(
            test_reporter_with_gpus(&[0, 1]),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        )
        .with_runtime_state_dir(state.path());

        // job 1: tracked (live) and committed.
        svc.insert_test_job(1, TrackedJob::dummy(0)).await;
        // job 2: orphan — committed allocation but never entered `running`
        // (simulating a teardown path that dropped the job without releasing).
        {
            let mut alloc = svc.allocation.lock().await;
            alloc.allocate_for_job(1, 1, 2, 0, &[0]).unwrap();
            alloc.commit_job(1, 1);
            alloc.allocate_for_job(2, 1, 2, 0, &[1]).unwrap();
            alloc.commit_job(2, 1);
        }
        assert_eq!(svc.free_gpu_count().await, 0);

        // Both get a record, so the tracked job's untouched hold is a real
        // assertion rather than a file that was never written.
        let admissions = svc.admissions();
        for job_id in [1, 2] {
            admissions
                .admit_run(&crate::admission::RunAdmission::new(
                    job_id,
                    1,
                    &svc.reporter.hostname,
                    crate::admission::AdmittedResources::default(),
                    crate::admission::now_unix_ms(),
                ))
                .expect("admit");
        }

        let unbacked = {
            let jobs = svc.running.lock().await;
            let alloc = svc.allocation.lock().await;
            alloc.unbacked_claims(
                &jobs.keys().copied().collect(),
                std::time::Instant::now(),
                LAUNCHING_TTL,
            )
        };
        assert_eq!(unbacked, vec![(2, 1)]);
        flag_unbacked_allocations(&unbacked, &admissions).await;

        assert_eq!(
            svc.free_gpu_count().await,
            0,
            "a claim with no tracked job must be held, not reclaimed"
        );
        let flagged = admissions.load_run(key(2, 1)).expect("record");
        assert!(
            flagged.conflict_hold.is_some(),
            "it must be flagged for the controller to resolve"
        );
        assert!(
            admissions
                .load_run(key(1, 1))
                .expect("record")
                .conflict_hold
                .is_none(),
            "the tracked job must not be flagged"
        );

        // A held claim is never released, so a second pass must not rewrite the
        // record and bury the reason the first one recorded.
        flag_unbacked_allocations(&unbacked, &admissions).await;
        assert_eq!(
            admissions
                .load_run(key(2, 1))
                .expect("record")
                .conflict_hold,
            flagged.conflict_hold
        );
        assert_eq!(svc.free_gpu_count().await, 0);
    }

    #[derive(Clone, Default)]
    struct CapturingWriter(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CapturingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturingWriter {
        type Writer = CapturingWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    // start_monitor's real completion tick must release a job's allocation
    // before reconcile can see it as parentless, or reconcile misclassifies
    // every ordinary completion as an orphan and logs it as one.
    #[tokio::test]
    async fn start_monitor_does_not_log_an_orphan_reclaim_for_an_ordinary_completion() {
        let log = CapturingWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(log.clone())
            .with_ansi(false)
            .finish();
        let _trace_guard = tracing::subscriber::set_default(subscriber);

        let svc = AgentService::new(
            test_reporter_with_gpus(&[0]),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        let job_id = 961;
        {
            let mut alloc = svc.allocation.lock().await;
            alloc.allocate_for_job(job_id, 1, 1, 0, &[0]).unwrap();
            alloc.commit_job(job_id, 1);
        }
        let child = tokio::process::Command::new("/bin/true")
            .process_group(0)
            .spawn()
            .expect("spawn short-lived job");
        let mut tracked = TrackedJob::dummy(0);
        tracked.job = executor::RunningJob::Managed { child };
        svc.insert_test_job(job_id, tracked).await;

        svc.start_monitor("http://127.0.0.1:1".into());
        assert!(
            wait_job_reaped(&svc, job_id, 5_000).await,
            "monitor should reap the exited job within 5s"
        );

        // The monitor reaps the job, but the controller here is unreachable
        // (port 1), so no acknowledgement arrives and the slice stays held.
        assert_eq!(
            svc.free_gpu_count().await,
            0,
            "an unacknowledged completion must not free the slice"
        );
        let output = String::from_utf8_lossy(&log.0.lock().unwrap()).into_owned();
        assert!(
            !output.contains("reconciled orphaned"),
            "an ordinary completion must not be reported as an orphan reclaim: {output}"
        );
    }

    // A conflicting owner no longer in `running` is stale and must be reclaimed
    // so the dispatch succeeds instead of stranding the node.
    #[tokio::test]
    async fn dispatch_is_refused_when_an_untracked_owner_holds_the_gpu() {
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
            alloc.allocate_for_job(99, 1, 1, 0, &[0]).unwrap();
            alloc.commit_job(99, 1);
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
            res.is_err(),
            "the agent cannot tell a finished owner from an unobserved one, so it must refuse"
        );
        assert_eq!(svc.free_gpu_count().await, 0, "the holder keeps its GPU");
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
            alloc.allocate_for_job(99, 1, 1, 0, &[0]).unwrap();
            alloc.commit_job(99, 1);
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
        let refusal =
            held(res.expect_err("must reject: the conflicting GPU owner is still running"));
        let conflict = refusal.conflict.expect("the holder is nameable");
        assert_eq!((conflict.job_id, conflict.run_attempt), (99, 1));
        assert_eq!(conflict.gpu_devices, vec![0]);
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
            alloc.allocate_for_job(99, 1, 1, 0, &[0]).unwrap();
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
        let refusal =
            held(res.expect_err("must reject: the conflicting GPU owner is still launching"));
        // A launch in flight is a live duplicate, not a claim to adjudicate, so
        // the node names nobody and the controller looks at the whole ledger.
        assert_eq!(
            refusal.kind(),
            LaunchFailureKind::LaunchFailureResidualState
        );

        // The launching owner must NOT have been reclaimed.
        assert_eq!(
            svc.free_gpu_count().await,
            0,
            "launching owner must be spared"
        );
    }

    fn held(rejection: LaunchRejection) -> HeldRefusal {
        match rejection {
            LaunchRejection::Held(refusal) => refusal,
            LaunchRejection::Failed(status) => {
                panic!("expected a refusal naming what the node holds, got {status:?}")
            }
        }
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
    async fn dispatch_is_refused_when_several_untracked_owners_hold_the_gpus() {
        let svc = AgentService::new(
            test_reporter_with_gpus(&[0, 1]),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        {
            let mut alloc = svc.allocation.lock().await;
            alloc.allocate_for_job(98, 1, 1, 0, &[0]).unwrap();
            alloc.commit_job(98, 1);
            alloc.allocate_for_job(99, 1, 1, 0, &[1]).unwrap();
            alloc.commit_job(99, 1);
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
        assert!(res.is_err(), "neither holder may be displaced");
        assert_eq!(svc.free_gpu_count().await, 0);
    }

    // A dispatch spanning an untracked GPU and a running one must reject.
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
            alloc.allocate_for_job(98, 1, 1, 0, &[0]).unwrap();
            alloc.commit_job(98, 1);
            alloc.allocate_for_job(99, 1, 1, 0, &[1]).unwrap();
            alloc.commit_job(99, 1);
        }

        let spec = JobSpec {
            cpus_per_task: 1,
            gres: vec!["gpu:2".into()],
            ..Default::default()
        };
        let res = svc
            .allocate_local_for_test(100, &spec, Some(&gpu_alloc_request(&[0, 1])))
            .await;
        let refusal = held(res.expect_err("must reject: GPU 1's owner is still running"));
        let conflict = refusal.conflict.expect("the stale holder is nameable");
        assert_eq!(
            conflict.job_id, 98,
            "the unaccounted holder is the drift; the running one is the controller's own doing"
        );
    }

    // A registered srun allocation must be committed AND tracked in `running`
    // so a reconcile pass spares it — the reservation is backed, not orphaned.
    #[tokio::test]
    async fn register_job_allocation_survives_reconcile() {
        let svc = AgentService::new(
            test_reporter_with_gpus(&[0]),
            HooksConfig::default(),
            // Registration resolves the granted GPU against this registry and
            // hard-fails when it cannot, so device 0 has to be known here.
            Arc::new(Mutex::new(test_gpu_registry())),
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
            run_attempt: 1,
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

        // The job is in `running`, so the sweep must not report it at all.
        {
            let jobs = svc.running.lock().await;
            let alloc = svc.allocation.lock().await;
            assert!(alloc
                .unbacked_claims(
                    &jobs.keys().copied().collect(),
                    std::time::Instant::now(),
                    LAUNCHING_TTL
                )
                .is_empty());
        }
        assert_eq!(
            svc.free_gpu_count().await,
            0,
            "a committed and tracked allocation must survive the sweep"
        );
    }

    // The refusal must undo the whole registration, not just return an error:
    // a committed reservation with nothing in `running` reads as live to the
    // node and as orphaned to the reconcile pass.
    #[tokio::test]
    async fn refusing_an_allocation_releases_it_and_drops_the_tracked_job() {
        let svc = AgentService::new(
            test_reporter_with_gpus(&[0]),
            HooksConfig::default(),
            Arc::new(Mutex::new(test_gpu_registry())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        // The state the handler is in when the cgroup setup fails: reserved,
        // committed, guard armed, nothing recorded yet.
        {
            let mut alloc = svc.allocation.lock().await;
            alloc.allocate_for_job(42, 1, 1, 0, &[0]).unwrap();
            alloc.commit_job(42, 1);
        }
        let reservation =
            LaunchReservationGuard::new(svc.allocation.clone(), svc.admissions(), key(42, 1));
        assert_eq!(svc.free_gpu_count().await, 0, "reservation holds the GPU");

        let status = {
            let mut jobs = svc.running.lock().await;
            // Inserted so the rollback has something to undo.
            jobs.insert(42, TrackedJob::allocation_only(None));
            let (status, cgroup) =
                refuse_allocation(42, reservation, &mut jobs, "cgroup root unavailable");
            assert!(
                !jobs.contains_key(&42),
                "a refused allocation must leave no tracked job"
            );
            drop(jobs);
            drop(cgroup);
            status
        };

        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(status.message().contains("cgroup root unavailable"));
        assert_eq!(
            svc.free_gpu_count().await,
            1,
            "a refusal must release the allocation it could not enforce"
        );
    }

    // The heartbeat's held-job source must report an allocation-only (srun/salloc)
    // job so the controller can reconcile it — the strand this fix addresses.
    /// The two trees a restart reads: `runtime/` for liveness, `admission/` for
    /// entitlement. Built with a real store so the replay walks real files.
    async fn svc_with_state_dir(state: &std::path::Path) -> AgentService {
        let running = new_running_jobs();
        let reporter = Arc::new(NodeReporter::new(
            "n1".into(),
            "http://localhost:6817".into(),
            ResourceSet {
                cpus: 8,
                memory_mb: 8192,
                gpus: (0..2)
                    .map(|device_id| spur_core::resource::GpuResource {
                        device_id,
                        gpu_type: "test".into(),
                        memory_mb: 1024,
                        peer_gpus: Vec::new(),
                        link_type: spur_core::resource::GpuLinkType::PCIe,
                    })
                    .collect(),
                ..Default::default()
            },
            spur_net::NodeAddress {
                ip: "127.0.0.1".into(),
                hostname: "n1".into(),
                port: 6818,
                source: spur_net::AddressSource::Static,
            },
            std::collections::HashMap::new(),
            String::new(),
            String::new(),
            std::path::PathBuf::from("/etc/wireguard"),
            running.clone(),
            false,
        ));
        AgentService::with_cluster_config(
            reporter,
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            &spur_core::config::ClusterConfig::default(),
            spur_core::config::JobLimits::default(),
            CgroupConfig {
                enabled: false,
                ..CgroupConfig::default()
            },
            MpiConfig::default(),
            running,
            false,
        )
        .with_runtime_state_dir(state)
    }

    async fn svc_with_epilog(state: &std::path::Path) -> AgentService {
        let mut svc = svc_with_state_dir(state).await;
        svc.hooks = Arc::new(HooksConfig {
            epilog: Some("/bin/true".into()),
            ..HooksConfig::default()
        });
        svc
    }

    fn an_allocation_only_job(run_attempt: u32) -> TrackedJob {
        TrackedJob {
            job: executor::RunningJob::AllocationOnly,
            cgroup_path: None,
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
        }
    }

    /// Re-points a run's supervisor at one the liveness read proves gone, which is
    /// what a test sees after a hook's owner dies between the cancel and the sweep.
    fn its_supervisor_has_since_died(
        admissions: &crate::admission::AdmissionStore,
        run: RunKey,
        step_id: spur_core::step::StepId,
    ) {
        let path = admissions
            .participant_path(run, step_id)
            .expect("participant path");
        let mut participant: crate::admission::ParticipantAdmission =
            serde_json::from_slice(&std::fs::read(&path).expect("read the participant"))
                .expect("decode the participant");
        participant.supervisor = Some(a_gone_supervisor());
        admissions
            .admit_participant(&participant)
            .expect("re-admit the participant");
    }

    /// One more participant on an already-admitted run, for a test that needs a
    /// numbered step beside the one that owns the job's lifetime.
    fn admit_a_participant_step(
        admissions: &crate::admission::AdmissionStore,
        node: &str,
        job_id: u32,
        step_id: spur_core::step::StepId,
        supervisor: Option<crate::admission::SupervisorRef>,
    ) {
        let mut participant = crate::admission::ParticipantAdmission::new(
            job_id,
            1,
            step_id,
            node,
            crate::admission::AdmittedResources::default(),
        );
        participant.lifecycle = crate::admission::ParticipantLifecycle::Running;
        participant.supervisor = supervisor;
        admissions
            .admit_participant(&participant)
            .expect("admit the participant");
    }

    /// The tracking entry a cancel drops, so a test asserting its removal is
    /// asserting something was there to remove.
    fn a_tracked_supervisor_session(
        job_id: u32,
        run_attempt: u32,
        state: &std::path::Path,
    ) -> crate::stepd::StepdDescriptor {
        crate::stepd::StepdDescriptor::new(
            job_id,
            run_attempt,
            spur_core::step::STEP_BATCH,
            std::process::id(),
            crate::stepd::process_start_ticks(std::process::id()).expect("start ticks"),
            state.join("stepd.sock"),
            state.join("cgroup"),
        )
    }

    /// The session that makes a cancel leave `job_id` tracked, naming the same
    /// process the run's participant records as its supervisor.
    async fn register_a_supervisor_session(
        svc: &AgentService,
        job_id: u32,
        state: &std::path::Path,
    ) {
        let live = a_live_supervisor();
        let descriptor = crate::stepd::StepdDescriptor::new(
            job_id,
            1,
            spur_core::step::STEP_BATCH,
            live.pid,
            live.start_ticks,
            state.join(format!("unreachable-{job_id}.sock")),
            std::path::PathBuf::new(),
        );
        svc.stepds
            .lock()
            .await
            .insert(stepd_key(&descriptor), descriptor);
    }

    /// A supervised run of `job_id` holding two cores, tracked and charged, as a
    /// cancel finds it: the shape every test below cancels out from under.
    async fn a_cancellable_supervised_run(
        svc: &AgentService,
        job_id: u32,
        supervisor: crate::admission::SupervisorRef,
    ) -> u32 {
        admit_a_supervised_run(
            &svc.admissions(),
            &svc.reporter.hostname,
            job_id,
            vec![0, 1],
            Some(supervisor),
        );
        {
            let mut alloc = svc.allocation.lock().await;
            alloc
                .allocate_for_job(job_id, 1, 2, 1_000, &[])
                .expect("allocate");
            alloc.commit_job(job_id, 1);
        }
        svc.insert_test_job(job_id, an_allocation_only_job(1)).await;
        svc.allocation.lock().await.free_cpus()
    }

    #[tokio::test]
    async fn the_claim_index_is_rebuilt_from_the_recorded_slice() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_state_dir(state.path()).await;
        let run = crate::admission::RunAdmission::new(
            7,
            1,
            "n1",
            crate::admission::AdmittedResources {
                cpu_ids: vec![2, 3, 5],
                memory_mb: 2048,
                gpu_devices: vec![1],
            },
            crate::admission::now_unix_ms(),
        );
        svc.admissions().admit_run(&run).expect("admit");

        let outcomes = svc.replay_admitted_allocations(&[]).await;
        assert_eq!(outcomes.len(), 1);

        // Exact, where a descriptor rebuild falls back to "the lowest free
        // cores" and under-counts a job whose cores were never recorded.
        let alloc = svc.allocation.lock().await;
        assert_eq!(alloc.allocated_gpu_ids(), vec![1]);
        for cpu in [2u32, 3, 5] {
            assert!(
                alloc.allocated_cpus[cpu as usize],
                "core {cpu} must still read as held"
            );
        }
        assert!(
            !alloc.allocated_cpus[0],
            "an unrecorded core must stay free"
        );
    }

    // A settled record that had already handed its slice back took the core
    // again on every start, starving the runs that genuinely still held one.
    #[tokio::test]
    async fn a_released_slice_is_not_charged_again_at_startup() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_state_dir(state.path()).await;
        let run = crate::admission::RunAdmission::new(
            7,
            1,
            "n1",
            crate::admission::AdmittedResources {
                cpu_ids: vec![0],
                memory_mb: 1024,
                gpu_devices: Vec::new(),
            },
            crate::admission::now_unix_ms(),
        );
        svc.admissions().admit_run(&run).expect("admit");
        svc.admissions()
            .record_slice_released(key(7, 1))
            .expect("record the release");

        let outcomes = svc.replay_admitted_allocations(&[]).await;
        assert_eq!(
            outcomes.len(),
            1,
            "the record still has to be reported to the controller"
        );

        let alloc = svc.allocation.lock().await;
        assert!(
            alloc.charged_runs().0.is_empty(),
            "a slice already given back must not be taken again"
        );
        assert!(!alloc.allocated_cpus[0], "the core must still read as free");
    }

    // A stale record winning the race left a running job's cores unprotected
    // for the rest of the agent's life: nothing restores a claim twice.
    #[tokio::test]
    async fn a_running_run_wins_a_core_a_gone_ones_record_also_names() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_state_dir(state.path()).await;
        let admissions = svc.admissions();
        // Named so the gone record sorts first, which is the order that lost it.
        admit_a_supervised_run(&admissions, "n1", 1, vec![0, 1], Some(a_gone_supervisor()));
        admit_a_supervised_run(&admissions, "n1", 2, vec![0, 1], Some(a_live_supervisor()));

        svc.replay_admitted_allocations(&[]).await;

        assert_eq!(
            svc.allocation.lock().await.charged_runs(),
            (HashSet::from([key(2, 1)]), vec![]),
            "the run still on those cores must be the one holding them"
        );
    }

    #[tokio::test]
    async fn a_run_with_no_supervisor_and_no_exit_is_held_as_unknown() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_state_dir(state.path()).await;
        let run = crate::admission::RunAdmission::new(
            7,
            1,
            "n1",
            crate::admission::AdmittedResources {
                cpu_ids: vec![0, 1],
                memory_mb: 1024,
                gpu_devices: vec![],
            },
            crate::admission::now_unix_ms(),
        );
        svc.admissions().admit_run(&run).expect("admit");
        svc.admissions()
            .admit_participant(&crate::admission::ParticipantAdmission::new(
                7,
                1,
                spur_core::step::STEP_BATCH,
                "n1",
                run.allocation.clone(),
            ))
            .expect("admit participant");
        // Through the production writer: a hand-set field would hide the fact
        // that nothing on the launch path records this.
        assert!(svc
            .admissions()
            .record_supervisor(
                key(7, 1),
                spur_core::step::STEP_BATCH,
                crate::admission::SupervisorRef {
                    pid: 999_999,
                    start_ticks: 1,
                    boot_id: crate::admission::current_boot_id(),
                },
            )
            .expect("record supervisor"));

        let outcomes = svc.replay_admitted_allocations(&[]).await;
        assert_eq!(
            outcomes[0].disposition,
            crate::admission::RunDisposition::Unknown
        );

        // The point of the whole ladder: not knowing is not a reason to free it.
        let alloc = svc.allocation.lock().await;
        assert!(alloc.allocated_cpus[0] && alloc.allocated_cpus[1]);
    }

    #[tokio::test]
    async fn a_recorded_exit_settles_a_run_whose_supervisor_is_gone() {
        // Reading the exit from the live descriptor set made this unreachable:
        // an exit only exists once the supervisor has gone.
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_state_dir(state.path()).await;
        let run = crate::admission::RunAdmission::new(
            7,
            1,
            "n1",
            crate::admission::AdmittedResources {
                cpu_ids: vec![0],
                memory_mb: 256,
                gpu_devices: vec![],
            },
            crate::admission::now_unix_ms(),
        );
        svc.admissions().admit_run(&run).expect("admit");
        svc.admissions()
            .admit_participant(&crate::admission::ParticipantAdmission::new(
                7,
                1,
                spur_core::step::STEP_BATCH,
                "n1",
                run.allocation.clone(),
            ))
            .expect("admit participant");
        let store = crate::stepd::StepdStore::new(state.path());
        store
            .prepare_session_dir(7, 1, spur_core::step::STEP_BATCH)
            .expect("session dir");
        store
            .obligations(7, 1, spur_core::step::STEP_BATCH)
            .append(&crate::stepd::StepdObligation::ExitObserved {
                exit_code: 3,
                signal: 0,
            })
            .expect("record exit");

        let outcomes = svc.replay_admitted_allocations(&[]).await;
        assert_eq!(
            outcomes[0].disposition,
            crate::admission::RunDisposition::SettledWithExit {
                exit_code: 3,
                signal: 0
            }
        );
        // Settled is still not released: only the acknowledgement frees it.
        assert!(svc.allocation.lock().await.allocated_cpus[0]);
    }

    #[tokio::test]
    async fn a_ledger_that_cannot_be_read_still_replays_the_descriptors() {
        // Returning empty here would advertise every running job's cores as
        // free on the next heartbeat.
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_state_dir(state.path()).await;
        // A file where the ledger directory belongs: the scan fails, rather
        // than reporting an empty but readable tree.
        std::fs::create_dir_all(state.path()).expect("state dir");
        std::fs::write(state.path().join("admission"), b"not a directory").expect("block");

        let mut descriptor = crate::stepd::StepdDescriptor::new(
            9,
            1,
            spur_core::step::STEP_BATCH,
            std::process::id(),
            crate::stepd::process_start_ticks(std::process::id()).expect("ticks"),
            state.path().join("runtime/9.1.4294967294/runtime.sock"),
            std::path::PathBuf::new(),
        );
        descriptor.resources = crate::stepd::StepdJobResources {
            cpus: 1,
            cpu_ids: vec![7],
            memory_mb: 128,
            gpu_devices: vec![],
            ..Default::default()
        };

        svc.replay_admitted_allocations(std::slice::from_ref(&descriptor))
            .await;
        assert!(
            svc.allocation.lock().await.allocated_cpus[7],
            "a failed ledger read must not leave live cores reading free"
        );
    }

    #[tokio::test]
    async fn a_session_predating_the_ledger_still_replays() {
        // An in-place upgrade over a live node has sessions with no record;
        // dropping them would free every running job's slice.
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_state_dir(state.path()).await;
        let mut descriptor = crate::stepd::StepdDescriptor::new(
            9,
            1,
            spur_core::step::STEP_BATCH,
            std::process::id(),
            crate::stepd::process_start_ticks(std::process::id()).expect("ticks"),
            state.path().join("runtime/9.1.4294967294/runtime.sock"),
            std::path::PathBuf::new(),
        );
        descriptor.boot_id = None;
        descriptor.resources = crate::stepd::StepdJobResources {
            cpus: 2,
            cpu_ids: vec![4, 6],
            memory_mb: 512,
            gpu_devices: vec![],
            ..Default::default()
        };

        let outcomes = svc
            .replay_admitted_allocations(std::slice::from_ref(&descriptor))
            .await;
        assert!(outcomes.is_empty(), "there is no record to classify");

        let alloc = svc.allocation.lock().await;
        assert!(
            alloc.allocated_cpus[4] && alloc.allocated_cpus[6],
            "the pre-ledger session's cores must still be held"
        );
    }

    #[tokio::test]
    async fn registering_an_allocation_records_its_entitlement() {
        let running = new_running_jobs();
        let reporter = Arc::new(NodeReporter::new(
            "n1".into(),
            "http://localhost:6817".into(),
            ResourceSet {
                cpus: 4,
                memory_mb: 8192,
                ..Default::default()
            },
            spur_net::NodeAddress {
                ip: "127.0.0.1".into(),
                hostname: "n1".into(),
                port: 6818,
                source: spur_net::AddressSource::Static,
            },
            std::collections::HashMap::new(),
            String::new(),
            String::new(),
            std::path::PathBuf::from("/etc/wireguard"),
            running.clone(),
            false,
        ));
        let svc = AgentService::with_cluster_config(
            reporter.clone(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            &spur_core::config::ClusterConfig::default(),
            spur_core::config::JobLimits::default(),
            CgroupConfig {
                enabled: false,
                ..CgroupConfig::default()
            },
            MpiConfig::default(),
            running,
            false,
        );
        let state_dir = tempfile::tempdir().expect("state dir");
        let svc = svc.with_runtime_state_dir(state_dir.path());

        svc.register_job_allocation(Request::new(RegisterJobAllocationRequest {
            job_id: 77,
            cpus: 2,
            run_attempt: 9,
            allocated: Some(ResourceAllocations {
                cpus: 2,
                memory_mb: 512,
                devices: std::collections::HashMap::new(),
            }),
            ..Default::default()
        }))
        .await
        .expect("register");

        let run = svc
            .admissions()
            .load_run(key(77, 9))
            .expect("the entitlement must outlive the process that claimed it");
        assert_eq!(run.node, "n1");
        assert_eq!(run.allocation.cpu_ids.len(), 2);
        assert_eq!(run.allocation.memory_mb, 512);
        assert!(
            run.created_at_unix_ms > 0,
            "a record with no age is uncollectable"
        );
    }

    #[tokio::test]
    async fn an_aborted_launch_leaves_no_entitlement_behind() {
        let running = new_running_jobs();
        let reporter = Arc::new(NodeReporter::new(
            "n1".into(),
            "http://localhost:6817".into(),
            ResourceSet {
                cpus: 4,
                memory_mb: 8192,
                ..Default::default()
            },
            spur_net::NodeAddress {
                ip: "127.0.0.1".into(),
                hostname: "n1".into(),
                port: 6818,
                source: spur_net::AddressSource::Static,
            },
            std::collections::HashMap::new(),
            String::new(),
            String::new(),
            std::path::PathBuf::from("/etc/wireguard"),
            running.clone(),
            false,
        ));
        let svc = AgentService::with_cluster_config(
            reporter.clone(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            &spur_core::config::ClusterConfig::default(),
            spur_core::config::JobLimits::default(),
            CgroupConfig {
                enabled: false,
                ..CgroupConfig::default()
            },
            MpiConfig::default(),
            running,
            false,
        );
        let state_dir = tempfile::tempdir().expect("state dir");
        let svc = svc.with_runtime_state_dir(state_dir.path());
        let admissions = svc.admissions();

        let run = crate::admission::RunAdmission::new(
            5,
            1,
            "n1",
            crate::admission::AdmittedResources::default(),
            crate::admission::now_unix_ms(),
        );
        admissions.admit_run(&run).expect("admit");
        assert!(admissions.load_run(key(5, 1)).is_ok());

        // Dropping while armed is the abort path: this process made the
        // reservation and is giving up before spawning anything against it.
        drop(LaunchReservationGuard::new(
            svc.allocation.clone(),
            admissions.clone(),
            key(5, 1),
        ));

        assert!(
            admissions.load_run(key(5, 1)).is_err(),
            "an aborted launch must not leave a claim nothing holds"
        );
    }

    /// Writes the pair `launch_job` writes before it spawns: a run and an owner
    /// step that owes a report. That owed report is what no retention collects.
    fn admit_a_launch(svc: &AgentService, job_id: u32) {
        let admissions = svc.admissions();
        let mut run = crate::admission::RunAdmission::new(
            job_id,
            1,
            &svc.reporter.hostname,
            crate::admission::AdmittedResources {
                cpu_ids: vec![0, 1],
                memory_mb: 1000,
                gpu_devices: Vec::new(),
            },
            crate::admission::now_unix_ms(),
        );
        // As `launch_job` writes it. Left unset, the owner guard waves through
        // any step's acknowledgement and every case here tests the fallback.
        run.lifecycle_owner_step = Some(spur_core::step::STEP_BATCH);
        admissions.admit_run(&run).expect("admit run");
        let mut participant = crate::admission::ParticipantAdmission::new(
            job_id,
            1,
            spur_core::step::STEP_BATCH,
            &svc.reporter.hostname,
            Default::default(),
        );
        participant.final_report.required = true;
        admissions
            .admit_participant(&participant)
            .expect("admit participant");
    }

    // Every settle/sweep/cut case is built on `admit_a_launch`'s hand-written
    // pair. This is the one that makes the real launch path answer for it.
    #[tokio::test]
    async fn a_real_launch_writes_the_record_the_other_cases_assume() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_state_dir(state.path()).await;
        let work_dir = tempfile::tempdir().expect("work dir");

        let response = svc
            .launch_job(Request::new(LaunchJobRequest {
                job_id: 4242,
                run_attempt: 1,
                spec: Some(JobSpec {
                    script: "#!/bin/sh\ntrue\n".into(),
                    cpus_per_task: 2,
                    work_dir: work_dir.path().to_string_lossy().into(),
                    ..Default::default()
                }),
                allocated: Some(ResourceAllocations {
                    cpus: 2,
                    memory_mb: 512,
                    devices: std::collections::HashMap::new(),
                }),
                ..Default::default()
            }))
            .await
            .expect("launch");
        let inner = response.into_inner();
        assert!(inner.success, "launch failed: {}", inner.error);

        let run = svc
            .admissions()
            .load_run(key(4242, 1))
            .expect("the launch must leave a run record");
        assert_eq!(run.node, svc.reporter.hostname);
        assert_eq!(run.state, crate::admission::RunState::Admitted);
        assert_eq!(run.allocation.cpu_ids.len(), 2);
        assert_eq!(run.allocation.memory_mb, 512);
        assert_ne!(
            run.created_at_unix_ms, 0,
            "an ageless record never collects"
        );
        assert_eq!(
            run.lifecycle_owner_step,
            Some(spur_core::step::STEP_BATCH),
            "the release gate keys on this; without it any step's ack frees the run"
        );
        assert!(!run.slice_released);

        let (participants, rejected) = svc
            .admissions()
            .participants(key(4242, 1))
            .expect("read participants");
        assert!(rejected.is_empty());
        assert_eq!(participants.len(), 1);
        assert_eq!(participants[0].step_id, spur_core::step::STEP_BATCH);
        assert!(
            participants[0].final_report.required,
            "a launch that owes no report can never free its slice"
        );
        assert_eq!(
            participants[0].allocation_subset.cpu_ids,
            run.allocation.cpu_ids
        );
    }

    // The guard frees a reservation nothing was spawned against. Once something
    // has been, those are a live run's cores and only the controller ends them.
    #[tokio::test]
    async fn an_aborted_launch_that_already_spawned_keeps_its_slice() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_state_dir(state.path()).await;
        admit_a_launch(&svc, 31);
        svc.allocation
            .lock()
            .await
            .allocate_for_job(31, 1, 4, 0, &[])
            .expect("reserve");
        let before = svc.allocation.lock().await.free_cpus();

        {
            let mut guard =
                LaunchReservationGuard::new(svc.allocation.clone(), svc.admissions(), key(31, 1));
            guard.mark_spawned();
        }

        assert_eq!(
            svc.allocation.lock().await.free_cpus(),
            before,
            "a running payload's cores must not go back on an aborted request"
        );
        assert!(
            svc.admissions().load_run(key(31, 1)).is_ok(),
            "the record is what still names the cores the payload holds"
        );
    }

    // A controller that has forgotten a job never acknowledges its completion, so
    // the cancel itself is what has to settle the run; nothing else ever will.
    #[tokio::test]
    async fn a_cancel_of_a_live_run_frees_its_slice_once_teardown_finishes() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_state_dir(state.path()).await;
        let admissions = svc.admissions();
        admit_a_launch(&svc, 7);
        svc.insert_test_job(
            7,
            TrackedJob {
                run_attempt: 1,
                ..TrackedJob::dummy(0)
            },
        )
        .await;
        {
            let mut alloc = svc.allocation.lock().await;
            alloc
                .allocate_for_job(7, 1, 2, 1000, &[])
                .expect("allocate");
            alloc.commit_job(7, 1);
        }
        let while_held = svc.allocation.lock().await.free_cpus();

        svc.cancel_job(Request::new(AgentCancelJobRequest {
            job_id: 7,
            run_attempt: 1,
            signal: 9,
        }))
        .await
        .expect("cancel");

        settle_cancelled_runs(&svc.lifecycle, &svc.allocation, &admissions).await;
        assert_eq!(
            svc.allocation.lock().await.free_cpus(),
            while_held,
            "a run still being torn down must keep its slice"
        );

        // What teardown finishing looks like: untracked, and the record cleaned.
        svc.running.lock().await.remove(&7);
        admissions
            .mark_run_cleaned(key(7, 1), crate::admission::EpilogOwed::No)
            .expect("cleaned");
        settle_cancelled_runs(&svc.lifecycle, &svc.allocation, &admissions).await;

        assert_eq!(
            svc.allocation.lock().await.free_cpus(),
            while_held + 2,
            "a cancelled run's slice must go back once its teardown is done"
        );
        assert_eq!(
            admissions
                .sweep(crate::admission::now_unix_ms(), 1, &HashSet::new())
                .expect("sweep"),
            1,
            "and nothing of it may be left to re-advertise as a claim"
        );
    }

    // The one answer that unsticks a run whose completion never landed: the
    // controller's word that it is not accounting for it.
    #[tokio::test]
    async fn a_settle_from_the_controller_releases_a_run_nothing_else_can_free() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_state_dir(state.path()).await;
        let admissions = svc.admissions();
        admit_a_launch(&svc, 7);
        let while_held = charge_a_slice(&svc, 7).await;
        // The stranded shape: teardown done, no acknowledgement, slice charged.
        // Driven through the teardown so the record says what this node owes.
        tear_down_a_finished_run(&svc, 7, state.path()).await;
        admissions
            .take_conflict_hold(key(7, 1), "held with no tracked job on this agent")
            .expect("hold");

        let response = svc
            .settle_run(Request::new(SettleRunRequest {
                job_id: 7,
                run_attempt: 1,
            }))
            .await
            .expect("settle")
            .into_inner();

        assert!(response.released);
        assert_eq!(
            svc.allocation.lock().await.free_cpus(),
            while_held + 2,
            "the cores the run was holding must go back to the node"
        );
        let run = admissions.load_run(key(7, 1)).expect("record");
        assert!(run.slice_released);
        assert!(
            run.conflict_hold.is_none(),
            "the hold is what kept asking for a reconcile that never resolved"
        );
    }

    /// The monitor's own teardown, so a test reaches the record this node writes
    /// rather than the one it would have to remember to write by hand.
    async fn tear_down_a_finished_run(svc: &AgentService, job_id: u32, state: &std::path::Path) {
        let cgroup = state.join(format!("cgroup_{job_id}"));
        std::fs::create_dir_all(&cgroup).expect("cgroup");
        let completed = CompletedJob {
            run_attempt: 1,
            ..completed_job(job_id, cgroup)
        };
        teardown_completed_job(
            &completed,
            &svc.lifecycle,
            &svc.running,
            &svc.allocation,
            &svc.mpi_host,
            &svc.admissions(),
            epilog_owed(&svc.hooks),
        )
        .await;
    }

    /// Charge a run's slice to this node, as its launch did.
    async fn charge_a_slice(svc: &AgentService, job_id: u32) -> u32 {
        let mut alloc = svc.allocation.lock().await;
        alloc
            .allocate_for_job(job_id, 1, 2, 1000, &[])
            .expect("allocate");
        alloc.commit_job(job_id, 1);
        alloc.free_cpus()
    }

    // Teardown finishes before the epilog runs. On a node with a hook the record
    // has to carry that debt, or the settle frees cores it is about to run on.
    #[tokio::test]
    async fn a_settle_cannot_free_cores_an_epilog_has_not_started_on() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_state_dir(state.path())
            .await
            .with_epilog_hook("/usr/local/sbin/spur-epilog");
        let admissions = svc.admissions();
        admit_a_launch(&svc, 7);
        let while_held = charge_a_slice(&svc, 7).await;
        tear_down_a_finished_run(&svc, 7, state.path()).await;

        assert_eq!(
            admissions.settle_permit(key(7, 1)).expect("permit"),
            crate::admission::SettlePermit::NotQuiescent,
            "the hook has not started, and only this record can say it is owed"
        );
        let response = svc
            .settle_run(Request::new(SettleRunRequest {
                job_id: 7,
                run_attempt: 1,
            }))
            .await
            .expect("settle")
            .into_inner();

        assert!(!response.released);
        assert_eq!(
            svc.allocation.lock().await.free_cpus(),
            while_held,
            "the operator's epilog has not run on these cores yet"
        );
        assert!(
            !admissions
                .load_run(key(7, 1))
                .expect("record")
                .slice_released,
            "a cut that drops this entry stops advertising a claim the node holds"
        );

        // The hook runs, and the veto lifts with it and nothing else.
        admissions
            .record_epilog(key(7, 1), crate::admission::HookState::Succeeded)
            .expect("epilog done");
        let response = svc
            .settle_run(Request::new(SettleRunRequest {
                job_id: 7,
                run_attempt: 1,
            }))
            .await
            .expect("settle")
            .into_inner();
        assert!(response.released);
        assert_eq!(svc.allocation.lock().await.free_cpus(), while_held + 2);
    }

    // The restart this design exists for: the record is all that is left of the
    // run, and adopting one is not evidence that its payload is gone.
    #[tokio::test]
    async fn a_settle_for_an_adopted_run_no_teardown_has_touched_is_declined() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_state_dir(state.path()).await;
        let admissions = svc.admissions();
        admit_a_launch(&svc, 7);
        let while_held = charge_a_slice(&svc, 7).await;
        assert!(
            svc.running.lock().await.is_empty(),
            "a restart rebuilds the ledger, not the process table"
        );

        assert_eq!(
            admissions.settle_permit(key(7, 1)).expect("permit"),
            crate::admission::SettlePermit::NotQuiescent,
            "nothing here has torn this run down, so nothing may hand its cores back"
        );
        let response = svc
            .settle_run(Request::new(SettleRunRequest {
                job_id: 7,
                run_attempt: 1,
            }))
            .await
            .expect("settle")
            .into_inner();

        assert!(!response.released);
        assert_eq!(svc.allocation.lock().await.free_cpus(), while_held);
        assert!(
            !admissions
                .load_run(key(7, 1))
                .expect("record")
                .slice_released
        );
    }

    // No acknowledgement may free a slice out from under a live payload; the
    // agent declining costs one reconcile, obeying costs a double-allocated core.
    #[tokio::test]
    async fn a_settle_for_a_run_this_agent_is_still_running_is_declined() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_state_dir(state.path()).await;
        let admissions = svc.admissions();
        admit_a_launch(&svc, 7);
        svc.insert_test_job(
            7,
            TrackedJob {
                run_attempt: 1,
                ..TrackedJob::dummy(0)
            },
        )
        .await;
        {
            let mut alloc = svc.allocation.lock().await;
            alloc
                .allocate_for_job(7, 1, 2, 1000, &[])
                .expect("allocate");
            alloc.commit_job(7, 1);
        }
        let while_held = svc.allocation.lock().await.free_cpus();

        let response = svc
            .settle_run(Request::new(SettleRunRequest {
                job_id: 7,
                run_attempt: 1,
            }))
            .await
            .expect("settle")
            .into_inner();

        assert!(!response.released);
        assert_eq!(
            svc.allocation.lock().await.free_cpus(),
            while_held,
            "a payload is still on these cores"
        );
        assert!(
            !admissions
                .load_run(key(7, 1))
                .expect("record")
                .slice_released
        );
    }

    // Teardown drops the job from `running` and marks the record cleaned before
    // the epilog runs, so both of the settle's own guards pass while a hook is live.
    #[tokio::test]
    async fn a_settle_while_an_epilog_is_still_running_is_declined() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_state_dir(state.path()).await;
        let admissions = svc.admissions();
        admit_a_launch(&svc, 7);
        {
            let mut alloc = svc.allocation.lock().await;
            alloc
                .allocate_for_job(7, 1, 2, 1000, &[])
                .expect("allocate");
            alloc.commit_job(7, 1);
        }
        admissions
            .mark_run_cleaned(key(7, 1), crate::admission::EpilogOwed::No)
            .expect("cleaned");
        admissions
            .record_epilog(key(7, 1), crate::admission::HookState::Running)
            .expect("epilog running");
        let while_held = svc.allocation.lock().await.free_cpus();

        assert_eq!(
            admissions.settle_permit(key(7, 1)).expect("permit"),
            crate::admission::SettlePermit::NotQuiescent,
            "the gate itself must refuse, not merely the release downstream of it"
        );
        let response = svc
            .settle_run(Request::new(SettleRunRequest {
                job_id: 7,
                run_attempt: 1,
            }))
            .await
            .expect("settle")
            .into_inner();

        assert!(
            !response.released,
            "the controller cannot see the hook; only this side can veto on it"
        );
        assert_eq!(
            svc.allocation.lock().await.free_cpus(),
            while_held,
            "an epilog is still on these cores"
        );
        let run = admissions.load_run(key(7, 1)).expect("record");
        assert!(
            !run.slice_released,
            "a cut that drops this entry stops advertising a claim the node still holds"
        );

        // The veto lifts with the hook, and nothing else is needed to unstick it.
        admissions
            .record_epilog(key(7, 1), crate::admission::HookState::Succeeded)
            .expect("epilog done");
        let response = svc
            .settle_run(Request::new(SettleRunRequest {
                job_id: 7,
                run_attempt: 1,
            }))
            .await
            .expect("settle")
            .into_inner();
        assert!(response.released);
        assert_eq!(svc.allocation.lock().await.free_cpus(), while_held + 2);
    }

    // The controller counts what comes back as a release it caused, so a settle
    // that found nothing to settle must not be counted as one.
    #[tokio::test]
    async fn a_settle_for_a_run_this_agent_has_no_record_of_reports_no_release() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_state_dir(state.path()).await;

        let response = svc
            .settle_run(Request::new(SettleRunRequest {
                job_id: 7,
                run_attempt: 1,
            }))
            .await
            .expect("settle")
            .into_inner();

        assert!(!response.released);
        assert!(!response.error.is_empty(), "the caller is told why");
    }

    // Reconciling cancels a leaked attempt beside a live one, and PMIx
    // namespaces are keyed by job alone, so the sibling's would go with it.
    #[tokio::test]
    async fn cancelling_a_stale_attempt_leaves_a_live_siblings_pmix_alone() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_state_dir(state.path()).await;
        let namespace = (7, spur_core::step::STEP_BATCH);
        svc.insert_test_job(
            7,
            TrackedJob {
                run_attempt: 2,
                ..TrackedJob::dummy(0)
            },
        )
        .await;
        let hosted = || {
            svc.mpi_host
                .active_namespaces
                .lock()
                .expect("namespaces")
                .contains_key(&namespace)
        };
        svc.mpi_host
            .active_namespaces
            .lock()
            .expect("namespaces")
            .insert(namespace, "spur.7.2".into());

        svc.cancel_job(Request::new(AgentCancelJobRequest {
            job_id: 7,
            run_attempt: 1,
            signal: 9,
        }))
        .await
        .expect("cancel the stale attempt");

        assert!(
            hosted(),
            "attempt 2 is still running under this job id and still needs its namespace"
        );

        svc.cancel_job(Request::new(AgentCancelJobRequest {
            job_id: 7,
            run_attempt: 2,
            signal: 9,
        }))
        .await
        .expect("cancel the tracked attempt");

        assert!(
            !hosted(),
            "the attempt that owns the namespace must still take it down"
        );
    }

    // A reaped launch produced nothing, and its record would owe a report
    // nothing will acknowledge -- so no sweep could ever collect it.
    #[tokio::test]
    async fn a_launch_whose_workload_was_reaped_leaves_no_record() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_state_dir(state.path()).await;
        let admissions = svc.admissions();
        admit_a_launch(&svc, 13);
        admit_a_launch(&svc, 14);
        assert_eq!(
            admissions
                .sweep(u64::MAX, 1, &HashSet::new())
                .expect("sweep"),
            0,
            "an owed report is what makes a leftover launch record immortal"
        );

        let mut reaped =
            LaunchReservationGuard::new(svc.allocation.clone(), admissions.clone(), key(13, 1));
        reaped.mark_spawned();
        reaped.mark_reaped();
        drop(reaped);

        assert!(
            admissions.load_run(key(13, 1)).is_err(),
            "a launch that produced nothing must leave no claim behind"
        );

        // The other side of the rule: a launch that really did leave something
        // running must keep its record, or the live process reads as free capacity.
        let mut running =
            LaunchReservationGuard::new(svc.allocation.clone(), admissions.clone(), key(14, 1));
        running.mark_spawned();
        drop(running);

        assert!(admissions.load_run(key(14, 1)).is_ok());
    }

    fn a_test_reporter() -> Arc<NodeReporter> {
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
            std::path::PathBuf::from("/etc/wireguard"),
            new_running_jobs(),
            false,
        ))
    }

    // The agent can name this the moment it sees it. Left for the routine sweep,
    // the cores it holds stay out of the cluster for up to an hour.
    #[tokio::test]
    async fn a_claim_with_no_job_behind_it_asks_the_controller_to_reconcile() {
        let spool = tempfile::tempdir().expect("admission state directory");
        let admissions = crate::admission::AdmissionStore::new(spool.path(), "test-node");
        let reporter = a_test_reporter();
        reporter.set_admissions(admissions.clone());
        admit_a_supervised_run(
            &admissions,
            "test-node",
            42,
            vec![0, 1],
            Some(a_live_supervisor()),
        );

        assert!(
            !reporter.wants_reconcile().await,
            "a run with a live supervisor behind it explains itself"
        );

        flag_unbacked_allocations(&[(42, 1)], &admissions).await;

        assert!(
            reporter.wants_reconcile().await,
            "a claim the agent cannot account for must reach the controller now, not in an hour"
        );
    }

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
            std::path::PathBuf::from("/etc/wireguard"),
            running.clone(),
            false,
        ));
        let svc = AgentService::with_cluster_config(
            reporter.clone(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            &spur_core::config::ClusterConfig::default(),
            spur_core::config::JobLimits::default(),
            // Registration now creates the job's cgroup; keep this test off the
            // runner's real cgroupfs.
            CgroupConfig {
                enabled: false,
                ..CgroupConfig::default()
            },
            MpiConfig::default(),
            running,
            false, // allow_root_jobs
        );
        // Registration now persists an admission record; keep it off the
        // runner's real /var/spool.
        let state_dir = tempfile::tempdir().expect("state dir");
        let svc = svc.with_runtime_state_dir(state_dir.path());

        assert!(
            reporter.held_job_ids().is_empty(),
            "reporter sees no jobs before registration"
        );

        svc.register_job_allocation(Request::new(RegisterJobAllocationRequest {
            job_id: 77,
            cpus: 1,
            run_attempt: 9,
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
        assert_eq!(
            svc.running
                .lock()
                .await
                .get(&77)
                .expect("tracked allocation")
                .run_attempt,
            9
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
            alloc.allocate_for_job(7, 1, 1, 0, &[0]).unwrap();
        }
        assert_eq!(
            svc.free_gpu_count().await,
            0,
            "GPU reserved while launching"
        );

        svc.cancel_job(Request::new(AgentCancelJobRequest {
            job_id: 7,
            signal: 9,
            run_attempt: 1,
        }))
        .await
        .expect("cancel_job");

        assert_eq!(
            svc.free_gpu_count().await,
            1,
            "cancel must release a launching (never-committed) reservation"
        );
    }

    // A cancel for a superseded attempt must not release a redispatch's
    // already-reserved, newer attempt.
    #[tokio::test]
    async fn cancel_spares_a_reused_job_ids_reservation() {
        let svc = AgentService::new(
            test_reporter_with_gpus(&[0]),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        {
            let mut alloc = svc.allocation.lock().await;
            alloc.allocate_for_job(7, 1, 1, 0, &[0]).unwrap();
            alloc.release_job(ReleaseWarrant::controller_cancelled(RunKey::any_attempt(7)));
            alloc.allocate_for_job(7, 2, 1, 0, &[0]).unwrap();
        }

        svc.cancel_job(Request::new(AgentCancelJobRequest {
            job_id: 7,
            signal: 9,
            run_attempt: 1,
        }))
        .await
        .expect("cancel_job");

        assert_eq!(
            svc.free_gpu_count().await,
            0,
            "a stale cancel must not release the current attempt's reservation"
        );
    }

    // A launch that aborts before entering `running` must tear down its PMI
    // server, since the monitor loop's completion cleanup never runs for it.
    #[tokio::test]
    async fn completion_cleanup_stops_every_pmix_namespace_the_agent_hosts_for_the_job() {
        let svc = AgentService::new(
            test_reporter_with_gpus(&[0]),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        for step_id in [spur_core::step::STEP_BATCH, 0] {
            svc.mpi_host
                .active_namespaces
                .lock()
                .unwrap()
                .insert((99, step_id), format!("spur.99.{step_id}"));
        }

        cleanup_completed_job_mpi(99, MPI_PMIX, &svc.mpi_host).await;

        for step_id in [spur_core::step::STEP_BATCH, 0] {
            assert!(
                !svc.mpi_host.has_active_pmix(99, step_id),
                "a finished job must leave no PMIx namespace hosted on the agent"
            );
        }
    }

    // A guard dropped while another task holds the steps lock must still
    // release, via the spawned-task fallback, once the lock frees up.
    #[tokio::test]
    async fn active_step_guard_releases_via_spawn_when_lock_is_contended() {
        let steps: Arc<Mutex<HashMap<(u32, u32), ActiveStep>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let key = (77, 1);
        steps.lock().await.insert(key, ActiveStep::default());

        let held = steps.lock().await;
        drop(ActiveStepGuard {
            steps: steps.clone(),
            key,
        });
        drop(held);

        let removed = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if !steps.lock().await.contains_key(&key) {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            removed.is_ok(),
            "guard should release the step entry once the lock frees up"
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
                .allocate_for_job(9, 1, 1, 0, &[0])
                .unwrap();
            let guard =
                LaunchReservationGuard::new(svc.allocation.clone(), svc.admissions(), key(9, 1));
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
                .allocate_for_job(10, 1, 1, 0, &[0])
                .unwrap();
            svc.allocation.lock().await.commit_job(10, 1);
            let mut guard =
                LaunchReservationGuard::new(svc.allocation.clone(), svc.admissions(), key(10, 1));
            guard.disarm();
            drop(guard);
        }
        assert_eq!(
            svc.free_gpu_count().await,
            0,
            "a disarmed guard must leave the committed reservation intact"
        );
    }

    // A guard whose reservation was superseded by a redispatch (e.g. a cancel
    // raced the original launch) must not release the new attempt's own
    // reservation when it drops still armed.
    #[tokio::test]
    async fn reservation_guard_spares_a_reused_job_ids_reservation_on_drop() {
        let svc = AgentService::new(
            test_reporter_with_gpus(&[0]),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        svc.allocation
            .lock()
            .await
            .allocate_for_job(11, 1, 1, 0, &[0])
            .unwrap();
        let guard =
            LaunchReservationGuard::new(svc.allocation.clone(), svc.admissions(), key(11, 1));

        // A cancel races the still-in-flight launch, releasing attempt 1's
        // reservation; the controller redispatches attempt 2, which reserves
        // and commits before attempt 1's guard is ever dropped.
        svc.allocation
            .lock()
            .await
            .release_job(ReleaseWarrant::controller_cancelled(RunKey::any_attempt(
                11,
            )));
        svc.allocation
            .lock()
            .await
            .allocate_for_job(11, 2, 1, 0, &[0])
            .unwrap();
        svc.allocation.lock().await.commit_job(11, 2);

        drop(guard);

        assert_eq!(
            svc.free_gpu_count().await,
            0,
            "attempt 1's stale guard must not release attempt 2's reservation"
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

    /// Records the completion reports it receives. Every other RPC reports
    /// unimplemented, so drifting onto an unmocked call fails loudly.
    struct MockController {
        reports: Arc<std::sync::Mutex<Vec<spur_proto::proto::ReportJobStatusRequest>>>,
        /// How completion reports are answered; `None` accepts them.
        refusal: Option<tonic::Code>,
    }

    /// The `async_trait` attribute has to be applied by the macro: it rewrites
    /// `async fn` signatures and only sees bodies that exist when it runs.
    macro_rules! mock_controller_impl {
        ($($method:ident($req:ty) -> $resp:ty;)*) => {
            #[tonic::async_trait]
            impl spur_proto::proto::slurm_controller_server::SlurmController for MockController {
                async fn report_job_status(
                    &self,
                    request: tonic::Request<spur_proto::proto::ReportJobStatusRequest>,
                ) -> Result<tonic::Response<()>, tonic::Status> {
                    let report = request.into_inner();
                    let job_id = report.job_id;
                    self.reports.lock().expect("completion reports").push(report);
                    match self.refusal {
                        Some(code) => Err(tonic::Status::new(code, format!("job {job_id} not found"))),
                        None => Ok(tonic::Response::new(())),
                    }
                }
                $(
                    async fn $method(
                        &self,
                        _request: tonic::Request<$req>,
                    ) -> Result<tonic::Response<$resp>, tonic::Status> {
                        Err(tonic::Status::unimplemented(stringify!($method)))
                    }
                )*
            }
        };
    }

    mock_controller_impl! {
            submit_job(spur_proto::proto::SubmitJobRequest) -> spur_proto::proto::SubmitJobResponse;
            get_jobs(spur_proto::proto::GetJobsRequest) -> spur_proto::proto::GetJobsResponse;
            get_job(spur_proto::proto::GetJobRequest) -> spur_proto::proto::JobInfo;
            cancel_job(spur_proto::proto::CancelJobRequest) -> ();
            complete_job(spur_proto::proto::CompleteJobRequest) -> ();
            job_keepalive(spur_proto::proto::JobKeepaliveRequest) -> spur_proto::proto::JobKeepaliveResponse;
            suspend_job(spur_proto::proto::SuspendJobRequest) -> ();
            resume_job(spur_proto::proto::ResumeJobRequest) -> ();
            update_job(spur_proto::proto::UpdateJobRequest) -> ();
            requeue_job(spur_proto::proto::RequeueJobRequest) -> spur_proto::proto::RequeueJobResponse;
            get_nodes(spur_proto::proto::GetNodesRequest) -> spur_proto::proto::GetNodesResponse;
            get_node(spur_proto::proto::GetNodeRequest) -> spur_proto::proto::NodeInfo;
            update_node(spur_proto::proto::UpdateNodeRequest) -> ();
            drain_node(spur_proto::proto::DrainNodeRequest) -> spur_proto::proto::DrainNodeResponse;
            deregister_node(spur_proto::proto::DeregisterNodeRequest) -> spur_proto::proto::DeregisterNodeResponse;
            deregister_agent(spur_proto::proto::DeregisterAgentRequest) -> ();
            get_partitions(spur_proto::proto::GetPartitionsRequest) -> spur_proto::proto::GetPartitionsResponse;
            get_job_steps(spur_proto::proto::GetJobStepsRequest) -> spur_proto::proto::GetJobStepsResponse;
            create_job_step(spur_proto::proto::CreateJobStepRequest) -> spur_proto::proto::CreateJobStepResponse;
            complete_job_step(spur_proto::proto::CompleteJobStepRequest) -> ();
            ping(()) -> spur_proto::proto::PingResponse;
            get_job_metrics(()) -> spur_proto::proto::JobMetrics;
            get_node_metrics(()) -> spur_proto::proto::NodeMetrics;
            get_rpc_stats(()) -> spur_proto::proto::RpcStats;
            reset_diag_stats(()) -> ();
            get_sched_stats(()) -> spur_proto::proto::SchedStats;
            get_assoc_mgr_info(spur_proto::proto::GetAssocMgrInfoRequest) -> spur_proto::proto::GetAssocMgrInfoResponse;
            register_agent(spur_proto::proto::RegisterAgentRequest) -> spur_proto::proto::RegisterAgentResponse;
            heartbeat(spur_proto::proto::HeartbeatRequest) -> spur_proto::proto::HeartbeatResponse;
            report_stepd_recovery(spur_proto::proto::StepdRecoveryRequest) -> spur_proto::proto::StepdRecoveryResponse;
            create_token(spur_proto::proto::CreateTokenRequest) -> spur_proto::proto::CreateTokenResponse;
            list_tokens(spur_proto::proto::ListTokensRequest) -> spur_proto::proto::ListTokensResponse;
            revoke_token(spur_proto::proto::RevokeTokenRequest) -> spur_proto::proto::RevokeTokenResponse;
            create_partition(spur_proto::proto::CreatePartitionRequest) -> ();
            update_partition(spur_proto::proto::UpdatePartitionRequest) -> ();
            delete_partition(spur_proto::proto::DeletePartitionRequest) -> ();
            reconfigure(()) -> ();
            create_reservation(spur_proto::proto::CreateReservationRequest) -> ();
            update_reservation(spur_proto::proto::UpdateReservationRequest) -> ();
            delete_reservation(spur_proto::proto::DeleteReservationRequest) -> ();
            list_reservations(spur_proto::proto::ListReservationsRequest) -> spur_proto::proto::ListReservationsResponse;
            exec_in_job(spur_proto::proto::ExecInJobRequest) -> spur_proto::proto::ExecInJobResponse;
            run_step(spur_proto::proto::RunStepRequest) -> spur_proto::proto::RunStepResponse;
            cluster_up(spur_proto::proto::ClusterUpRequest) -> spur_proto::proto::ClusterUpResponse;
            cluster_down(spur_proto::proto::ClusterDownRequest) -> spur_proto::proto::ClusterDownResponse;
            cluster_status(spur_proto::proto::ClusterStatusRequest) -> spur_proto::proto::ClusterStatusResponse;
            cluster_kubeconfig(spur_proto::proto::ClusterKubeconfigRequest) -> spur_proto::proto::ClusterKubeconfigResponse;
            cluster_add_nodes(spur_proto::proto::ClusterAddNodesRequest) -> spur_proto::proto::ClusterAddNodesResponse;
            cluster_remove_nodes(spur_proto::proto::ClusterRemoveNodesRequest) -> spur_proto::proto::ClusterRemoveNodesResponse;

    }

    type CompletionReports = Arc<std::sync::Mutex<Vec<spur_proto::proto::ReportJobStatusRequest>>>;

    fn spawn_mock_controller() -> (String, CompletionReports) {
        spawn_mock_controller_answering(None)
    }

    fn spawn_mock_controller_answering(
        refusal: Option<tonic::Code>,
    ) -> (String, CompletionReports) {
        let incoming = tonic::transport::server::TcpIncoming::bind(
            "127.0.0.1:0".parse().expect("loopback address"),
        )
        .expect("bind mock controller");
        let addr = incoming.local_addr().expect("mock controller address");
        let reports: CompletionReports = Arc::new(std::sync::Mutex::new(Vec::new()));
        let service = MockController {
            reports: reports.clone(),
            refusal,
        };
        tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(spur_proto::controller_server(service))
                .serve_with_incoming(incoming)
                .await;
        });
        (format!("http://{addr}"), reports)
    }

    fn fence_context(
        running: &RunningJobs,
        allocation: &Arc<Mutex<NodeAllocation>>,
        sessions: &Arc<Mutex<StepdMap>>,
        completions: &crate::step_completion::StepCompletions,
        store: &crate::stepd::StepdStore,
    ) -> CompletionListenerContext {
        CompletionListenerContext {
            admissions: crate::admission::AdmissionStore::new(
                std::env::temp_dir().join(format!("spur-test-adm-{}", uuid::Uuid::new_v4())),
                "test-node",
            ),
            running: running.clone(),
            step_completions: completions.clone(),
            allocation: allocation.clone(),
            stepds: sessions.clone(),
            stepds_store: store.clone(),
            controller_addr: "http://127.0.0.1:1".into(),
            hostname: "test-node".into(),
        }
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

        svc.graceful_cancel(job_id, 0).await;

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
            job: executor::RunningJob::Managed { child },
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
            cgroup_path: None,
        };
        svc.insert_test_job(job_id, tracked).await;

        svc.graceful_cancel(job_id, 0).await;

        // 5s grace + up to 2s monitor tick + buffer
        assert!(
            wait_job_reaped(&svc, job_id, 10_000).await,
            "monitor should reap job after SIGKILL escalation"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn graceful_cancel_stepd_escalates_to_sigkill() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        // Paused time auto-advances onto the control-exchange bound mid-socket,
        // dropping the connection under the fake supervisor below.
        let _unbounded = crate::stepd::UnboundedRequests::new();
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        let state = tempfile::tempdir().expect("runtime socket directory");
        let socket_path = state.path().join("runtime.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("bind runtime socket");
        let mut descriptor = crate::stepd::StepdDescriptor::new(
            903,
            4,
            spur_core::step::STEP_BATCH,
            0,
            0,
            socket_path,
            std::path::PathBuf::new(),
        );
        descriptor.capability = "runtime-cancel-test".into();
        svc.stepds
            .lock()
            .await
            .insert(stepd_key(&descriptor), descriptor.clone());

        let server_descriptor = descriptor.clone();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for _ in 0..2 {
                let (stream, _) = crate::stepd::accept_hello(
                    &listener,
                    &server_descriptor,
                    &server_descriptor.capability,
                )
                .await
                .expect("accept runtime hello");
                let (reader, mut writer) = stream.into_split();
                let mut reader = BufReader::new(reader);
                let mut line = String::new();
                reader
                    .read_line(&mut line)
                    .await
                    .expect("read runtime request");
                requests.push(serde_json::from_str(&line).expect("decode runtime request"));
                writer
                    .write_all(
                        format!(
                            "{}\n",
                            serde_json::to_string(&crate::stepd::StepdResponse::Acknowledged)
                                .expect("encode acknowledgement")
                        )
                        .as_bytes(),
                    )
                    .await
                    .expect("acknowledge runtime request");
            }
            requests
        });

        svc.graceful_cancel(descriptor.job_id, 0).await;
        tokio::time::advance(tokio::time::Duration::from_secs(6)).await;
        tokio::task::yield_now().await;

        let requests = server.await.expect("runtime control server");
        assert!(matches!(
            requests.as_slice(),
            [crate::stepd::StepdRequest::Shutdown,
             crate::stepd::StepdRequest::SignalAllocation { signal }]
                if *signal == nix::sys::signal::Signal::SIGKILL as i32
        ));
    }

    // The immediate shutdown needs the same epoch guard as the grace-period
    // SIGKILL, or a cancel for a superseded run tears down its replacement.
    #[tokio::test(start_paused = true)]
    async fn graceful_cancel_ignores_a_superseded_attempt() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        let state = tempfile::tempdir().expect("runtime socket directory");
        let socket_path = state.path().join("runtime.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("bind runtime socket");
        let mut descriptor = crate::stepd::StepdDescriptor::new(
            904,
            4,
            spur_core::step::STEP_BATCH,
            0,
            0,
            socket_path,
            std::path::PathBuf::new(),
        );
        descriptor.capability = "superseded-cancel-test".into();
        svc.stepds
            .lock()
            .await
            .insert(stepd_key(&descriptor), descriptor.clone());

        svc.graceful_cancel(904, 3).await;

        let contacted =
            tokio::time::timeout(tokio::time::Duration::from_secs(1), listener.accept()).await;
        assert!(
            contacted.is_err(),
            "a cancel for a superseded attempt must not reach the live supervisor"
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
                job: executor::RunningJob::Managed { child },
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
                cgroup_path: None,
            };
            (t, pid)
        }
        let job_id = 902;
        let (run1, pid1) = spawn_trap(1);
        svc.insert_test_job(job_id, run1).await;

        // Cancel epoch 1 (SIGTERM; trapped, survives) and spawn the grace timer.
        svc.graceful_cancel(job_id, 0).await;

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

    // The cancel that ends a run is not the end of the work on its cores: an
    // operator's epilog is still standing on them when the tracking goes away.
    #[tokio::test]
    async fn a_cancel_holds_the_slice_while_a_supervisor_owes_an_epilog() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_epilog(state.path()).await;
        let admissions = svc.admissions();
        let while_held = a_cancellable_supervised_run(&svc, 51, a_live_supervisor()).await;

        svc.drop_tracked_job(51, 1).await;

        assert_eq!(
            svc.allocation.lock().await.free_cpus(),
            while_held,
            "a hook still running under the run must keep its cores"
        );
        let run = admissions.load_run(key(51, 1)).expect("record");
        assert_eq!(run.cleanup.epilog, crate::admission::HookState::Pending);
        assert!(!run.slice_released, "and the record must not say otherwise");
        assert!(
            run.cancelled_by_controller,
            "the cancel has to be durable, or nothing finishes the release later"
        );
    }

    // A gate that refuses everything looks exactly like one that works. With no
    // hook configured there is nothing to wait for and the cores go back at once.
    #[tokio::test]
    async fn a_cancel_with_no_epilog_configured_releases_at_once() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_state_dir(state.path()).await;
        let while_held = a_cancellable_supervised_run(&svc, 52, a_live_supervisor()).await;

        svc.drop_tracked_job(52, 1).await;

        assert_eq!(
            svc.allocation.lock().await.free_cpus(),
            while_held + 2,
            "with no hook owed the cancel itself hands the cores back"
        );
        let run = svc.admissions().load_run(key(52, 1)).expect("record");
        assert!(run.slice_released);
        assert!(
            run.cancelled_by_controller,
            "the release still has to go through the cancel's own record"
        );
    }

    // A failed hook has returned, so it is off the cores and nothing re-runs it.
    // Holding for one would be a hold with no way out.
    #[tokio::test]
    async fn a_failed_epilog_hands_the_cancelled_run_its_slice_back() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_epilog(state.path()).await;
        let admissions = svc.admissions();
        let store = crate::stepd::StepdStore::new(state.path());
        let while_held = a_cancellable_supervised_run(&svc, 53, a_live_supervisor()).await;
        svc.drop_tracked_job(53, 1).await;

        // The supervisor's own ledger, which it writes before telling anyone.
        store
            .prepare_session_dir(53, 1, spur_core::step::STEP_BATCH)
            .expect("session dir");
        let obligations = store.obligations(53, 1, spur_core::step::STEP_BATCH);
        obligations
            .append(&crate::stepd::StepdObligation::ExitObserved {
                exit_code: 0,
                signal: 9,
            })
            .expect("record the exit");
        obligations
            .append(&crate::stepd::StepdObligation::EpilogCompleted { failed: true })
            .expect("record the epilog");

        resolve_supervised_epilogs(&store, &admissions, &mut HashMap::new()).await;
        assert_eq!(
            admissions
                .load_run(key(53, 1))
                .expect("record")
                .cleanup
                .epilog,
            crate::admission::HookState::Failed
        );
        settle_cancelled_runs(&svc.lifecycle, &svc.allocation, &admissions).await;

        assert_eq!(
            svc.allocation.lock().await.free_cpus(),
            while_held + 2,
            "a hook that has returned no longer holds anything"
        );
    }

    // The outcome reaches the record through the supervisor's own ledger, not
    // through anything the cancel tore down, so a live agent finishes the release.
    #[tokio::test]
    async fn a_finished_epilog_reaches_the_record_without_the_tracking_the_cancel_removed() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_epilog(state.path()).await;
        let admissions = svc.admissions();
        let store = crate::stepd::StepdStore::new(state.path());
        let while_held = a_cancellable_supervised_run(&svc, 55, a_live_supervisor()).await;
        // Installed so the drop below has a tracking entry to take away; without
        // one the assertion that it is gone is a statement about an empty map.
        let tracked = a_tracked_supervisor_session(55, 1, state.path());
        svc.stepds
            .lock()
            .await
            .insert(stepd_key(&tracked), tracked.clone());

        svc.drop_tracked_job(55, 1).await;

        assert!(
            svc.stepds.lock().await.is_empty(),
            "the cancel owns no channel the outcome could have come back on"
        );
        assert_eq!(
            svc.allocation.lock().await.free_cpus(),
            while_held,
            "and the cores stay put until the ledger speaks, tracking or no tracking"
        );

        store
            .prepare_session_dir(55, 1, spur_core::step::STEP_BATCH)
            .expect("session dir");
        store
            .obligations(55, 1, spur_core::step::STEP_BATCH)
            .append(&crate::stepd::StepdObligation::EpilogCompleted { failed: false })
            .expect("record the epilog");
        resolve_supervised_epilogs(&store, &admissions, &mut HashMap::new()).await;
        settle_cancelled_runs(&svc.lifecycle, &svc.allocation, &admissions).await;

        assert_eq!(
            svc.allocation.lock().await.free_cpus(),
            while_held + 2,
            "the slice comes back on the supervisor's own word"
        );
    }

    // An agent that dies mid-hook leaves a debt nothing alive owes. Its restart
    // settles it on proof the owner is gone, or the cores never come back.
    #[tokio::test]
    async fn a_cancelled_run_gets_its_cores_back_after_an_agent_restart() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_epilog(state.path()).await;
        let admissions = svc.admissions();
        let while_held = a_cancellable_supervised_run(&svc, 54, a_live_supervisor()).await;
        svc.drop_tracked_job(54, 1).await;
        its_supervisor_has_since_died(&admissions, key(54, 1), spur_core::step::STEP_BATCH);
        assert_eq!(
            svc.allocation.lock().await.free_cpus(),
            while_held,
            "the cancel leaves the debt behind that could strand the cores"
        );

        // What replay_admitted_allocations does before it rebuilds the index.
        admissions
            .settle_hooks_whose_owner_is_gone(hook_owner_did_not_survive_restart)
            .expect("settle the hooks of a previous agent");
        settle_cancelled_runs(&svc.lifecycle, &svc.allocation, &admissions).await;

        assert_eq!(
            svc.allocation.lock().await.free_cpus(),
            while_held + 2,
            "a hook whose owner did not survive must not strand the cores"
        );
    }

    // A supervisor outlives the agent that spawned it, so the restart sweep is
    // not proof its hook ended. Settling one here frees cores it stands on.
    #[tokio::test]
    async fn an_agent_restart_leaves_a_live_supervisors_hook_alone() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_epilog(state.path()).await;
        let admissions = svc.admissions();
        let while_held = a_cancellable_supervised_run(&svc, 57, a_live_supervisor()).await;
        svc.drop_tracked_job(57, 1).await;

        admissions
            .settle_hooks_whose_owner_is_gone(hook_owner_did_not_survive_restart)
            .expect("sweep the hooks of a previous agent");
        settle_cancelled_runs(&svc.lifecycle, &svc.allocation, &admissions).await;

        assert_eq!(
            admissions
                .load_run(key(57, 1))
                .expect("record")
                .cleanup
                .epilog,
            crate::admission::HookState::Pending,
            "a restart says nothing about a supervisor that survived it"
        );
        assert_eq!(
            svc.allocation.lock().await.free_cpus(),
            while_held,
            "and the cores stay under the hook that is still running"
        );
    }

    /// A run whose supervisor could not be recorded at launch. The supervisor is
    /// live and will run the epilog; only the name of it is missing.
    async fn a_cancellable_run_that_named_no_supervisor(svc: &AgentService, job_id: u32) -> u32 {
        admit_a_supervised_run(
            &svc.admissions(),
            &svc.reporter.hostname,
            job_id,
            vec![0, 1],
            None,
        );
        let while_held = {
            let mut alloc = svc.allocation.lock().await;
            alloc
                .allocate_for_job(job_id, 1, 2, 1_000, &[])
                .expect("allocate");
            alloc.commit_job(job_id, 1);
            alloc.free_cpus()
        };
        svc.insert_test_job(job_id, an_allocation_only_job(1)).await;
        while_held
    }

    // One failed write at launch leaves a live supervisor unnamed. Reading that
    // silence as "no hook is owed" hands the slice back under a live epilog.
    #[tokio::test]
    async fn a_cancelled_run_whose_supervisor_was_never_recorded_holds_its_slice() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_epilog(state.path()).await;
        let while_held = a_cancellable_run_that_named_no_supervisor(&svc, 63).await;

        svc.drop_tracked_job(63, 1).await;

        assert_eq!(
            svc.admissions()
                .load_run(key(63, 1))
                .expect("record")
                .cleanup
                .epilog,
            crate::admission::HookState::Pending,
            "an unnamed owner is no evidence that no hook is owed"
        );
        assert_eq!(
            svc.allocation.lock().await.free_cpus(),
            while_held,
            "so the cores stay under the epilog that may still be running"
        );
    }

    // The way out of the hold above, without which it would never end: a run
    // that named nobody had the previous agent's hook, and it did not survive.
    #[tokio::test]
    async fn a_restart_settles_a_hook_whose_run_named_no_supervisor() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_epilog(state.path()).await;
        let admissions = svc.admissions();
        let while_held = a_cancellable_run_that_named_no_supervisor(&svc, 64).await;
        svc.drop_tracked_job(64, 1).await;

        admissions
            .settle_hooks_whose_owner_is_gone(hook_owner_did_not_survive_restart)
            .expect("sweep the hooks of a previous agent");
        settle_cancelled_runs(&svc.lifecycle, &svc.allocation, &admissions).await;

        assert_eq!(
            svc.allocation.lock().await.free_cpus(),
            while_held + 2,
            "a hook nothing is left to run must not strand the cores"
        );
    }

    // The RPC is a separate site from the tracked-job drop, and has its own
    // release. It has to answer to the same gate.
    #[tokio::test]
    async fn cancel_job_holds_the_slice_while_a_supervisor_owes_an_epilog() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_epilog(state.path()).await;
        let admissions = svc.admissions();
        let while_held = a_cancellable_supervised_run(&svc, 56, a_live_supervisor()).await;
        // Nothing tracked is what sends the RPC down its own release path.
        svc.running.lock().await.remove(&56);

        let mut req = Request::new(AgentCancelJobRequest {
            job_id: 56,
            signal: 9,
            run_attempt: 1,
        });
        req.extensions_mut().insert(controller_identity());
        svc.cancel_job(req).await.expect("cancel");

        assert_eq!(
            svc.allocation.lock().await.free_cpus(),
            while_held,
            "the RPC must not free cores a hook is still running on"
        );
        assert_eq!(
            admissions
                .load_run(key(56, 1))
                .expect("record")
                .cleanup
                .epilog,
            crate::admission::HookState::Pending
        );
    }

    // A cancel of a job still being torn down leaves the record for the supervisor
    // to finish, and the hook it owes has to be written down there and then.
    #[tokio::test]
    async fn a_cancel_of_a_tracked_run_records_the_epilog_its_supervisor_owes() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_epilog(state.path()).await;
        let admissions = svc.admissions();
        let while_held = a_cancellable_supervised_run(&svc, 71, a_live_supervisor()).await;
        // A registered supervisor is what leaves the job tracked through the
        // signal, which is the branch this covers.
        register_a_supervisor_session(&svc, 71, state.path()).await;

        let mut req = Request::new(AgentCancelJobRequest {
            job_id: 71,
            signal: 9,
            run_attempt: 1,
        });
        req.extensions_mut().insert(controller_identity());
        svc.cancel_job(req).await.expect("cancel");

        assert!(
            svc.running.lock().await.contains_key(&71),
            "a supervised job stays tracked through its cancel"
        );
        let run = admissions.load_run(key(71, 1)).expect("record");
        assert_eq!(
            run.cleanup.epilog,
            crate::admission::HookState::Pending,
            "a tracked cancel that records no hook is a gate nothing can close"
        );
        assert_eq!(
            svc.allocation.lock().await.free_cpus(),
            while_held,
            "and the cancel itself must not hand the cores back"
        );
    }

    // The acknowledgement is the controller's word about its own accounting, not
    // the hook's about the cores. Taken for both, it freed under a running epilog.
    #[tokio::test]
    async fn an_acknowledged_completion_does_not_settle_an_epilog_still_in_flight() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_epilog(state.path()).await;
        let admissions = svc.admissions();
        let while_held = a_cancellable_supervised_run(&svc, 72, a_live_supervisor()).await;
        // Cancelled and still admitted, which is where a tracked cancel leaves it.
        admissions
            .mark_controller_cancelled(key(72, 1))
            .expect("mark cancelled");
        admissions
            .record_epilog(key(72, 1), crate::admission::HookState::Pending)
            .expect("record the owed hook");

        let freed = settle_acknowledged_completion(
            &svc.allocation,
            &admissions,
            key(72, 1),
            spur_core::step::STEP_BATCH,
        )
        .await;

        assert!(
            !freed,
            "the hook, not the acknowledgement, frees these cores"
        );
        assert_eq!(svc.allocation.lock().await.free_cpus(), while_held);
        assert!(admissions
            .load_run(key(72, 1))
            .expect("record")
            .cleanup
            .epilog
            .is_in_flight());
    }

    // A gate that never opens is worse than one that opens early. The hold an
    // acknowledgement declines has to end when the supervisor's ledger answers.
    #[tokio::test]
    async fn the_hold_an_acknowledgement_declined_ends_when_the_epilog_answers() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_epilog(state.path()).await;
        let admissions = svc.admissions();
        let store = crate::stepd::StepdStore::new(state.path());
        let while_held = a_cancellable_supervised_run(&svc, 73, a_live_supervisor()).await;
        register_a_supervisor_session(&svc, 73, state.path()).await;
        let mut req = Request::new(AgentCancelJobRequest {
            job_id: 73,
            signal: 9,
            run_attempt: 1,
        });
        req.extensions_mut().insert(controller_identity());
        svc.cancel_job(req).await.expect("cancel");
        settle_acknowledged_completion(
            &svc.allocation,
            &admissions,
            key(73, 1),
            spur_core::step::STEP_BATCH,
        )
        .await;
        assert_eq!(
            svc.allocation.lock().await.free_cpus(),
            while_held,
            "the acknowledgement must find the hook still holding"
        );

        store
            .prepare_session_dir(73, 1, spur_core::step::STEP_BATCH)
            .expect("session dir");
        let obligations = store.obligations(73, 1, spur_core::step::STEP_BATCH);
        obligations
            .append(&crate::stepd::StepdObligation::ExitObserved {
                exit_code: 0,
                signal: 9,
            })
            .expect("record the observed exit");
        obligations
            .append(&crate::stepd::StepdObligation::EpilogCompleted { failed: false })
            .expect("record the finished epilog");
        resolve_supervised_epilogs(&store, &admissions, &mut HashMap::new()).await;
        release_due_allocations(&svc.allocation, &admissions).await;

        assert_eq!(
            svc.allocation.lock().await.free_cpus(),
            while_held + 2,
            "once the hook has answered nothing may keep the cores"
        );
    }

    // A second cancel must not put a hook that has already answered back in
    // flight: nothing re-runs one, so that hold would have no way out.
    #[tokio::test]
    async fn a_cancel_does_not_re_hold_a_run_whose_epilog_already_answered() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_epilog(state.path()).await;
        let admissions = svc.admissions();
        a_cancellable_supervised_run(&svc, 74, a_live_supervisor()).await;
        register_a_supervisor_session(&svc, 74, state.path()).await;
        admissions
            .record_epilog(key(74, 1), crate::admission::HookState::Succeeded)
            .expect("record the finished hook");

        let mut req = Request::new(AgentCancelJobRequest {
            job_id: 74,
            signal: 9,
            run_attempt: 1,
        });
        req.extensions_mut().insert(controller_identity());
        svc.cancel_job(req).await.expect("cancel");

        assert_eq!(
            admissions
                .load_run(key(74, 1))
                .expect("record")
                .cleanup
                .epilog,
            crate::admission::HookState::Succeeded
        );
    }

    // The record is the only thing that can say a hook is owed, so a participant
    // file the node cannot parse is not its word that none is.
    #[tokio::test]
    async fn a_cancel_holds_when_the_record_of_its_supervisor_cannot_be_read() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_epilog(state.path()).await;
        let admissions = svc.admissions();
        let while_held = a_cancellable_supervised_run(&svc, 58, a_live_supervisor()).await;
        // The file naming the supervisor, corrupted: readable as neither a
        // participant nor an absence of one.
        let participant = admissions
            .participant_path(key(58, 1), spur_core::step::STEP_BATCH)
            .expect("participant path");
        std::fs::write(&participant, b"{not json").expect("corrupt the participant");

        svc.drop_tracked_job(58, 1).await;

        assert_eq!(
            svc.allocation.lock().await.free_cpus(),
            while_held,
            "an unreadable record must not read as a run that owes no hook"
        );
        assert_eq!(
            admissions
                .load_run(key(58, 1))
                .expect("record")
                .cleanup
                .epilog,
            crate::admission::HookState::Pending
        );
    }

    // The read can fail outright rather than per-file. What it writes outlives the
    // failure, so a cancel that read nothing must not leave a record saying so.
    #[tokio::test]
    async fn a_cancel_that_could_not_list_its_participants_still_records_the_debt() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_epilog(state.path()).await;
        let admissions = svc.admissions();
        let while_held = a_cancellable_supervised_run(&svc, 59, a_live_supervisor()).await;
        // A file where the directory belongs: every listing of it fails, and
        // none of those failures is a NotFound the caller may read as "none".
        let dir = admissions
            .participant_path(key(59, 1), spur_core::step::STEP_BATCH)
            .expect("participant path")
            .parent()
            .expect("participants dir")
            .to_path_buf();
        std::fs::remove_dir_all(&dir).expect("drop the participants dir");
        std::fs::write(&dir, b"").expect("put a file in its place");

        svc.drop_tracked_job(59, 1).await;

        assert_eq!(
            admissions
                .load_run(key(59, 1))
                .expect("record")
                .cleanup
                .epilog,
            crate::admission::HookState::Pending,
            "a listing that could not be completed is not a run that owes no hook"
        );
        // Readable again, and the record written above is now the only account of
        // what the cancel could not see.
        std::fs::remove_file(&dir).expect("take the file away");
        admit_a_participant_step(
            &admissions,
            &svc.reporter.hostname,
            59,
            spur_core::step::STEP_BATCH,
            Some(a_live_supervisor()),
        );
        settle_cancelled_runs(&svc.lifecycle, &svc.allocation, &admissions).await;

        assert_eq!(
            svc.allocation.lock().await.free_cpus(),
            while_held,
            "and the slice stays until the hook it could not rule out reports"
        );
    }

    // The cancel reads the tracking map and acts on it much later. A launch of
    // the same id must not land in between, or it is committed and then freed.
    #[tokio::test]
    async fn a_cancel_waits_for_a_launch_that_owns_the_job_id() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = Arc::new(svc_with_state_dir(state.path()).await);
        let while_held = a_cancellable_supervised_run(&svc, 60, a_live_supervisor()).await;
        // Nothing tracked is what sends the RPC down its own release path.
        svc.running.lock().await.remove(&60);

        // Stands in for a launch of the same id: it owns the id until it is done.
        let launching = svc.lifecycle.acquire(60).await;
        let cancel = tokio::spawn({
            let svc = Arc::clone(&svc);
            async move {
                let mut req = Request::new(AgentCancelJobRequest {
                    job_id: 60,
                    signal: 0,
                    run_attempt: 0,
                });
                req.extensions_mut().insert(controller_identity());
                svc.cancel_job(req).await.expect("cancel");
            }
        });
        // Long enough for a cancel that does not wait on the id to have read the
        // tracking map; one that does is still parked ahead of the read.
        for _ in 0..16 {
            tokio::task::yield_now().await;
            tokio::task::spawn_blocking(|| ())
                .await
                .expect("blocking pool");
        }

        // The launch finishing: its job is tracked before it lets the id go.
        svc.insert_test_job(60, an_allocation_only_job(1)).await;
        drop(launching);
        cancel.await.expect("the cancel runs once the id is free");

        assert_eq!(
            svc.allocation.lock().await.free_cpus(),
            while_held,
            "a cancel that read the map before the launch wrote it must not free its slice"
        );
    }

    // A reservation whose launch never wrote a record has no hook to wait on, and
    // under the id's guard its absence is settled rather than merely not yet.
    #[tokio::test]
    async fn a_cancel_releases_a_reservation_nothing_was_ever_spawned_against() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_epilog(state.path()).await;
        let while_held = {
            let mut alloc = svc.allocation.lock().await;
            alloc
                .allocate_for_job(65, 1, 2, 1_000, &[])
                .expect("allocate");
            alloc.commit_job(65, 1);
            alloc.free_cpus()
        };

        let mut req = Request::new(AgentCancelJobRequest {
            job_id: 65,
            signal: 0,
            run_attempt: 0,
        });
        req.extensions_mut().insert(controller_identity());
        svc.cancel_job(req).await.expect("cancel");

        assert_eq!(
            svc.allocation.lock().await.free_cpus(),
            while_held + 2,
            "a claim with no record behind it must not be held for a hook that cannot exist"
        );
    }

    // A numbered step runs no epilog and still reports one as not failed, so
    // letting it answer for the run settles a hook that never started.
    #[test]
    fn a_numbered_steps_word_is_never_the_runs_epilog() {
        let state = tempfile::tempdir().expect("state dir");
        let admissions = crate::admission::AdmissionStore::new(state.path(), "n1");
        admit_a_supervised_run(&admissions, "n1", 61, vec![0], Some(a_live_supervisor()));
        admissions
            .mark_run_cleaned(key(61, 1), crate::admission::EpilogOwed::Yes)
            .expect("the debt the teardown leaves");

        record_run_epilog(
            &admissions,
            key(61, 1),
            0,
            crate::admission::HookState::Succeeded,
        );

        assert_eq!(
            admissions
                .load_run(key(61, 1))
                .expect("record")
                .cleanup
                .epilog,
            crate::admission::HookState::Pending,
            "a step that ran no hook cannot report one as finished"
        );
        record_run_epilog(
            &admissions,
            key(61, 1),
            spur_core::step::STEP_BATCH,
            crate::admission::HookState::Succeeded,
        );
        assert_eq!(
            admissions
                .load_run(key(61, 1))
                .expect("record")
                .cleanup
                .epilog,
            crate::admission::HookState::Succeeded,
            "and the step that owns the hook still answers for it"
        );
    }

    // The ledger is read per step, and a numbered step's says the hook passed
    // whatever it did. A run whose owner step is unnamed must not fall to it.
    #[tokio::test]
    async fn the_runs_epilog_is_read_from_the_step_that_owns_the_hook() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_epilog(state.path()).await;
        let admissions = svc.admissions();
        let store = crate::stepd::StepdStore::new(state.path());
        let while_held = a_cancellable_supervised_run(&svc, 62, a_live_supervisor()).await;
        // A numbered step of the same run, which sorts below the batch step, and
        // a record from before the owner was named: the shape that picks it.
        admit_a_participant_step(&admissions, "n1", 62, 0, Some(a_live_supervisor()));
        let mut run = admissions.load_run(key(62, 1)).expect("record");
        run.lifecycle_owner_step = None;
        admissions.admit_run(&run).expect("re-admit");
        svc.drop_tracked_job(62, 1).await;

        // What a numbered step's supervisor writes: it ran no hook, and says so
        // as a hook that did not fail.
        store.prepare_session_dir(62, 1, 0).expect("session dir");
        store
            .obligations(62, 1, 0)
            .append(&crate::stepd::StepdObligation::EpilogCompleted { failed: false })
            .expect("record the step's epilog");
        resolve_supervised_epilogs(&store, &admissions, &mut HashMap::new()).await;
        settle_cancelled_runs(&svc.lifecycle, &svc.allocation, &admissions).await;

        assert_eq!(
            svc.allocation.lock().await.free_cpus(),
            while_held,
            "a numbered step's ledger must not release the job's slice"
        );
    }

    // A ledger corrupt anywhere but its last line never parses again, so holding
    // on the read alone is a hold with no way out.
    #[tokio::test]
    async fn an_unreadable_ledger_settles_once_its_supervisor_is_proven_gone() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_epilog(state.path()).await;
        let admissions = svc.admissions();
        let store = crate::stepd::StepdStore::new(state.path());
        let while_held = a_cancellable_supervised_run(&svc, 63, a_live_supervisor()).await;
        svc.drop_tracked_job(63, 1).await;
        its_supervisor_has_since_died(&admissions, key(63, 1), spur_core::step::STEP_BATCH);
        store
            .prepare_session_dir(63, 1, spur_core::step::STEP_BATCH)
            .expect("session dir");
        let obligations = store.obligations(63, 1, spur_core::step::STEP_BATCH);
        obligations
            .append(&crate::stepd::StepdObligation::ExitObserved {
                exit_code: 0,
                signal: 0,
            })
            .expect("record the exit");
        // Corruption before the final newline: the tail-tolerant read cannot
        // skip it, so this log is unreadable for good.
        let path = obligations.path().to_path_buf();
        let mut contents = std::fs::read_to_string(&path).expect("read the log");
        contents.insert_str(0, "{not json\n");
        std::fs::write(&path, contents).expect("corrupt the log");

        resolve_supervised_epilogs(&store, &admissions, &mut HashMap::new()).await;
        settle_cancelled_runs(&svc.lifecycle, &svc.allocation, &admissions).await;

        assert_eq!(
            svc.allocation.lock().await.free_cpus(),
            while_held + 2,
            "a ledger nobody is left to write must not hold the cores for good"
        );
    }

    /// An `srun --pty` allocation as the RPC leaves it, plus the supervisor
    /// identity a unit test cannot spawn a real `spurstepd` to write.
    async fn a_registered_allocation(svc: &AgentService, job_id: u32) -> u32 {
        svc.register_job_allocation(Request::new(RegisterJobAllocationRequest {
            job_id,
            cpus: 2,
            run_attempt: 1,
            allocated: Some(ResourceAllocations {
                cpus: 2,
                memory_mb: 512,
                devices: std::collections::HashMap::new(),
            }),
            ..Default::default()
        }))
        .await
        .expect("register the allocation");
        svc.admissions()
            .record_supervisor(
                key(job_id, 1),
                spur_core::step::STEP_EXTERN,
                a_live_supervisor(),
            )
            .expect("record the allocation supervisor");
        svc.allocation.lock().await.free_cpus()
    }

    // The allocation path launches nothing the controller told it to, so its
    // record was the one nobody wrote — and a cancel read that as owing no hook.
    #[tokio::test]
    async fn a_registered_allocation_names_the_step_that_owns_its_lifetime() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_epilog(state.path()).await;
        a_registered_allocation(&svc, 70).await;

        let admissions = svc.admissions();
        assert_eq!(
            admissions
                .load_run(key(70, 1))
                .expect("record")
                .lifecycle_owner_step,
            Some(spur_core::step::STEP_EXTERN),
            "an allocation's own step has to be the one a release is keyed on"
        );
        let (admitted, rejected) = admissions.load_admitted(key(70, 1)).expect("participants");
        assert!(rejected.is_empty());
        assert_eq!(
            hook_owner_step(&admitted),
            Some(spur_core::step::STEP_EXTERN),
            "without a participant the run has no step that could answer for a hook"
        );
    }

    // Measured live: a cancelled pty job handed its cores to the next job 321ms
    // in, while its own epilog still had ~29s left to run on them.
    #[tokio::test]
    async fn a_cancelled_allocation_holds_its_cores_until_its_epilog_ends() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_epilog(state.path()).await;
        let admissions = svc.admissions();
        let store = crate::stepd::StepdStore::new(state.path());
        let while_held = a_registered_allocation(&svc, 71).await;

        svc.drop_tracked_job(71, 1).await;
        assert_eq!(
            svc.allocation.lock().await.free_cpus(),
            while_held,
            "cores an epilog is still running on are not free"
        );

        store
            .prepare_session_dir(71, 1, spur_core::step::STEP_EXTERN)
            .expect("session dir");
        store
            .obligations(71, 1, spur_core::step::STEP_EXTERN)
            .append(&crate::stepd::StepdObligation::EpilogCompleted { failed: false })
            .expect("record the epilog's outcome");
        resolve_supervised_epilogs(&store, &admissions, &mut HashMap::new()).await;
        settle_cancelled_runs(&svc.lifecycle, &svc.allocation, &admissions).await;

        assert_eq!(
            svc.allocation.lock().await.free_cpus(),
            while_held + 2,
            "and released once the hook's owner says it ended"
        );
    }

    // Nothing was ever named to answer for this hook, so no ledger can say it
    // ended. Waiting is not proof, and the slice stays put until a restart is.
    #[tokio::test]
    async fn an_epilog_whose_owner_was_never_recorded_holds_its_slice() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_epilog(state.path()).await;
        let admissions = svc.admissions();
        let store = crate::stepd::StepdStore::new(state.path());

        admit_a_supervised_run(&admissions, &svc.reporter.hostname, 74, vec![0, 1], None);
        {
            let mut alloc = svc.allocation.lock().await;
            alloc
                .allocate_for_job(74, 1, 2, 1_000, &[])
                .expect("allocate");
            alloc.commit_job(74, 1);
        }
        svc.insert_test_job(74, an_allocation_only_job(1)).await;
        let while_held = svc.allocation.lock().await.free_cpus();

        svc.drop_tracked_job(74, 1).await;
        let admitted = admissions
            .load_all()
            .expect("the ledger")
            .runs
            .into_iter()
            .find(|admitted| admitted.run.job_id == 74)
            .expect("the record");
        assert!(
            hook_owner_step(&admitted).is_none(),
            "no participant carries a supervisor, so no step owns the hook"
        );
        assert!(admitted.run.cleanup.epilog.is_in_flight());

        resolve_supervised_epilogs(&store, &admissions, &mut HashMap::new()).await;
        settle_cancelled_runs(&svc.lifecycle, &svc.allocation, &admissions).await;

        assert_eq!(
            svc.allocation.lock().await.free_cpus(),
            while_held,
            "an epilog no one can answer for must not hand its cores on"
        );
        assert!(
            admissions
                .load_run(key(74, 1))
                .expect("the record")
                .cleanup
                .epilog
                .is_in_flight(),
            "and the hook must still read as unfinished"
        );
    }

    // The control for the hold above: a gate that refuses everything reads the
    // same as one that refuses only what a hook is standing on.
    #[tokio::test]
    async fn a_cancelled_allocation_with_no_epilog_releases_at_once() {
        let state = tempfile::tempdir().expect("state dir");
        let svc = svc_with_state_dir(state.path()).await;
        let while_held = a_registered_allocation(&svc, 72).await;

        svc.drop_tracked_job(72, 1).await;

        assert_eq!(
            svc.allocation.lock().await.free_cpus(),
            while_held + 2,
            "a node that runs no epilog has nothing to wait for"
        );
    }

    // Holding for a hook with no deadline is right, and silent is not: the one
    // line a cancel logs scrolls away long before an operator looks.
    #[test]
    fn a_hold_that_outlasts_the_threshold_is_said_again() {
        let mut held_since = HashMap::new();
        let run = key(64, 1);

        assert!(
            !epilog_hold_is_worth_saying(&mut held_since, run),
            "a hook that just started is not news"
        );

        let stale = std::time::Instant::now()
            .checked_sub(EPILOG_HOLD_STUCK_AFTER)
            .expect("a threshold the clock can reach back past");
        held_since.insert(run, stale);
        assert!(
            epilog_hold_is_worth_saying(&mut held_since, run),
            "one that has held past the threshold is"
        );
        assert!(
            !epilog_hold_is_worth_saying(&mut held_since, run),
            "and saying it restarts its clock rather than repeating every tick"
        );
    }

    // A stale-epoch drop (peeked before a concurrent redispatch retracked the
    // same job_id under a newer attempt) must not evict the new tracking.
    #[tokio::test]
    async fn drop_tracked_job_skips_a_reused_job_id() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        let job_id = 903;
        svc.insert_test_job(job_id, an_allocation_only_job(1)).await;
        svc.insert_test_job(job_id, an_allocation_only_job(2)).await;

        svc.drop_tracked_job(job_id, 1).await;

        assert_eq!(
            svc.running
                .lock()
                .await
                .get(&job_id)
                .map(|job| job.run_attempt),
            Some(2),
            "a stale-epoch drop must not evict a newer, already-retracked job"
        );
    }

    // The running-map guard passing (a stale caller genuinely still matches
    // running) must not let a stale drop also evict a stepds entry
    // that a redispatch already retracked under a newer attempt.
    #[tokio::test]
    async fn drop_tracked_job_skips_a_reused_stepd() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        let job_id = 904;
        svc.insert_test_job(job_id, an_allocation_only_job(1)).await;

        let newer = crate::stepd::StepdDescriptor::new(
            job_id,
            2,
            spur_core::step::STEP_BATCH,
            0,
            0,
            std::path::PathBuf::from("/tmp/runtime.sock"),
            std::path::PathBuf::new(),
        );
        svc.stepds
            .lock()
            .await
            .insert(stepd_key(&newer), newer.clone());

        svc.drop_tracked_job(job_id, 1).await;

        assert_eq!(
            svc.stepds
                .lock()
                .await
                .get(&(job_id, spur_core::step::STEP_BATCH)),
            Some(&newer)
        );
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
            reaped: false,
        };
        reap_killed_job(job).await;

        // After reaping, the pid is gone from the process table entirely.
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "reap_killed_job must reap the Forked child (pid {pid} still present)"
        );
    }

    /// Aborting a supervised launch spawns a reaper over an allocation, whose
    /// `try_wait` never settles — it must not poll for the life of the agent.
    #[tokio::test(start_paused = true)]
    async fn reap_killed_job_returns_for_an_allocation_with_no_process() {
        // Paused time: a looping reaper burns its virtual sleeps instantly, so
        // this resolves without ever depending on how fast the machine is.
        let reaped = tokio::time::timeout(
            std::time::Duration::from_secs(3600),
            reap_killed_job(executor::RunningJob::AllocationOnly),
        )
        .await;

        assert!(
            reaped.is_ok(),
            "reap_killed_job must return for an allocation instead of polling forever"
        );
    }

    /// The reclaimed-reservation abort has to reach a supervised workload, which
    /// `kill_signal` cannot touch: only stopping the supervisor ends the run.
    #[tokio::test]
    async fn discarding_a_stepd_session_stops_the_supervisor_and_its_state() {
        let state = tempfile::tempdir().expect("runtime state directory");
        let store = crate::stepd::StepdStore::new(state.path());
        let session_dir = store
            .prepare_session_dir(77, 3, spur_core::step::STEP_BATCH)
            .expect("session directory");
        // Named job_* so cleanup_cgroup accepts it as one of ours.
        let cgroup_dir = state.path().join("job_77_3");
        std::fs::create_dir_all(&cgroup_dir).expect("stand-in cgroup directory");

        let mut child = std::process::Command::new("sleep")
            .arg("300")
            .spawn()
            .expect("spawn a stand-in supervisor");
        let pid = child.id();
        let descriptor = crate::stepd::StepdDescriptor::new(
            77,
            3,
            spur_core::step::STEP_BATCH,
            pid,
            crate::stepd::process_start_ticks(pid).expect("start ticks"),
            session_dir.join("runtime.sock"),
            cgroup_dir.clone(),
        );
        assert_eq!(
            crate::stepd::stepd_liveness(&descriptor).expect("liveness check"),
            crate::stepd::StepdLiveness::Live,
            "the stand-in supervisor must be live before the abort tears it down"
        );

        discard_stepd_session(&descriptor).await;

        assert_eq!(
            await_proc_state(pid as i32, &['Z', 'X']).await,
            'Z',
            "the abort must stop the supervisor, not leave it parked"
        );
        assert!(
            !cgroup_dir.exists(),
            "the abort must reap the abandoned supervisor's cgroup"
        );
        assert!(
            !session_dir.exists(),
            "the abort must drop the abandoned session's on-disk state"
        );
        let _ = child.wait();
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

        svc.send_explicit_signal(job_id, 0, 9).await; // cleanup
    }

    #[tokio::test]
    async fn a_signal_for_a_superseded_run_spares_the_redispatch() {
        // The same job id runs again on this node after a requeue. A delayed
        // kill for the run it replaced must not land on the new one.
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        svc.start_monitor("http://127.0.0.1:1".into());

        let job_id = 903;
        let mut redispatch = TrackedJob::dummy(0);
        redispatch.run_attempt = 2;
        svc.insert_test_job(job_id, redispatch).await;

        svc.send_explicit_signal(job_id, 1, 9).await;

        assert!(
            !wait_job_reaped(&svc, job_id, 2_000).await,
            "a kill aimed at the run this one replaced must not reap it"
        );

        svc.send_explicit_signal(job_id, 2, 9).await;
        assert!(
            wait_job_reaped(&svc, job_id, 5_000).await,
            "naming the running epoch must still kill it"
        );
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

        svc.send_explicit_signal(job_id, 0, 9).await; // SIGKILL

        assert!(
            wait_job_reaped(&svc, job_id, 5_000).await,
            "monitor should reap SIGKILL'd job within 5s"
        );
    }

    // A finished job's cgroup must be released exactly once, by the monitor
    // loop taking it off the tracked job.
    #[tokio::test]
    async fn monitor_removes_the_job_cgroup_on_completion() {
        let dir = tempfile::tempdir().unwrap();
        let cgroup = dir.path().join("job_904");
        std::fs::create_dir(&cgroup).unwrap();

        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        svc.start_monitor("http://127.0.0.1:1".into());

        let job_id = 904;
        let mut tracked = TrackedJob::dummy(0);
        tracked.cgroup_path = Some(cgroup.clone());
        svc.insert_test_job(job_id, tracked).await;

        svc.send_explicit_signal(job_id, 0, 9).await; // SIGKILL

        assert!(
            wait_job_reaped(&svc, job_id, 5_000).await,
            "monitor should reap SIGKILL'd job within 5s"
        );
        assert!(
            !cgroup.exists(),
            "completion must remove the finished job's cgroup"
        );
    }

    fn completed_job(job_id: u32, cgroup: std::path::PathBuf) -> CompletedJob {
        CompletedJob {
            job_id,
            exit_code: 0,
            signal: 0,
            run_attempt: 0,
            rootfs_mode: crate::container::RootfsMode::Extracted,
            cgroup: Some(cgroup),
            work_dir: "/tmp".into(),
            uid: 0,
            gid: 0,
            partition: String::new(),
            gpu_devices: Vec::new(),
            cpus: 1,
            memory_mb: 0,
            nodelist: String::new(),
            mpi: String::new(),
        }
    }

    // The monitor drops the job from `running` before tearing it down, so the
    // controller can re-dispatch the id into that window and own the same state.
    #[tokio::test]
    async fn teardown_spares_a_job_id_re_dispatched_during_the_window() {
        let dir = tempfile::tempdir().unwrap();
        let cgroup = dir.path().join("job_907");
        std::fs::create_dir(&cgroup).unwrap();

        let svc = AgentService::new(
            test_reporter_with_gpus(&[0]),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        let job_id = 907;
        let completed = completed_job(job_id, cgroup.clone());

        // The re-dispatch: tracked again under the same id, holding the GPU.
        svc.insert_test_job(job_id, TrackedJob::allocation_only(Some(cgroup.clone())))
            .await;
        {
            let mut alloc = svc.allocation.lock().await;
            alloc.allocate_for_job(job_id, 1, 1, 0, &[0]).unwrap();
            alloc.commit_job(job_id, 1);
        }

        teardown_completed_job(
            &completed,
            &svc.lifecycle,
            &svc.running,
            &svc.allocation,
            &svc.mpi_host,
            &svc.admissions(),
            crate::admission::EpilogOwed::No,
        )
        .await;

        assert!(
            cgroup.exists(),
            "teardown must not remove the cgroup the re-dispatched run is in"
        );
        assert_eq!(
            svc.free_gpu_count().await,
            0,
            "teardown must not release the re-dispatched run's reservation"
        );
    }

    // The counterpart: with no live run under the id, the same teardown must
    // still release everything, so the guard above cannot just skip always.
    #[tokio::test]
    async fn teardown_cleans_up_without_releasing_the_slice() {
        let dir = tempfile::tempdir().unwrap();
        let cgroup = dir.path().join("job_908");
        std::fs::create_dir(&cgroup).unwrap();

        let svc = AgentService::new(
            test_reporter_with_gpus(&[0]),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        let job_id = 908;
        let completed = completed_job(job_id, cgroup.clone());
        {
            let mut alloc = svc.allocation.lock().await;
            alloc.allocate_for_job(job_id, 1, 1, 0, &[0]).unwrap();
            alloc.commit_job(job_id, 1);
        }

        teardown_completed_job(
            &completed,
            &svc.lifecycle,
            &svc.running,
            &svc.allocation,
            &svc.mpi_host,
            &svc.admissions(),
            crate::admission::EpilogOwed::No,
        )
        .await;

        assert!(
            !cgroup.exists(),
            "teardown must remove the finished run's cgroup"
        );
        // Teardown cleans up but does not free the slice: only the controller's
        // committed completion does.
        assert_eq!(
            svc.free_gpu_count().await,
            0,
            "teardown must not release ahead of the acknowledgement"
        );
    }

    // An allocation never completes, so the monitor loop's teardown never runs
    // for it. Cancelling is its only chance to release the cgroup.
    #[tokio::test]
    async fn cancelling_an_allocation_removes_its_cgroup() {
        let dir = tempfile::tempdir().unwrap();
        let cgroup = dir.path().join("job_905");
        std::fs::create_dir(&cgroup).unwrap();

        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        let job_id = 905;
        svc.insert_test_job(job_id, TrackedJob::allocation_only(Some(cgroup.clone())))
            .await;

        svc.graceful_cancel(job_id, 0).await;

        assert!(
            !svc.running.lock().await.contains_key(&job_id),
            "cancelling an allocation must drop the tracked job"
        );
        assert!(
            !cgroup.exists(),
            "dropping an allocation must remove its cgroup"
        );
    }

    // Re-dispatch inserts the new run under the same id and discards the
    // displaced job. Both name the same job_<id> cgroup, which the new run has
    // already joined, so discarding one must never release it.
    #[tokio::test]
    async fn displacing_a_tracked_job_leaves_its_cgroup_alone() {
        let dir = tempfile::tempdir().unwrap();
        let cgroup = dir.path().join("job_906");
        std::fs::create_dir(&cgroup).unwrap();

        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        let job_id = 906;
        svc.insert_test_job(job_id, TrackedJob::allocation_only(Some(cgroup.clone())))
            .await;
        svc.insert_test_job(job_id, TrackedJob::allocation_only(Some(cgroup.clone())))
            .await;

        assert!(
            cgroup.exists(),
            "displacing a run must not release the cgroup its successor is in"
        );
    }

    #[tokio::test]
    async fn failed_runtime_signal_keeps_the_allocation_tracked() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        let state = tempfile::tempdir().expect("runtime state directory");
        let descriptor = crate::stepd::StepdDescriptor::new(
            902,
            1,
            spur_core::step::STEP_BATCH,
            0,
            0,
            state.path().join("missing-runtime.sock"),
            std::path::PathBuf::new(),
        );
        recover_stepds(&svc.running, vec![descriptor.clone()]).await;
        svc.adopt_stepds(&[descriptor]).await;

        svc.send_explicit_signal(902, 0, nix::sys::signal::Signal::SIGTERM as i32)
            .await;
        svc.graceful_cancel(902, 0).await;

        assert!(svc.running.lock().await.contains_key(&902));
        assert!(svc
            .stepds
            .lock()
            .await
            .contains_key(&(902, spur_core::step::STEP_BATCH)));
    }

    // Two descriptors recording the same core is the corrupt case the ledger
    // guards; losing the loser's GPUs to it would be worse than the overlap.
    #[tokio::test]
    async fn a_contested_core_costs_an_adopted_job_occupancy_not_its_gpus() {
        let svc = AgentService::new(
            test_reporter_with_gpus(&[0, 1]),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        let state = tempfile::tempdir().expect("runtime state directory");
        let descriptor = |job_id: u32, cpu_ids: Vec<u32>, gpu: u32| {
            let mut descriptor = crate::stepd::StepdDescriptor::new(
                job_id,
                1,
                spur_core::step::STEP_BATCH,
                0,
                0,
                state.path().join(format!("{job_id}.sock")),
                std::path::PathBuf::new(),
            );
            descriptor.resources = crate::stepd::StepdJobResources {
                cpus: cpu_ids.len() as u32,
                memory_mb: 1024,
                gpu_devices: vec![gpu],
                cpu_ids,
                ..Default::default()
            };
            descriptor
        };
        let first = descriptor(910, vec![1, 2], 0);
        let second = descriptor(911, vec![2, 3], 1);

        svc.replay_adopted_allocations(&[first, second]).await;

        let mut alloc = svc.allocation.lock().await;
        // Job 911 yields core 2 to job 910, but its GPU and memory stay on.
        assert_eq!(alloc.allocated_gpu_ids(), vec![0, 1]);
        assert_eq!(alloc.free_gpus(None), 0);
        assert_eq!(alloc.free_memory_mb(), 8192 - 2048);
        assert!(alloc.allocated_cpus[1] && alloc.allocated_cpus[2] && alloc.allocated_cpus[3]);
        assert!(!alloc.allocated_cpus[0]);
        assert_eq!(alloc.conflicting_owners(&[1]), vec![911]);

        // Core 2 is 910's alone: had 911 replayed it verbatim, releasing 911
        // would clear a core 910 is still running on.
        alloc.release_job(ReleaseWarrant::controller_cancelled(RunKey::any_attempt(
            911,
        )));
        assert!(alloc.allocated_cpus[2]);
        assert!(!alloc.allocated_cpus[3]);
    }

    // An empty ledger was only correct while a restart killed every job: it now
    // lets a later dispatch hand out cores and GPUs an adopted job still holds.
    #[tokio::test]
    async fn an_adopted_job_puts_its_resources_back_on_the_ledger() {
        let svc = AgentService::new(
            test_reporter_with_gpus(&[0, 1]),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        let state = tempfile::tempdir().expect("runtime state directory");
        let mut descriptor = crate::stepd::StepdDescriptor::new(
            903,
            1,
            spur_core::step::STEP_BATCH,
            0,
            0,
            state.path().join("adopted.sock"),
            std::path::PathBuf::new(),
        );
        descriptor.resources = crate::stepd::StepdJobResources {
            cpus: 2,
            memory_mb: 1024,
            gpu_devices: vec![1],
            cpu_ids: vec![2, 3],
            ..Default::default()
        };

        svc.adopt_stepds(std::slice::from_ref(&descriptor)).await;
        svc.replay_adopted_allocations(std::slice::from_ref(&descriptor))
            .await;

        let alloc = svc.allocation.lock().await;
        assert_eq!(alloc.free_cpus(), 2);
        assert!(alloc.allocated_cpus[2] && alloc.allocated_cpus[3]);
        assert!(!alloc.allocated_cpus[0] && !alloc.allocated_cpus[1]);
        assert_eq!(alloc.allocated_gpu_ids(), vec![1]);
        assert_eq!(alloc.free_gpus(None), 1);
        assert_eq!(alloc.free_memory_mb(), 8192 - 1024);
    }

    // Only the batch descriptor records the cores, and sessions are discovered
    // in arbitrary order, so a step must not decide the job's replayed cpuset.
    #[tokio::test]
    async fn a_step_descriptor_does_not_displace_the_cores_its_job_recorded() {
        for step_first in [false, true] {
            let svc = AgentService::new(
                test_reporter_with_gpus(&[0, 1]),
                HooksConfig::default(),
                Arc::new(Mutex::new(DeviceRegistry::new())),
                spur_core::config::MemlockLimit::Unlimited,
            );
            let state = tempfile::tempdir().expect("runtime state directory");
            let mut batch = crate::stepd::StepdDescriptor::new(
                905,
                1,
                spur_core::step::STEP_BATCH,
                0,
                0,
                state.path().join("batch.sock"),
                std::path::PathBuf::new(),
            );
            batch.resources = crate::stepd::StepdJobResources {
                cpus: 2,
                memory_mb: 1024,
                gpu_devices: vec![0],
                cpu_ids: vec![2, 3],
                ..Default::default()
            };
            let mut step = batch.clone();
            step.step_id = 0;
            step.socket_path = state.path().join("step.sock");
            step.resources.cpu_ids = Vec::new();

            let descriptors = if step_first {
                vec![step, batch]
            } else {
                vec![batch, step]
            };
            svc.replay_adopted_allocations(&descriptors).await;

            let alloc = svc.allocation.lock().await;
            assert!(
                alloc.allocated_cpus[2] && alloc.allocated_cpus[3],
                "step_first={step_first}: the job's recorded cores must be held"
            );
            assert!(
                !alloc.allocated_cpus[0] && !alloc.allocated_cpus[1],
                "step_first={step_first}: cores the job never held must stay free"
            );
            assert_eq!(alloc.free_cpus(), 2);
        }
    }

    // Adoption replays every step of a job, and each carries the job's
    // resources: counting them once is what keeps the ledger honest.
    #[tokio::test]
    async fn replaying_an_adopted_job_twice_counts_it_once() {
        let svc = AgentService::new(
            test_reporter_with_gpus(&[0, 1]),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        let state = tempfile::tempdir().expect("runtime state directory");
        let mut batch = crate::stepd::StepdDescriptor::new(
            904,
            1,
            spur_core::step::STEP_BATCH,
            0,
            0,
            state.path().join("batch.sock"),
            std::path::PathBuf::new(),
        );
        batch.resources = crate::stepd::StepdJobResources {
            cpus: 2,
            memory_mb: 1024,
            gpu_devices: vec![0],
            cpu_ids: vec![0, 1],
            ..Default::default()
        };
        let mut step = batch.clone();
        step.step_id = 0;
        step.socket_path = state.path().join("step.sock");

        svc.replay_adopted_allocations(&[batch.clone(), step]).await;
        svc.replay_adopted_allocations(&[batch]).await;

        let alloc = svc.allocation.lock().await;
        assert_eq!(alloc.free_cpus(), 2);
        assert_eq!(alloc.free_memory_mb(), 8192 - 1024);
        assert_eq!(alloc.allocated_gpu_ids(), vec![0]);
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

    // An interactive allocation has no batch process, so its cgroup can only
    // live on the tracked job — that is the one handle a step or exec has to
    // reach the job's limits and device filter.
    #[tokio::test]
    async fn job_entry_carries_the_cgroup_of_an_allocation_only_job() {
        let svc = AgentService::new(
            test_reporter(),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );
        let job_id = 4242;
        let cgroup = std::path::PathBuf::from("/sys/fs/cgroup/spur/job_4242");
        svc.insert_test_job(job_id, TrackedJob::allocation_only(Some(cgroup.clone())))
            .await;

        let entry = svc
            .job_entry(job_id)
            .await
            .expect("job_entry should succeed");
        assert_eq!(entry.cgroup_path, Some(cgroup));
    }

    #[tokio::test]
    async fn job_entry_not_found() {
        let (svc, _) = run_command_test_setup().await;

        let err = svc.job_entry(9999).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    // The PTY bridge reaps a raw-forked container step through `waitpid_exit_code`
    // (there is no `tokio::process::Child` to `wait()` on), so exit-code mapping
    // must match what the bridge reports to the client.
    #[test]
    fn waitpid_exit_code_reports_normal_exit() {
        match unsafe { nix::unistd::fork() }.expect("fork") {
            nix::unistd::ForkResult::Child => unsafe { libc::_exit(7) },
            nix::unistd::ForkResult::Parent { child } => {
                assert_eq!(waitpid_exit_code(child), 7);
            }
        }
    }

    #[test]
    fn waitpid_exit_code_reports_signal_death_as_128_plus_signal() {
        match unsafe { nix::unistd::fork() }.expect("fork") {
            nix::unistd::ForkResult::Child => loop {
                unsafe { libc::pause() };
            },
            nix::unistd::ForkResult::Parent { child } => {
                unsafe { libc::kill(child.as_raw(), libc::SIGKILL) };
                assert_eq!(waitpid_exit_code(child), 128 + libc::SIGKILL);
            }
        }
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
        let mut child = cmd.spawn().expect("spawn cat");
        let child_pid = child.id().expect("child pid") as i32;
        drop(slave);

        let (in_tx, in_rx) =
            tokio::sync::mpsc::channel::<Result<InteractiveInput, tonic::Status>>(64);
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<
            Result<spur_proto::proto::InteractiveOutput, tonic::Status>,
        >(64);

        let inbound = tokio_stream::wrappers::ReceiverStream::new(in_rx);
        let wait_exit = async move {
            child
                .wait()
                .await
                .ok()
                .and_then(|s| s.code())
                .unwrap_or(128)
        };
        tokio::spawn(AgentService::run_pty_bridge(
            master, wait_exit, child_pid, true, inbound, out_tx,
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

    // A non-interactive client (no TTY) closes its input stream on stdin-EOF while
    // still reading output. The bridge must NOT hang the step up then — it must
    // drain the command's output and report it. Regression guard for the fix that
    // makes `srun --pty <cmd>` work in scripts/pipes/CI. The child sleeps briefly
    // before printing so a hang-up-on-input-close regression would kill it first.
    #[tokio::test]
    async fn run_pty_bridge_non_interactive_drains_after_input_close() {
        use spur_proto::proto::{interactive_output, InteractiveInput};

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
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg("sleep 0.2; printf DRAINED")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        unsafe {
            cmd.pre_exec(move || raw.wire());
        }
        let mut child = cmd.spawn().expect("spawn sh");
        let child_pid = child.id().expect("child pid") as i32;
        drop(slave);

        // Input stream closed immediately (dropped sender) = non-interactive stdin-EOF.
        let (in_tx, in_rx) =
            tokio::sync::mpsc::channel::<Result<InteractiveInput, tonic::Status>>(1);
        drop(in_tx);
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<
            Result<spur_proto::proto::InteractiveOutput, tonic::Status>,
        >(64);

        let inbound = tokio_stream::wrappers::ReceiverStream::new(in_rx);
        let wait_exit = async move {
            child
                .wait()
                .await
                .ok()
                .and_then(|s| s.code())
                .unwrap_or(128)
        };
        // interactive = false: input-close is stdin-EOF, not a hangup.
        tokio::spawn(AgentService::run_pty_bridge(
            master, wait_exit, child_pid, false, inbound, out_tx,
        ));

        let mut collected = Vec::new();
        let mut got_exit = false;
        while let Some(Ok(msg)) = out_rx.recv().await {
            match msg.msg {
                Some(interactive_output::Msg::Data(d)) => collected.extend_from_slice(&d),
                Some(interactive_output::Msg::ExitStatus(_)) => {
                    got_exit = true;
                    break;
                }
                None => {}
            }
        }
        let text = String::from_utf8_lossy(&collected);
        assert!(
            text.contains("DRAINED"),
            "non-interactive bridge dropped output after input close, got: {text:?}"
        );
        assert!(got_exit, "bridge did not report an exit status");
    }
}
