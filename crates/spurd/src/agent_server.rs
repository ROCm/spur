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

use spur_core::config::{HooksConfig, MpiConfig};
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

#[derive(Debug, Default)]
struct ActiveStep {
    cancel_requested: bool,
    pid: Option<u32>,
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

pub struct AgentService {
    pub reporter: Arc<NodeReporter>,
    /// In-memory only: starts empty on every spurd start/restart, regardless
    /// of whether the controller still reports a job Running from before.
    running: RunningJobs,
    allocation: Arc<Mutex<NodeAllocation>>,
    spank: Arc<Option<SpankHost>>,
    mpi_host: Arc<MpiPluginHost>,
    hooks: Arc<HooksConfig>,
    memlock: spur_core::config::MemlockLimit,
    #[allow(dead_code)]
    device_registry: Arc<Mutex<DeviceRegistry>>,
    /// RPC-driven owner of this node's k0s systemd unit.
    k0s: Arc<crate::cluster::K0sAgent>,
    /// In-flight srun steps keyed by `(job_id, step_id)`.
    active_steps: Arc<Mutex<HashMap<(u32, u32), ActiveStep>>>,
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
            memlock,
            MpiConfig::default(),
            new_running_jobs(),
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
        memlock: spur_core::config::MemlockLimit,
        mpi: MpiConfig,
        running: RunningJobs,
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
            memlock,
            device_registry,
            k0s: Arc::new(crate::cluster::K0sAgent::from_config(cluster)),
            active_steps: Arc::new(Mutex::new(HashMap::new())),
        }
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
                    crate::container::cleanup_rootfs(c.job_id, &c.rootfs_mode);
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

fn controller_rpc_retryable(status: &tonic::Status) -> bool {
    use tonic::Code;
    matches!(
        status.code(),
        Code::Unavailable | Code::Internal | Code::DeadlineExceeded | Code::Unknown
    )
}

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
        let req = request.into_inner();
        let job_id = req.job_id;
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
            tasks_per_node * spec.cpus_per_task.max(1),
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
            SpurEnv::apply_task_rank(&mut senv, task_offset, 0, 1);
        } else {
            senv.set("SPUR_TASK_OFFSET", task_offset);
            senv.set("LOCAL_RANK", "0");
            senv.set("LOCAL_WORLD_SIZE", tasks_per_node);
            senv.set("NPROC_PER_NODE", tasks_per_node);
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

        if !pmix_multi_task {
            // Third-party distributed training / MPI env vars
            if tasks_per_node > 1 {
                senv.set("LOCAL_RANK", "0");
                senv.set("LOCAL_WORLD_SIZE", tasks_per_node);
                senv.set("NPROC_PER_NODE", tasks_per_node);
            }
            senv.set("NODE_RANK", node_rank);

            if peer_nodes.len() > 1 {
                if let Some(first_peer) = peer_nodes.first() {
                    let master_addr = first_peer
                        .rsplit(':')
                        .nth(1)
                        .or_else(|| first_peer.split(':').next())
                        .unwrap_or(first_peer);
                    senv.set("MASTER_ADDR", master_addr);
                }
                senv.set("MASTER_PORT", "29500");
                senv.set("WORLD_SIZE", peer_nodes.len());
                senv.set("RANK", node_rank);
            }
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

            let (rootfs, rootfs_mode) =
                crate::container::setup_rootfs(&image_path, job_id, cfg.name.as_deref())
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

        let (alloc_result, allocated_device_ids) = self
            .allocate_local_resources(job_id, &spec, req.allocated.as_ref())
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
                cpus: spec.cpus_per_task.max(1),
                memory_mb: spec.memory_per_node_mb,
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

        if let Some(ref mut cfg) = container_config {
            cfg.environment = env.clone();
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
            cpus: spec.cpus_per_task.max(1),
            memory_mb: spec.memory_per_node_mb,
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
            memlock: self.memlock,
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
                            crate::container::cleanup_rootfs(job_id, &rootfs_mode);
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
            signal_step_process_group(pid, signal);
        }
        Ok(Response::new(()))
    }

    async fn suspend_job(
        &self,
        request: Request<AgentSuspendJobRequest>,
    ) -> Result<Response<()>, Status> {
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
        let req = request.into_inner();

        self.check_job_access(req.job_id, &req.user, "exec into")
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

        let priv_drop = crate::privdrop::PrivDrop::resolve_if_needed(entry.uid, entry.gid);

        let mut cmd = if entry.has_namespaces() && entry.pid > 0 {
            let mut c = tokio::process::Command::new("nsenter");
            for arg in entry.nsenter_args() {
                c.arg(arg);
            }
            if let Some(ref pd) = priv_drop {
                for arg in pd.nsenter_args() {
                    c.arg(arg);
                }
            }
            c.arg("--");
            c.arg(&req.command[0]);
            for arg in &req.command[1..] {
                c.arg(arg);
            }
            c
        } else {
            let mut c = tokio::process::Command::new(&req.command[0]);
            for arg in &req.command[1..] {
                c.arg(arg);
            }
            c.current_dir(&entry.work_dir);
            if let Some(pd) = priv_drop {
                unsafe {
                    c.pre_exec(move || {
                        pd.apply()
                            .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
                        Ok(())
                    });
                }
            }
            c
        };

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
                run_attempt: req.run_attempt,
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
        let req = request.into_inner();
        if req.command.is_empty() {
            return Err(Status::invalid_argument("no command specified"));
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
            self.active_steps
                .lock()
                .await
                .insert(step_key, ActiveStep::default());
        }
        let _active_step_guard = ActiveStepGuard {
            steps: self.active_steps.clone(),
            key: step_key,
        };

        // No retry on a miss: a step only reaches a Running job, i.e. one every
        // node already confirmed via LaunchJob (confirm_dispatch_on_nodes) — so a
        // miss is a wrong job/node pairing, not a launch race. The one uncovered
        // case is a spurd restart mid-job, which starts `running` empty.
        let (gpu_devices, partition, cpus, memory_mb, nodelist, job_mpi) = {
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
            (
                tracked.gpu_devices.clone(),
                tracked.partition.clone(),
                tracked.cpus,
                tracked.memory_mb,
                nodelist,
                tracked.mpi.clone(),
            )
        };

        let agent_hostname = self.reporter.hostname.clone();
        let node_names: Vec<&str> = nodelist.split(',').filter(|s| !s.is_empty()).collect();
        let num_nodes = node_names.len().max(1) as u32;
        let node_id = node_names
            .iter()
            .position(|n| *n == agent_hostname)
            .unwrap_or(0) as u32;

        let gpu_env = if gpu_devices.is_empty() {
            HashMap::new()
        } else {
            self.device_registry
                .lock()
                .await
                .build_job_injection_plans("gpu", &gpu_devices, req.uid, req.gid)
                .map_err(|e| {
                    Status::failed_precondition(format!("GPU injection plan failed: {}", e))
                })?
                .0
                .env
        };

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
                SpurEnv::apply_task_rank(&mut senv, req.task_offset, 0, 1);
            }
            let wrapper_path_string = wrapper_path.to_string_lossy().into_owned();
            ("bash".to_string(), vec![wrapper_path_string], Some(guard))
        } else {
            SpurEnv::apply_task_rank(&mut senv, req.task_offset, 0, 1);
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

        let mut cmd = tokio::process::Command::new(&program);
        cmd.args(&program_args)
            .current_dir(&work_dir)
            .process_group(0);
        for (k, v) in env {
            cmd.env(k, v);
        }

        let memlock = self.memlock;
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

        info!(
            command = ?req.command,
            num_tasks,
            task_offset = req.task_offset,
            uid = req.uid,
            work_dir = %work_dir,
            "RunCommand: executing step"
        );

        use std::process::Stdio;

        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Status::internal(format!("command failed: {}", e)))?;
        if let Some(pid) = child.id() {
            let cancel_now = {
                let mut steps = self.active_steps.lock().await;
                if let Some(step) = steps.get_mut(&step_key) {
                    step.pid = Some(pid);
                    step.cancel_requested
                } else {
                    false
                }
            };
            if cancel_now {
                signal_step_process_group(pid, nix::sys::signal::Signal::SIGTERM as i32);
                let _ = child.kill().await;
                let _ = child.wait_with_output().await;
                return Ok(Response::new(cancelled_step_response()));
            }
        }

        let output = match child.wait_with_output().await {
            Ok(output) => output,
            Err(e) => return Err(Status::internal(format!("command failed: {}", e))),
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

        Ok(Response::new(RunCommandResponse {
            exit_code: spur_core::process::shell_exit_code(&output.status),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        }))
    }

    async fn stream_job_output(
        &self,
        request: Request<StreamJobOutputRequest>,
    ) -> Result<Response<Self::StreamJobOutputStream>, Status> {
        let req = request.into_inner();
        let job_id = req.job_id;

        self.check_job_access(job_id, &req.user, "read output of")
            .await?;

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

        self.check_job_access(init.job_id, &init.user, "attach to")
            .await?;

        let entry = self.job_entry(init.job_id).await?;

        let winsize = init.winsize.as_ref().map(|ws| PtyWinSize {
            rows: ws.rows as u16,
            cols: ws.cols as u16,
            xpixel: ws.xpixel as u16,
            ypixel: ws.ypixel as u16,
        });

        let argv: Vec<String> = init.argv.clone();

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

    async fn get_kubeconfig(
        &self,
        request: Request<GetKubeconfigRequest>,
    ) -> Result<Response<GetKubeconfigResponse>, Status> {
        let req = request.into_inner();
        // Empty user -> cluster-admin kubeconfig; set -> a scoped kubeconfig (SA + bound token in the
        // user's account namespace).
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
        let iface = std::env::var("SPUR_WG_INTERFACE").unwrap_or_else(|_| "spur0".into());
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

    /// Record controller-allocated GPUs and allocate local CPU/memory resources.
    async fn allocate_local_resources(
        &self,
        job_id: u32,
        spec: &JobSpec,
        allocated: Option<&ResourceAllocations>,
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

        let cpus = if spec.cpus_per_task > 0 {
            spec.cpus_per_task
        } else {
            0
        };
        let result = match alloc.allocate_for_job(
            job_id,
            cpus,
            spec.memory_per_node_mb,
            &controller_gpu_ids,
        ) {
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
                match alloc.allocate_for_job(
                    job_id,
                    cpus,
                    spec.memory_per_node_mb,
                    &controller_gpu_ids,
                ) {
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

    /// SIGTERM now, escalate to SIGKILL after a 5-second grace period.
    async fn graceful_cancel(&self, job_id: u32) {
        let is_allocation_only = {
            let jobs = self.running.lock().await;
            jobs.get(&job_id)
                .is_some_and(|tracked| tracked.job.is_allocation_only())
        };
        if is_allocation_only {
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

    /// Gate `user` for `action` on job `job_id`, per
    /// [`spur_core::auth::check_job_owner`].
    ///
    /// Enforced here as well as on the controller because `sattach` and the
    /// output stream dial the agent's port directly.
    async fn check_job_access(&self, job_id: u32, user: &str, action: &str) -> Result<(), Status> {
        let jobs = self.running.lock().await;
        let tracked = jobs
            .get(&job_id)
            .ok_or_else(|| Status::not_found(format!("job {} not running on this node", job_id)))?;

        spur_core::auth::check_job_owner(user, &tracked.user, action)
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

        let use_nsenter = entry.has_namespaces() && entry.pid > 0;
        let (launch_cmd, launch_args, apply_priv_in_child) = if use_nsenter {
            let mut args = entry.nsenter_args();
            if let Some(ref pd) = priv_drop {
                args.extend(pd.nsenter_args());
            }
            args.push("--".into());
            args.extend(shell);
            ("nsenter".to_string(), args, false)
        } else {
            (shell[0].clone(), shell[1..].to_vec(), true)
        };

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
        let job_id = 100;
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

        svc.check_job_access(46, "testuser", "attach to")
            .await
            .expect("the owner must be allowed to attach");
        svc.check_job_access(46, "root", "attach to")
            .await
            .expect("root is an admin override");

        let err = svc
            .check_job_access(46, "intruder", "attach to")
            .await
            .expect_err("a non-owner must not attach to another user's job");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);

        let missing = svc
            .check_job_access(999, "testuser", "attach to")
            .await
            .expect_err("an untracked job must report not-found");
        assert_eq!(missing.code(), tonic::Code::NotFound);
    }

    /// An empty-owner job runs as root, so non-root callers must be denied.
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

        let err = svc
            .check_job_access(47, "alice", "attach to")
            .await
            .expect_err("empty-owner jobs run as root; non-root must be denied");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);

        svc.check_job_access(47, "root", "attach to")
            .await
            .expect("root must always be allowed");

        svc.check_job_access(47, "", "attach to")
            .await
            .expect("daemon (empty user) must always be allowed");
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
    async fn signal_step_process_group_terminates_child_processes() {
        use std::time::Duration;

        let mut child = tokio::process::Command::new("bash")
            .arg("-c")
            .arg("sleep 3600 & sleep 3600 & wait")
            .process_group(0)
            .spawn()
            .expect("failed to spawn process group");
        let pid = child.id().expect("spawned child should have pid");
        tokio::time::sleep(Duration::from_millis(50)).await;

        signal_step_process_group(pid, nix::sys::signal::Signal::SIGTERM as i32);

        let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("process group did not exit after signal")
            .expect("wait failed");
        assert!(!status.success());
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
            .allocate_local_resources(100, &spec, Some(&allocated))
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
            .allocate_local_resources(100, &spec, Some(&allocated))
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
            .allocate_local_resources(100, &spec, Some(&allocated))
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
            .allocate_local_resources(100, &spec, Some(&gpu_alloc_request(&[0, 1])))
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
            .allocate_local_resources(100, &spec, Some(&gpu_alloc_request(&[0, 1])))
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

    // Interactive (srun/salloc) allocations must carry the controller's real
    // run_attempt, not a hardcoded 0 that disables the stale-report guard.
    #[tokio::test]
    async fn register_job_allocation_stores_requested_run_attempt() {
        let svc = AgentService::new(
            test_reporter_with_gpus(&[0]),
            HooksConfig::default(),
            Arc::new(Mutex::new(DeviceRegistry::new())),
            spur_core::config::MemlockLimit::Unlimited,
        );

        svc.register_job_allocation(Request::new(RegisterJobAllocationRequest {
            job_id: 91,
            cpus: 1,
            run_attempt: 3,
            ..Default::default()
        }))
        .await
        .expect("register");

        let running = svc.running.lock().await;
        let tracked = running.get(&91).expect("job tracked");
        assert_eq!(tracked.run_attempt, 3);
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
            spur_core::config::MemlockLimit::Unlimited,
            MpiConfig::default(),
            running,
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
