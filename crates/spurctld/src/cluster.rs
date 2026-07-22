// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use tokio::sync::Notify;
use tracing::{debug, info, warn};

use spur_core::account_limits::{check_account_limits, AccountCheckResult};
use spur_core::accounting::{Qos, TresRecord, TresType};
use spur_core::auth::AuthError;
use spur_core::burst_buffer::BbStageState;
use spur_core::config::SlurmConfig;
use spur_core::job::{
    effective_gpus, effective_memory_mb, Job, JobId, JobSpec, JobState, NodeCompleteError,
    PendingReason, TransitionOutcome,
};
use spur_core::node::{Node, NodeEvent, NodeSource, NodeState};
use spur_core::partition::{requested_partition_names, Partition, PreemptMode};
use spur_core::qos::{check_qos_limits, qos_adjusted_priority, QosCheckResult};
use spur_core::reservation::{self, normalize_node_list, running_jobs_overlap_start, Reservation};
use spur_core::resource::{ResourceAllocations, ResourceSet};
use spur_core::step::{JobStep, StepState, STEP_BATCH, STEP_RESERVED_MIN};
use spur_core::wal::WalOperation;
use spur_metrics::job::JobMetricsSnapshot;
use spur_metrics::node::NodeMetricsSnapshot;
use spur_metrics::partition::PartitionMetricsSnapshot;
use spur_metrics::user_acct::UserAcctMetricsSnapshot;

use crate::accounting::{AccountingNotifier, JobStartRecord};
use crate::association_cache::{AccountMembership, AssociationCache};
use crate::fairshare_cache::FairshareCache;
use crate::limits_cache::QosCache;
use crate::raft::{ClientResponse, JobFinalized, SpurRaft, StateMachineApply};
use crate::sched_stats::SchedStatsCollector;

/// Result of recording a per-node completion report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeCompleteResult {
    /// Node recorded; waiting for remaining nodes.
    Completing,
    /// All allocated nodes have reported; job is now terminal.
    AllDone { state: JobState, exit_code: i32 },
    /// Job was already in a terminal state (duplicate or race with cancel/timeout).
    AlreadyTerminal,
    /// Report came from a superseded run (older `run_attempt`); ignored so it
    /// cannot fail a job that has since been requeued and re-dispatched.
    StaleReport,
}

/// Reservation CRUD errors for the gRPC boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReservationError {
    InvalidArgument(String),
    NotFound(String),
    AlreadyExists(String),
    PermissionDenied(String),
    Raft(String),
}

impl std::fmt::Display for ReservationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidArgument(m)
            | Self::NotFound(m)
            | Self::AlreadyExists(m)
            | Self::PermissionDenied(m)
            | Self::Raft(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for ReservationError {}

/// Job submission errors for the gRPC / REST boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitError {
    InvalidArgument(String),
    Internal(String),
}

/// Errors from completing a standalone srun allocation.
#[derive(Debug)]
pub enum SrunCompleteError {
    NotFound(JobId),
    NotSrunJob(JobId),
    NotStepDispatch(JobId),
    AlreadyTerminal { job_id: JobId, state: JobState },
    NotOwner { job_id: JobId, user: String },
    Internal { job_id: JobId, message: String },
}

impl std::fmt::Display for SrunCompleteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(id) => write!(f, "job {id} not found"),
            Self::NotSrunJob(id) => write!(f, "job {id} is not an srun allocation"),
            Self::NotStepDispatch(id) => {
                write!(
                    f,
                    "job {id} does not use native step dispatch (CompleteJob is not valid)"
                )
            }
            Self::AlreadyTerminal { job_id, state } => {
                write!(f, "job {job_id} is already {state:?}")
            }
            Self::NotOwner { job_id, user } => {
                write!(f, "user {user} is not permitted to complete job {job_id}")
            }
            Self::Internal { job_id, message } => {
                write!(f, "job {job_id}: {message}")
            }
        }
    }
}

impl std::error::Error for SrunCompleteError {}

impl std::fmt::Display for SubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidArgument(m) | Self::Internal(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for SubmitError {}

impl SubmitError {
    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::InvalidArgument(msg.into())
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }
}

/// Maximum serialized size of a single job submission, in bytes.
///
/// A submission becomes one Raft log entry (`WalOperation::JobSubmit`) that is
/// also retained in every snapshot, so bounding the whole serialized spec keeps
/// that entry well under `crate::raft::RAFT_MAX_MESSAGE_SIZE`. Measuring the
/// serialized form rather than individual fields counts every payload the spec
/// carries (script, environment, argv, container env/mounts, ...), so it cannot
/// be bypassed by a field the check forgot to sum. Mirrors Slurm, which rejects
/// oversized batch scripts.
const MAX_JOB_SPEC_SIZE: usize = 4 * 1024 * 1024;

/// A `std::io::Write` that only tallies byte counts and discards the data. Used
/// to measure a value's serialized size without allocating the serialized bytes.
#[derive(Default)]
struct ByteCounter {
    len: usize,
}

impl std::io::Write for ByteCounter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.len += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn check_submission_size(spec: &JobSpec) -> Result<(), SubmitError> {
    // Count serialized bytes without allocating the output; the entry is
    // re-serialized by openraft on propose.
    let mut counter = ByteCounter::default();
    serde_json::to_writer(&mut counter, spec)
        .map_err(|e| SubmitError::internal(format!("failed to encode job spec: {e}")))?;
    let size = counter.len;
    if size > MAX_JOB_SPEC_SIZE {
        return Err(SubmitError::invalid(format!(
            "job submission too large: {:.1} MiB serialized, limit is {} MiB (reduce script, environment, or argv size)",
            size as f64 / (1024.0 * 1024.0),
            MAX_JOB_SPEC_SIZE / (1024 * 1024),
        )));
    }
    Ok(())
}

impl ReservationError {
    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::InvalidArgument(msg.into())
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::NotFound(msg.into())
    }

    pub fn already_exists(msg: impl Into<String>) -> Self {
        Self::AlreadyExists(msg.into())
    }

    pub fn permission_denied(msg: impl Into<String>) -> Self {
        Self::PermissionDenied(msg.into())
    }

    pub fn raft(msg: impl Into<String>) -> Self {
        Self::Raft(msg.into())
    }
}

/// What the caller must dispatch to node agents after `preempt_job`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreemptOutcome {
    /// Kill the job's processes on every allocated node (cancel or requeue mode).
    Killed,
    /// Stop (SIGSTOP) the job's processes; allocation is retained (suspend mode).
    Suspended,
}

/// Central cluster state manager.
///
/// Thread-safe via RwLock. The scheduler and gRPC server both access this.
/// State recovery happens through Raft log replay (via `StateMachineApply`).
pub struct ClusterManager {
    pub config: SlurmConfig,
    jobs: RwLock<HashMap<JobId, Job>>,
    nodes: RwLock<HashMap<String, Node>>,
    partitions: RwLock<Vec<Partition>>,
    next_job_id: AtomicU32,
    reservations: RwLock<Vec<Reservation>>,
    steps: RwLock<HashMap<(JobId, u32), JobStep>>,
    /// Configured cluster-wide license totals (immutable; from config). Current
    /// availability is derived as total minus the licenses held by active jobs
    /// (see `available_licenses`), so it cannot drift or diverge from config.
    license_pool: RwLock<HashMap<String, u64>>,
    /// Configured cluster-wide burst-buffer capacity in GB (immutable; from
    /// config). Like licenses, current availability is derived as total minus
    /// the capacity reserved by jobs that have entered staging (see
    /// `available_bb_with`), so it cannot drift from config.
    burst_buffer_total_gb: RwLock<u64>,
    tokens: RwLock<HashMap<String, spur_core::admission::AdmissionToken>>,
    /// Native k0s cluster state (phase, control-plane node, join-token metadata).
    k0s: RwLock<spur_core::k0s::K0sClusterState>,
    raft: RwLock<Option<SpurRaft>>,
    accounting: RwLock<Option<AccountingNotifier>>,
    fairshare_cache: Arc<FairshareCache>,
    qos_cache: Arc<QosCache>,
    association_cache: Arc<AssociationCache>,
    /// Wake signal for the scheduler loop.
    pub(crate) scheduler_notify: Arc<Notify>,
    sched_stats: OnceLock<Arc<SchedStatsCollector>>,
}

impl ClusterManager {
    pub fn new(config: SlurmConfig, _state_dir: &Path) -> anyhow::Result<Self> {
        let partitions = config.build_partitions();
        let license_pool = config.licenses.clone();
        let burst_buffer_total_gb = config.burst_buffer.total_gb;
        let fairshare_cache = Arc::new(FairshareCache::new());
        let first_job_id = config.controller.first_job_id;
        let qos_cache = Arc::new(QosCache::new());
        let association_cache = Arc::new(AssociationCache::new());

        let cm = Self {
            config,
            jobs: RwLock::new(HashMap::new()),
            nodes: RwLock::new(HashMap::new()),
            partitions: RwLock::new(partitions),
            reservations: RwLock::new(Vec::new()),
            steps: RwLock::new(HashMap::new()),
            next_job_id: AtomicU32::new(first_job_id),
            license_pool: RwLock::new(license_pool),
            burst_buffer_total_gb: RwLock::new(burst_buffer_total_gb),
            tokens: RwLock::new(HashMap::new()),
            k0s: RwLock::new(spur_core::k0s::K0sClusterState::default()),
            raft: RwLock::new(None),
            accounting: RwLock::new(None),
            fairshare_cache,
            qos_cache,
            association_cache,
            scheduler_notify: Arc::new(Notify::new()),
            sched_stats: OnceLock::new(),
        };

        info!("cluster manager initialized (state will be recovered via Raft)");

        Ok(cm)
    }

    /// Submit a new job. If it has an array spec, expand into individual tasks.
    pub fn submit_job(&self, mut spec: JobSpec) -> Result<JobId, SubmitError> {
        apply_default_partition(&mut spec, &self.partitions.read());
        apply_default_time_limit(&mut spec, &self.partitions.read());
        apply_default_account(&mut spec, &self.association_cache);
        validate_user_account(&spec, &self.association_cache)?;
        self.validate_partition(&spec)?;
        apply_default_qos(
            &mut spec,
            &self.association_cache,
            &self.qos_cache,
            &self.config.accounting,
        )?;

        // Checked after defaults are applied so we measure the final spec.
        // Array expansion only adds bounded integer metadata per task, so a
        // single pre-expansion check still bounds each Raft log entry.
        check_submission_size(&spec)?;

        // Reject unknown/malformed dependency types up front so users get a
        // clear error instead of a silently-deadlocked job (e.g. `expand:N`).
        // This validates syntax only — the dependency *target* is intentionally
        // not checked for existence here (matching Slurm), so e.g. `after:9999`
        // against a nonexistent job is accepted and resolves as satisfiable.
        if !spec.dependency.is_empty() {
            spur_core::dependency::try_parse_dependencies(&spec.dependency)
                .map_err(|e| SubmitError::invalid(format!("invalid dependency: {e}")))?;
        }

        let job_id = self.next_job_id.fetch_add(1, Ordering::SeqCst);
        let specs =
            expand_job_specs(spec, job_id).map_err(|e| SubmitError::invalid(e.to_string()))?;

        for task_spec in specs {
            let task_id = if task_spec.array_job_id.is_some() {
                self.next_job_id.fetch_add(1, Ordering::SeqCst)
            } else {
                job_id
            };
            self.propose(WalOperation::JobSubmit {
                job_id: task_id,
                spec: Box::new(task_spec),
            })
            .map_err(|e| SubmitError::internal(e.to_string()))?;
            if let Some(stats) = self.sched_stats.get() {
                stats.record_submitted(1);
            }
        }

        self.scheduler_notify.notify_one();

        info!(job_id, "job submitted");
        Ok(job_id)
    }

    /// Validate requested partition names and partition account access control.
    fn validate_partition(&self, spec: &JobSpec) -> Result<(), SubmitError> {
        let partition_spec = match spec.partition.as_deref() {
            Some(p) if !p.is_empty() => p,
            _ => return Ok(()), // Unset or empty partition name — nothing to validate
        };

        let partitions = self.partitions.read();
        let requested = requested_partition_names(Some(partition_spec))
            .map(|name| {
                partitions
                    .iter()
                    .find(|part| part.name == name)
                    .ok_or_else(|| SubmitError::invalid(format!("partition '{name}' not found")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if requested.is_empty() {
            return Err(SubmitError::invalid(format!(
                "partition '{partition_spec}' not found"
            )));
        }

        let Some(first_acl_partition) = requested
            .iter()
            .copied()
            .find(|part| !part.allow_accounts.is_empty() || !part.deny_accounts.is_empty())
        else {
            return Ok(());
        };

        if !self.association_cache.is_loaded() {
            warn!(
                user = %spec.user,
                partition = %partition_spec,
                "association cache unavailable; skipping partition account access checks"
            );
            return Ok(());
        }

        let account = match spec.account.as_deref().filter(|a| !a.is_empty()) {
            Some(a) => a,
            None => {
                return Err(SubmitError::invalid(format!(
                    "no account for user '{}' on partition '{}'",
                    spec.user, first_acl_partition.name
                )));
            }
        };

        for part in requested {
            if !part.allow_accounts.is_empty() {
                if part.allow_accounts.iter().any(|a| a == account) {
                    continue;
                }
                return Err(SubmitError::invalid(format!(
                    "account '{account}' not allowed on partition '{}'",
                    part.name
                )));
            }
            if part.deny_accounts.iter().any(|a| a == account) {
                return Err(SubmitError::invalid(format!(
                    "account '{account}' denied on partition '{}'",
                    part.name
                )));
            }
        }

        Ok(())
    }

    /// Get a job by ID.
    pub fn get_job(&self, job_id: JobId) -> Option<Job> {
        self.jobs.read().get(&job_id).cloned()
    }

    /// Get a job by ID, synthesizing an aggregate record for an array *parent*
    /// id (which has no stored job — Spur stores only per-task jobs) so
    /// `scontrol show job <array_parent>` matches Slurm instead of returning
    /// empty. The synthesized job borrows the first task's spec, reports the
    /// aggregate state, earliest start / latest end; it is never stored.
    pub fn get_job_for_display(&self, job_id: JobId) -> Option<Job> {
        let jobs = self.jobs.read();
        if let Some(j) = jobs.get(&job_id) {
            return Some(j.clone());
        }
        // Maybe it's an array parent id.
        let mut tasks: Vec<&Job> = jobs
            .values()
            .filter(|j| j.spec.array_job_id == Some(job_id))
            .collect();
        if tasks.is_empty() {
            return None;
        }
        tasks.sort_by_key(|t| t.spec.array_task_id);

        let first = tasks[0];
        let mut synth = (*first).clone();
        synth.job_id = job_id;
        // Present as the parent: drop per-task id, keep array linkage.
        synth.spec.array_task_id = None;
        synth.spec.array_job_id = Some(job_id);

        let states: Vec<JobState> = tasks.iter().map(|t| t.state).collect();
        synth.state = spur_core::array::aggregate_array_state(&states).unwrap_or(JobState::Pending);
        synth.start_time = tasks.iter().filter_map(|t| t.start_time).min();
        synth.end_time = if synth.state.is_terminal() {
            tasks.iter().filter_map(|t| t.end_time).max()
        } else {
            None
        };
        // Worst non-zero exit across tasks; None while non-terminal so a
        // pending aggregate doesn't read as "0 / success".
        synth.exit_code = if synth.state.is_terminal() {
            tasks
                .iter()
                .filter_map(|t| t.exit_code)
                .filter(|c| *c != 0)
                .max()
                .or(Some(0))
        } else {
            None
        };
        Some(synth)
    }

    /// Aggregated job metrics from the current in-memory job map (lazy scan).
    ///
    /// The `jobs` map is authoritative (WAL-backed); this scans it on each call.
    pub fn job_metrics(&self) -> JobMetricsSnapshot {
        let jobs = self.jobs.read();
        JobMetricsSnapshot::collect(jobs.values())
    }

    /// Aggregated node metrics from the current in-memory node map (lazy scan).
    ///
    /// The `nodes` map is authoritative (WAL-backed for node catalog fields);
    /// this scans it on each call.
    pub fn node_metrics(&self) -> NodeMetricsSnapshot {
        let nodes = self.nodes.read();
        NodeMetricsSnapshot::collect(nodes.values())
    }

    /// Aggregated per-partition metrics from the current job, node, and partition maps.
    pub fn partition_metrics(&self) -> PartitionMetricsSnapshot {
        let names: Vec<String> = self
            .partitions
            .read()
            .iter()
            .map(|p| p.name.clone())
            .collect();
        let jobs = self.jobs.read();
        let nodes = self.nodes.read();
        PartitionMetricsSnapshot::collect(
            names.iter().map(|s| s.as_str()),
            jobs.values(),
            nodes.values(),
        )
    }

    /// Aggregated per-user and per-account job metrics from the current job map.
    pub fn user_acct_metrics(&self) -> UserAcctMetricsSnapshot {
        let jobs = self.jobs.read();
        UserAcctMetricsSnapshot::collect(jobs.values())
    }

    /// Get jobs matching filters.
    pub fn get_jobs(
        &self,
        states: &[JobState],
        user: Option<&str>,
        partition: Option<&str>,
        account: Option<&str>,
        name: Option<&str>,
        job_ids: &[JobId],
    ) -> Vec<Job> {
        let matches = |j: &Job| -> bool {
            if !states.is_empty() && !states.contains(&j.state) {
                return false;
            }
            if let Some(u) = user {
                if !u.is_empty() && j.spec.user != u {
                    return false;
                }
            }
            if let Some(p) = partition {
                if !p.is_empty() && j.spec.partition.as_deref() != Some(p) {
                    return false;
                }
            }
            if let Some(a) = account {
                if !a.is_empty() && j.spec.account.as_deref() != Some(a) {
                    return false;
                }
            }
            if let Some(n) = name {
                if !n.is_empty() && !n.split(',').any(|pat| pat.trim() == j.spec.name) {
                    return false;
                }
            }
            true
        };

        let mut result: Vec<Job> = {
            let jobs = self.jobs.read();
            jobs.values()
                .filter(|j| {
                    if !job_ids.is_empty() && !job_ids.contains(&j.job_id) {
                        return false;
                    }
                    matches(j)
                })
                .cloned()
                .collect()
        };

        // Requested ids with no stored job may be array parents — synthesize
        // their aggregate. Read lock above is released before get_job_for_display.
        if !job_ids.is_empty() {
            for &id in job_ids {
                if result.iter().any(|j| j.job_id == id) {
                    continue;
                }
                if let Some(parent) = self.get_job_for_display(id) {
                    if matches(&parent) {
                        result.push(parent);
                    }
                }
            }
        }

        result
    }

    /// Mark a pending job as DEADLINE (Slurm parity for `--deadline`).
    ///
    /// Only valid from `Pending`: returns `Err` if the job is unknown, already
    /// terminal, or has started running. Callers treat the error as non-fatal.
    pub fn deadline_job(&self, job_id: JobId) -> anyhow::Result<()> {
        {
            let mut jobs = self.jobs.write();
            let job = jobs
                .get_mut(&job_id)
                .ok_or_else(|| anyhow::anyhow!("job {} not found", job_id))?;
            if job.state.is_terminal() {
                anyhow::bail!("job {} is already {:?}", job_id, job.state);
            }
            if job.state != JobState::Pending {
                anyhow::bail!(
                    "job {} not eligible for DEADLINE from state {:?}",
                    job_id,
                    job.state
                );
            }
            // Record the reason before the terminal transition so any
            // observer (history, audit log, late `squeue` poll) sees DeadLine
            // instead of whatever update_pending_reasons last wrote.
            job.pending_reason = PendingReason::DeadLine;
        }

        let resp = self.propose(WalOperation::JobComplete {
            job_id,
            exit_code: -1,
            state: JobState::Deadline,
        })?;
        self.run_all_finalized_side_effects(&resp);

        info!(job_id, "job deadline passed — transitioned to DEADLINE");
        Ok(())
    }

    /// Check that `user` is allowed to perform `action` on a job owned by `owner`.
    /// Empty user (internal/daemon calls) and root are always allowed.
    fn check_job_owner(user: &str, owner: &str, action: &str) -> anyhow::Result<()> {
        if user.is_empty() || user == "root" || user == owner {
            return Ok(());
        }
        Err(AuthError::NotJobOwner {
            user: user.into(),
            owner: owner.into(),
            action: action.into(),
        }
        .into())
    }

    /// Cancel a job. The requesting `user` must be the job owner, root, or
    /// empty (trusted internal/daemon calls).
    pub fn cancel_job(&self, job_id: JobId, user: &str) -> anyhow::Result<()> {
        {
            let jobs = self.jobs.read();
            let job = jobs
                .get(&job_id)
                .ok_or_else(|| anyhow::anyhow!("job {} not found", job_id))?;
            if job.state.is_terminal() {
                anyhow::bail!("job {} is already {:?}", job_id, job.state);
            }
            Self::check_job_owner(user, &job.spec.user, "cancel")?;
        }

        // Use JobComplete (not JobStateChange) so that resource deallocation
        // fires for any allocated nodes. For pending jobs, allocated_nodes is empty
        // so the deallocation loop is a no-op.
        let resp = self.propose(WalOperation::JobComplete {
            job_id,
            exit_code: -1,
            state: JobState::Cancelled,
        })?;
        self.run_all_finalized_side_effects(&resp);

        info!(job_id, "job cancelled");
        Ok(())
    }

    /// Complete a standalone srun allocation after its step finishes.
    pub fn finish_srun_job(
        &self,
        job_id: JobId,
        exit_code: i32,
        user: &str,
    ) -> Result<Job, SrunCompleteError> {
        let job = {
            let jobs = self.jobs.read();
            let job = jobs
                .get(&job_id)
                .ok_or(SrunCompleteError::NotFound(job_id))?;
            if !job.spec.srun_job {
                return Err(SrunCompleteError::NotSrunJob(job_id));
            }
            if !job.srun_step_dispatch {
                return Err(SrunCompleteError::NotStepDispatch(job_id));
            }
            if job.state.is_terminal() {
                return Err(SrunCompleteError::AlreadyTerminal {
                    job_id,
                    state: job.state,
                });
            }
            Self::check_job_owner(user, &job.spec.user, "complete").map_err(|_| {
                SrunCompleteError::NotOwner {
                    job_id,
                    user: user.to_string(),
                }
            })?;
            job.clone()
        };

        let state = if exit_code == 0 {
            JobState::Completed
        } else {
            JobState::Failed
        };
        self.complete_job(job_id, exit_code, state).map_err(|e| {
            warn!(job_id, error = %e, "finish_srun_job: complete_job failed");
            SrunCompleteError::Internal {
                job_id,
                message: e.to_string(),
            }
        })?;
        Ok(job)
    }

    /// Suspend a running job: validate state, record through Raft. Allocation is retained.
    /// The requesting `user` must be the job owner, root, or empty (trusted internal calls).
    pub fn suspend_job(&self, job_id: JobId, user: &str) -> anyhow::Result<()> {
        {
            let jobs = self.jobs.read();
            let job = jobs
                .get(&job_id)
                .ok_or_else(|| anyhow::anyhow!("job {} not found", job_id))?;
            if job.state != JobState::Running {
                anyhow::bail!("job {} is not running (state {:?})", job_id, job.state);
            }
            Self::check_job_owner(user, &job.spec.user, "suspend")?;
        }
        self.propose(WalOperation::JobSuspend {
            job_id,
            at: chrono::Utc::now(),
        })?;
        info!(job_id, "job suspended");
        Ok(())
    }

    /// Resume a suspended job: validate state, record through Raft, fold suspended time.
    /// The requesting `user` must be the job owner, root, or empty (trusted internal calls).
    pub fn resume_job(&self, job_id: JobId, user: &str) -> anyhow::Result<()> {
        {
            let jobs = self.jobs.read();
            let job = jobs
                .get(&job_id)
                .ok_or_else(|| anyhow::anyhow!("job {} not found", job_id))?;
            if job.state != JobState::Suspended {
                anyhow::bail!("job {} is not suspended (state {:?})", job_id, job.state);
            }
            Self::check_job_owner(user, &job.spec.user, "resume")?;
        }
        self.propose(WalOperation::JobResume {
            job_id,
            at: chrono::Utc::now(),
        })?;
        info!(job_id, "job resumed");
        Ok(())
    }

    /// Start a job on specific nodes.
    /// Transition a pending job to Running and record its allocation. Returns
    /// the run epoch assigned to this dispatch (threaded into the launch RPC).
    pub fn start_job(
        &self,
        job_id: JobId,
        node_names: Vec<String>,
        resources: ResourceAllocations,
        per_node_alloc: std::collections::HashMap<String, ResourceAllocations>,
    ) -> anyhow::Result<u32> {
        self.start_job_impl(job_id, node_names, resources, per_node_alloc, false)
    }

    pub(crate) fn start_job_impl(
        &self,
        job_id: JobId,
        node_names: Vec<String>,
        resources: ResourceAllocations,
        per_node_alloc: std::collections::HashMap<String, ResourceAllocations>,
        srun_step_dispatch: bool,
    ) -> anyhow::Result<u32> {
        for name in &node_names {
            if !per_node_alloc.contains_key(name) {
                anyhow::bail!(
                    "job {}: per_node_alloc missing entry for node '{}'",
                    job_id,
                    name
                );
            }
        }

        // Validate job exists and can transition
        let old_state;
        let spec_for_notify;
        let submit_time_for_notify;
        let run_attempt;
        {
            let jobs = self.jobs.read();
            let job = jobs
                .get(&job_id)
                .ok_or_else(|| anyhow::anyhow!("job {} not found", job_id))?;
            old_state = job.state;
            spec_for_notify = job.spec.clone();
            submit_time_for_notify = job.submit_time;
            // Next run epoch (first dispatch = 1), threaded to the agents.
            run_attempt = job.run_attempt.saturating_add(1);
            if job.state != JobState::Pending {
                anyhow::bail!("job {} cannot start from state {:?}", job_id, job.state);
            }
        }

        // propose() handles: state transition, resource allocation, license subtraction
        self.propose(WalOperation::job_state_change(
            job_id,
            old_state,
            JobState::Running,
        ))?;
        self.propose(WalOperation::JobStart {
            job_id,
            nodes: node_names.clone(),
            resources: resources.clone(),
            per_node_alloc: per_node_alloc.clone(),
            srun_step_dispatch,
            run_attempt,
        })?;

        let node_count = node_names.len().max(1) as u32;
        let per_node = node_names
            .first()
            .and_then(|n| per_node_alloc.get(n).cloned())
            .unwrap_or_else(|| {
                ResourceAllocations::with_scalar(
                    resources.cpus / node_count,
                    resources.memory_mb / node_count as u64,
                )
            });
        if !srun_step_dispatch {
            let batch_step = JobStep {
                job_id,
                step_id: STEP_BATCH,
                name: "batch".into(),
                state: StepState::Running,
                num_tasks: 1,
                cpus_per_task: per_node.cpus,
                resources: per_node,
                nodes: node_names,
                distribution: spur_core::step::TaskDistribution::Block,
                start_time: Some(Utc::now()),
                end_time: None,
                exit_code: None,
            };
            if let Err(e) = self.create_step(batch_step) {
                warn!(job_id, error = %e, "failed to record batch step");
            }
        }

        if spec_for_notify
            .mail_type
            .iter()
            .any(|t| t == "BEGIN" || t == "ALL")
        {
            self.send_notification(job_id, "BEGIN", &spec_for_notify);
        }

        if let Some(ref notifier) = *self.accounting.read() {
            notifier.notify_job_start(JobStartRecord {
                job_id,
                name: spec_for_notify.name.clone(),
                user: spec_for_notify.user.clone(),
                account: spec_for_notify.account.clone().unwrap_or_default(),
                partition: spec_for_notify.partition.clone().unwrap_or_default(),
                num_nodes: spec_for_notify.num_nodes,
                num_tasks: spec_for_notify.num_tasks,
                cpus_per_task: spec_for_notify.cpus_per_task,
                memory_mb: resources.memory_mb,
                submit_time: submit_time_for_notify,
                start_time: Utc::now(),
                reservation: spec_for_notify.reservation.clone(),
            });
        }

        debug!(job_id, "job started");
        Ok(run_attempt)
    }

    /// Record completion from one allocated node (multi-node COMPLETING flow).
    pub fn node_complete(
        &self,
        job_id: JobId,
        node_name: &str,
        exit_code: i32,
        signal: i32,
        run_attempt: u32,
    ) -> Result<NodeCompleteResult, NodeCompleteError> {
        {
            let jobs = self.jobs.read();
            let job = jobs
                .get(&job_id)
                .ok_or(NodeCompleteError::JobNotFound { job_id })?;
            if job.state.is_terminal() {
                return Ok(NodeCompleteResult::AlreadyTerminal);
            }
            // Drop a report from a superseded run (older epoch); e.g. the
            // delayed SIGKILL of a preemption-requeued process. Epoch 0 on
            // either side (legacy) disables the check.
            if run_attempt != 0 && job.run_attempt != 0 && run_attempt < job.run_attempt {
                return Ok(NodeCompleteResult::StaleReport);
            }
            if !job.allocated_nodes.iter().any(|n| n == node_name) {
                return Err(NodeCompleteError::NodeNotAllocated {
                    job_id,
                    node: node_name.to_string(),
                });
            }
        }

        let resp = self
            .propose(WalOperation::JobNodeComplete {
                job_id,
                node_name: node_name.to_string(),
                exit_code,
                signal,
            })
            .map_err(|source| NodeCompleteError::RaftPropose { source })?;

        self.run_all_finalized_side_effects(&resp);
        if let Some(f) = resp.jobs_finalized.first() {
            return Ok(NodeCompleteResult::AllDone {
                state: f.state,
                exit_code: f.exit_code,
            });
        }

        let jobs = self.jobs.read();
        if jobs.get(&job_id).is_some_and(|job| job.state.is_terminal()) {
            return Ok(NodeCompleteResult::AlreadyTerminal);
        }

        Ok(NodeCompleteResult::Completing)
    }

    /// Complete a job (controller-initiated or force-finish from COMPLETING timeout).
    pub fn complete_job(
        &self,
        job_id: JobId,
        exit_code: i32,
        state: JobState,
    ) -> anyhow::Result<()> {
        // Validate
        {
            let jobs = self.jobs.read();
            let job = jobs
                .get(&job_id)
                .ok_or_else(|| anyhow::anyhow!("job {} not found", job_id))?;
            if job.state.is_terminal() {
                anyhow::bail!("invalid transition from {:?} to {:?}", job.state, state);
            }
        }

        // propose() handles: state transition, exit_code, end_time,
        // resource deallocation, step completion, license return
        let resp = self.propose(WalOperation::JobComplete {
            job_id,
            exit_code,
            state,
        })?;
        self.run_all_finalized_side_effects(&resp);

        debug!(job_id, exit_code, "job completed");
        Ok(())
    }

    /// Preempt a running job per its partition's PreemptMode. Does the
    /// controller-side state change; the caller dispatches the signal named by
    /// the returned `PreemptOutcome`. `Off` is rejected.
    pub fn preempt_job(&self, job_id: JobId, mode: PreemptMode) -> anyhow::Result<PreemptOutcome> {
        {
            let jobs = self.jobs.read();
            let job = jobs
                .get(&job_id)
                .ok_or_else(|| anyhow::anyhow!("job {} not found", job_id))?;
            if job.state != JobState::Running {
                anyhow::bail!("job {} is not running (state {:?})", job_id, job.state);
            }
        }

        match mode {
            PreemptMode::Off => anyhow::bail!("preemption disabled for job {}", job_id),
            PreemptMode::Suspend => {
                self.suspend_job(job_id, "")?;
                info!(job_id, "job preempted (suspend)");
                Ok(PreemptOutcome::Suspended)
            }
            PreemptMode::Cancel => {
                self.complete_job(job_id, -1, JobState::Cancelled)?;
                info!(job_id, "job preempted (cancel)");
                Ok(PreemptOutcome::Killed)
            }
            PreemptMode::Requeue => {
                // Single atomic op: free nodes, end the run for accounting, and
                // return to Pending with an eligibility hold. A two-proposal
                // sequence could strand the job in PREEMPTED if the second
                // proposal failed after the first committed (leadership change /
                // restart), which nothing scans for or recovers.
                //
                // Requeue-by-preemption intentionally ignores spec.requeue and
                // the maybe_requeue MAX_REQUEUE cap: Slurm always requeues a
                // preempted job regardless of its --requeue flag. This is a
                // deliberate divergence from the ordinary requeue path.
                let hold_secs = (self.config.scheduler.interval_secs as i64 * 2 + 3).max(5);
                let hold = Utc::now() + chrono::Duration::seconds(hold_secs);
                // Honor a later user --begin: compute the max on the leader so
                // followers apply one verbatim instant (no per-replica clock).
                let begin_time = self
                    .jobs
                    .read()
                    .get(&job_id)
                    .and_then(|j| j.spec.begin_time)
                    .map_or(hold, |user_begin| user_begin.max(hold));
                let resp = self.propose(WalOperation::JobPreemptRequeue { job_id, begin_time })?;
                self.run_all_finalized_side_effects(&resp);
                info!(job_id, hold_secs, "job preempted (requeue)");
                Ok(PreemptOutcome::Killed)
            }
        }
    }

    fn run_job_finalized_side_effects(&self, finalized: JobFinalized) {
        if let Some(stats) = self.sched_stats.get() {
            stats.record_finalized();
        }
        self.run_epilog_slurmctld(finalized.job_id);
        self.notify_job_finished(finalized.job_id, finalized.state, finalized.exit_code);
    }

    fn run_all_finalized_side_effects(&self, resp: &ClientResponse) {
        for f in &resp.jobs_finalized {
            self.run_job_finalized_side_effects(*f);
        }
    }

    fn run_epilog_slurmctld(&self, job_id: JobId) {
        let Some(epilog_ctld) = self.config.hooks.epilog_slurmctld.clone() else {
            return;
        };
        let job = self.get_job(job_id);
        let ctx = spur_core::hooks::HookContext {
            job_id,
            work_dir: job
                .as_ref()
                .map(|j| j.spec.work_dir.clone())
                .unwrap_or_else(|| "/tmp".into()),
            uid: job.as_ref().map(|j| j.spec.uid).unwrap_or(0),
            gid: job.as_ref().map(|j| j.spec.gid).unwrap_or(0),
            partition: job
                .as_ref()
                .and_then(|j| j.spec.partition.clone())
                .unwrap_or_default(),
            nodelist: job
                .as_ref()
                .map(|j| j.allocated_nodes.join(","))
                .unwrap_or_default(),
            script_context: "epilog_slurmctld".into(),
            gpu_devices: Vec::new(),
            cpus: job.as_ref().map(|j| j.spec.cpus_per_task).unwrap_or(1),
            memory_mb: job
                .as_ref()
                .and_then(|j| j.spec.memory_per_node_mb)
                .unwrap_or(0),
        };
        tokio::spawn(async move {
            if let Err(e) = spur_core::hooks::run_hook(&epilog_ctld, &ctx).await {
                warn!(job_id, error = %e, "EpilogSlurmctld failed");
            }
        });
    }

    fn notify_job_finished(&self, job_id: JobId, state: JobState, exit_code: i32) {
        let spec_for_notify = self.jobs.read().get(&job_id).map(|j| j.spec.clone());
        if let Some(spec) = spec_for_notify {
            let is_success = state == JobState::Completed;
            let is_failure = matches!(
                state,
                JobState::Failed | JobState::Timeout | JobState::NodeFail | JobState::Deadline
            );
            if is_success && spec.mail_type.iter().any(|t| t == "END" || t == "ALL") {
                self.send_notification(job_id, "END", &spec);
            }
            if is_failure && spec.mail_type.iter().any(|t| t == "FAIL" || t == "ALL") {
                self.send_notification(job_id, "FAIL", &spec);
            }
        }

        if let Some(ref notifier) = *self.accounting.read() {
            let (exit_signal, derived_exit_code) = self
                .jobs
                .read()
                .get(&job_id)
                .map(|j| (j.exit_signal, j.derived_exit_code))
                .unwrap_or((0, 0));
            notifier.notify_job_end(
                job_id,
                state,
                exit_code,
                Utc::now(),
                exit_signal,
                derived_exit_code,
            );
        }

        // Preempted excluded: preempt_job owns its requeue (with hold).
        let should_requeue = matches!(state, JobState::Timeout | JobState::NodeFail);
        if should_requeue {
            if let Err(e) = self.maybe_requeue(job_id) {
                warn!(job_id, error = %e, "failed to requeue job");
            }
        }
    }

    /// Requeue a job if spec.requeue is set and attempt limit not exceeded.
    fn maybe_requeue(&self, job_id: JobId) -> anyhow::Result<()> {
        let max = self.config.controller.max_batch_requeue;
        let (should_requeue, old_state) = {
            let jobs = self.jobs.read();
            let Some(job) = jobs.get(&job_id) else {
                return Ok(());
            };
            if job.requeue_count >= max {
                if matches!(
                    job.state,
                    JobState::Preempted | JobState::Timeout | JobState::NodeFail
                ) {
                    drop(jobs);
                    return self.hold_job_at_max_requeue(job_id);
                }
                return Ok(());
            }
            if !job.spec.requeue {
                return Ok(());
            }
            (true, job.state)
        };
        if !should_requeue {
            return Ok(());
        }

        self.propose(WalOperation::job_state_change(
            job_id,
            old_state,
            JobState::Pending,
        ))?;

        info!(job_id, from = %old_state, "job requeued");
        Ok(())
    }

    /// Requeue a job back to Pending after a dispatch failure.
    /// Unlike `maybe_requeue`, this is unconditional and doesn't require
    /// the requeue flag on the spec. Used when the agent rejects a job
    /// (e.g., container image not found) so it can be retried after the
    /// user fixes the issue. (Issue #91)
    pub fn requeue_job(&self, job_id: JobId) -> anyhow::Result<()> {
        let old_state = {
            let jobs = self.jobs.read();
            let Some(job) = jobs.get(&job_id) else {
                return Ok(());
            };
            if job.state.is_terminal() {
                return Ok(());
            }
            if job.requeue_count >= self.config.controller.max_batch_requeue {
                drop(jobs);
                return self.hold_job_at_max_requeue(job_id);
            }
            job.state
        };

        // transition to Failed via JobComplete so node resources,
        // licenses, and steps are properly cleaned up.
        self.propose(WalOperation::JobComplete {
            job_id,
            exit_code: -1,
            state: JobState::Failed,
        })?;

        // Failed → Pending resets allocation fields and makes
        // the job schedulable again.
        self.propose(WalOperation::job_state_change(
            job_id,
            JobState::Failed,
            JobState::Pending,
        ))?;

        info!(job_id, from = %old_state, "job requeued after dispatch failure");
        Ok(())
    }

    /// Evict a running job to NodeFail: frees allocations and feeds the
    /// existing auto-requeue path in `notify_job_finished`, same as the
    /// health-check path (`evict_jobs_on_node`) that runs when an entire
    /// node goes Down, but scoped to a single job. Used when only some of a
    /// job's assigned nodes could be dispatched to, since a node that never
    /// launched the job will never report completion. Unlike the
    /// health-check path, this does not by itself cancel the job on nodes
    /// that *did* launch it — the caller (`scheduler_loop`) is responsible
    /// for sending the cancel RPC once eviction succeeds.
    pub fn evict_job(&self, job_id: JobId) -> anyhow::Result<()> {
        {
            let jobs = self.jobs.read();
            let job = jobs
                .get(&job_id)
                .ok_or_else(|| anyhow::anyhow!("job {} not found", job_id))?;
            if job.state.is_terminal() {
                return Ok(());
            }
        }
        let resp = self.propose(WalOperation::JobEvict { job_id })?;
        self.run_all_finalized_side_effects(&resp);
        Ok(())
    }

    /// Register a node agent.
    #[allow(clippy::too_many_arguments)]
    pub fn register_node(
        &self,
        name: String,
        resources: ResourceSet,
        address: String,
        port: u16,
        wg_pubkey: String,
        version: String,
        source: NodeSource,
        labels: HashMap<String, String>,
    ) -> anyhow::Result<()> {
        let action = {
            let nodes = self.nodes.read();
            evaluate_registration(nodes.get(&name), &resources)
        };

        match action {
            RegistrationAction::Skip => {
                debug!(node = %name, "node unchanged, skipping");
                self.sync_node_labels(&name, labels)?;
            }
            RegistrationAction::Update => {
                self.propose(WalOperation::NodeUpdate {
                    name: name.clone(),
                    resources,
                    address,
                    port,
                    wg_pubkey,
                    version,
                })?;
                self.sync_node_labels(&name, labels)?;
                if let Some(node) = self.nodes.write().get_mut(&name) {
                    node.source = source;
                }
                info!(node = %name, "node updated (resources changed)");
            }
            RegistrationAction::Register => {
                self.propose(WalOperation::NodeRegister {
                    name: name.clone(),
                    resources,
                    address,
                    port,
                    wg_pubkey,
                    version,
                    labels,
                })?;
                if let Some(node) = self.nodes.write().get_mut(&name) {
                    node.source = source;
                    node.agent_start_time = Some(Utc::now());
                }
                info!(node = %name, "node registered");
            }
        }
        Ok(())
    }

    /// Sync node labels if they differ from the expected set.
    /// Proposes a `NodeLabelsUpdate` WAL operation when there's a mismatch.
    fn sync_node_labels(
        &self,
        node_name: &str,
        new_labels: HashMap<String, String>,
    ) -> anyhow::Result<()> {
        if let Some(existing) = self.get_node(node_name) {
            if existing.labels != new_labels {
                let remove: Vec<String> = existing
                    .labels
                    .keys()
                    .filter(|k| !new_labels.contains_key(*k))
                    .cloned()
                    .collect();
                self.propose(WalOperation::NodeLabelsUpdate {
                    name: node_name.to_string(),
                    set: new_labels,
                    remove,
                })?;
                info!(node = %node_name, "node labels synced on re-registration");
            }
        }
        Ok(())
    }

    /// Update node heartbeat telemetry (load, memory, timestamp).
    ///
    /// Returns `true` if the node was found, `false` if unknown.
    /// State recovery is handled separately by `check_node_health`, which
    /// detects the fresh `last_heartbeat` and proposes a WAL-backed transition.
    pub fn update_heartbeat(&self, name: &str, cpu_load: u32, free_memory_mb: u64) -> bool {
        let mut nodes = self.nodes.write();
        if let Some(node) = nodes.get_mut(name) {
            node.cpu_load = cpu_load;
            node.free_memory_mb = free_memory_mb;
            node.last_heartbeat = Some(Utc::now());
            true
        } else {
            false
        }
    }

    /// Update a node's WireGuard mesh public key from a heartbeat when it appears or changes (mesh
    /// came up after registration, or `spur0` was recreated). In-memory like `update_heartbeat` —
    /// the mesh reconcile loop reads live inventory, so this is enough to include the node in
    /// ApplyMesh without a spurd restart. Returns true if the stored key changed.
    pub fn update_node_wg_pubkey(&self, name: &str, pubkey: &str) -> bool {
        if pubkey.is_empty() {
            return false;
        }
        let mut nodes = self.nodes.write();
        if let Some(node) = nodes.get_mut(name) {
            if node.wg_pubkey.as_deref() != Some(pubkey) {
                node.wg_pubkey = Some(pubkey.to_string());
                return true;
            }
        }
        false
    }

    /// Create an admission token and persist via Raft.
    pub fn create_token(
        &self,
        ttl_secs: Option<u32>,
    ) -> anyhow::Result<(spur_core::admission::AdmissionToken, String)> {
        let (token, full_string) = spur_core::admission::generate_token(ttl_secs);
        self.propose(WalOperation::TokenCreate {
            token: token.clone(),
        })?;
        Ok((token, full_string))
    }

    /// List all admission tokens (without secrets).
    pub fn list_tokens(&self) -> Vec<spur_core::admission::AdmissionToken> {
        self.tokens.read().values().cloned().collect()
    }

    /// Revoke an admission token by ID.
    pub fn revoke_token(&self, token_id: &str) -> anyhow::Result<()> {
        if !self.tokens.read().contains_key(token_id) {
            anyhow::bail!("token not found: {}", token_id);
        }
        self.propose(WalOperation::TokenRevoke {
            token_id: token_id.to_string(),
        })?;
        Ok(())
    }

    /// Get a read-only reference to the token store for validation.
    pub fn get_tokens(&self) -> HashMap<String, spur_core::admission::AdmissionToken> {
        self.tokens.read().clone()
    }

    /// Get all nodes.
    pub fn get_nodes(&self) -> Vec<Node> {
        self.nodes.read().values().cloned().collect()
    }

    /// Get a node by name.
    pub fn get_node(&self, name: &str) -> Option<Node> {
        self.nodes.read().get(name).cloned()
    }

    /// Get all partitions.
    pub fn get_partitions(&self) -> Vec<Partition> {
        self.partitions.read().clone()
    }

    /// Hold a job (prevent scheduling).
    pub fn hold_job(&self, job_id: JobId) -> anyhow::Result<()> {
        let old_priority = {
            let jobs = self.jobs.read();
            let job = jobs
                .get(&job_id)
                .ok_or_else(|| anyhow::anyhow!("job {} not found", job_id))?;
            if job.state != JobState::Pending {
                anyhow::bail!(
                    "can only hold pending jobs (job {} is {:?})",
                    job_id,
                    job.state
                );
            }
            job.priority
        };

        self.propose(WalOperation::JobPriorityChange {
            job_id,
            old_priority,
            new_priority: 0,
            pending_reason: Some(PendingReason::Held),
            reset_requeue_count: false,
            clear_reservation: false,
        })?;
        info!(job_id, "job held");
        Ok(())
    }

    /// Hold a job that exhausted automatic requeues (`JobHoldMaxRequeue`).
    fn hold_job_at_max_requeue(&self, job_id: JobId) -> anyhow::Result<()> {
        let mut state = {
            let jobs = self.jobs.read();
            let job = jobs
                .get(&job_id)
                .ok_or_else(|| anyhow::anyhow!("job {} not found", job_id))?;
            job.state
        };

        if state == JobState::Running {
            self.propose(WalOperation::JobComplete {
                job_id,
                exit_code: -1,
                state: JobState::Failed,
            })?;
            state = JobState::Failed;
        }

        if matches!(
            state,
            JobState::Preempted | JobState::Timeout | JobState::NodeFail | JobState::Failed
        ) {
            self.propose(WalOperation::job_state_change_held_pending(
                job_id,
                state,
                PendingReason::JobHoldMaxRequeue,
            ))?;
            info!(job_id, "job held at max requeue limit");
            return Ok(());
        }

        if state != JobState::Pending {
            anyhow::bail!(
                "cannot hold job {} at max requeue from state {:?}",
                job_id,
                state
            );
        }

        let needs_hold = self.jobs.read().get(&job_id).is_some_and(|j| {
            j.pending_reason != PendingReason::JobHoldMaxRequeue || j.priority != 0
        });
        if needs_hold {
            let old_priority = self
                .jobs
                .read()
                .get(&job_id)
                .map(|j| j.priority)
                .unwrap_or(0);
            self.propose(WalOperation::JobPriorityChange {
                job_id,
                old_priority,
                new_priority: 0,
                pending_reason: Some(PendingReason::JobHoldMaxRequeue),
                reset_requeue_count: false,
                clear_reservation: false,
            })?;
        }
        info!(job_id, "job held at max requeue limit");
        Ok(())
    }

    /// Release a held job.
    pub fn release_job(&self, job_id: JobId) -> anyhow::Result<()> {
        let (reset_requeue, clear_reservation, old_priority) = {
            let jobs = self.jobs.read();
            let job = jobs
                .get(&job_id)
                .ok_or_else(|| anyhow::anyhow!("job {} not found", job_id))?;
            if !job.pending_reason.is_scheduling_hold() {
                anyhow::bail!("job {} is not held", job_id);
            }
            (
                job.pending_reason == PendingReason::JobHoldMaxRequeue,
                job.pending_reason == PendingReason::ReservationDeleted,
                job.priority,
            )
        };

        self.propose(WalOperation::JobPriorityChange {
            job_id,
            old_priority,
            new_priority: 1000,
            pending_reason: Some(PendingReason::Priority),
            reset_requeue_count: reset_requeue,
            clear_reservation,
        })?;
        info!(job_id, "job released");
        Ok(())
    }

    /// Update job properties.
    #[allow(clippy::too_many_arguments)]
    pub fn update_job(
        &self,
        job_id: JobId,
        time_limit: Option<chrono::Duration>,
        priority: Option<u32>,
        partition: Option<String>,
        comment: Option<String>,
        account: Option<String>,
        qos: Option<String>,
    ) -> anyhow::Result<()> {
        let (job_user, job_account) = {
            let jobs = self.jobs.read();
            let job = jobs
                .get(&job_id)
                .ok_or_else(|| anyhow::anyhow!("job {} not found", job_id))?;
            (job.spec.user.clone(), job.spec.account.clone())
        };

        // Reject before mutating: an unknown QOS resolves to the limitless
        // default and an empty QOS clears enforcement — both reopen the bypass.
        let qos = match qos {
            Some(q) => {
                let q = q.trim().to_string();
                if q.is_empty() {
                    anyhow::bail!("cannot clear a job's QOS");
                }
                if self.qos_cache.get(&q).is_none() {
                    anyhow::bail!("QOS '{q}' does not exist");
                }
                let effective_account = account.as_deref().or(job_account.as_deref());
                if let Some(acct) = effective_account {
                    if !self.association_cache.qos_allowed(&job_user, acct, &q) {
                        anyhow::bail!(
                            "QOS '{q}' is not permitted for user '{job_user}' under account '{acct}'"
                        );
                    }
                }
                Some(q)
            }
            None => None,
        };

        if let Some(p) = priority {
            let old = self
                .jobs
                .read()
                .get(&job_id)
                .map(|j| j.priority)
                .unwrap_or(0);
            self.propose(WalOperation::JobPriorityChange {
                job_id,
                old_priority: old,
                new_priority: p,
                pending_reason: None,
                reset_requeue_count: false,
                clear_reservation: false,
            })?;
        }

        // Non-WAL-tracked fields: update directly
        let mut jobs = self.jobs.write();
        if let Some(job) = jobs.get_mut(&job_id) {
            if let Some(tl) = time_limit {
                job.spec.time_limit = Some(tl);
            }
            if let Some(part) = partition {
                job.spec.partition = Some(part);
            }
            if let Some(c) = comment {
                job.spec.comment = Some(c);
            }
            if let Some(a) = account {
                job.spec.account = Some(a);
            }
            if let Some(q) = qos {
                job.spec.qos = Some(q);
            }
        }
        info!(job_id, "job updated");
        Ok(())
    }

    /// Record the primary agent's resolved output paths for `scontrol`. Display-only
    /// advisory metadata written straight to the in-memory job — not applied via a WAL
    /// op (though it may ride along in a periodic snapshot), so a failover before the
    /// next snapshot falls back to the computed path.
    /// Empty paths stay `None` (a mixed-version agent decodes the fields as "").
    pub fn set_job_output_paths(&self, job_id: JobId, stdout_path: String, stderr_path: String) {
        let mut jobs = self.jobs.write();
        if let Some(job) = jobs.get_mut(&job_id) {
            job.actual_stdout_path = (!stdout_path.is_empty()).then_some(stdout_path);
            job.actual_stderr_path = (!stderr_path.is_empty()).then_some(stderr_path);
        }
    }

    /// Update node state (admin: drain, resume, etc.)
    ///
    /// When draining a node that still has running jobs, the state is set to
    /// `Draining` instead of `Drain`. Once all jobs complete (tracked in
    /// `complete_job`), the node transitions to `Drain`.
    pub fn update_node_state(
        &self,
        name: &str,
        state: NodeState,
        reason: Option<String>,
    ) -> anyhow::Result<()> {
        let (old_state, effective_state) = {
            let nodes = self.nodes.read();
            let node = nodes
                .get(name)
                .ok_or_else(|| anyhow::anyhow!("node {} not found", name))?;
            let old = node.state;
            let requested = old
                .transition(&NodeEvent::AdminSetState(state), node.admin_locked)
                .unwrap_or(state);
            // Drain with active allocations becomes Draining
            let effective = if requested == NodeState::Drain
                && (node.alloc_resources.cpus > 0 || node.alloc_resources.has_devices())
            {
                NodeState::Draining
            } else {
                requested
            };
            (old, effective)
        };

        // Admin-initiated state changes that move into a hold state are
        // locked so auto-recovery won't override the operator's intent.
        // Resuming to Idle clears the lock.
        let admin_locked = effective_state.is_admin_hold();

        self.propose(WalOperation::NodeStateChange {
            name: name.to_string(),
            old_state,
            new_state: effective_state,
            reason,
            admin_locked,
        })?;
        info!(node = %name, old = ?old_state, new = ?effective_state, "node state updated");
        Ok(())
    }

    pub fn update_node_labels(
        &self,
        name: &str,
        set: HashMap<String, String>,
        remove: &[String],
    ) -> anyhow::Result<()> {
        {
            let nodes = self.nodes.read();
            if !nodes.contains_key(name) {
                anyhow::bail!("node {} not found", name);
            }
        }
        self.propose(WalOperation::NodeLabelsUpdate {
            name: name.to_string(),
            set: set.clone(),
            remove: remove.to_vec(),
        })?;
        info!(node = %name, "node labels updated");
        Ok(())
    }

    /// assign a k0s role + allocated mesh IP + pod /24 to a node (replicated via Raft).
    /// Callers never touch `self.nodes`/`self.k0s` directly — that would bypass Raft.
    pub fn assign_node_k0s(
        &self,
        name: &str,
        role: spur_core::k0s::K0sRole,
        mesh_ip: &str,
        pod_cidr: &str,
    ) -> anyhow::Result<()> {
        {
            let nodes = self.nodes.read();
            if !nodes.contains_key(name) {
                anyhow::bail!("node {} not found", name);
            }
        }
        self.propose(WalOperation::NodeK0sAssign {
            name: name.to_string(),
            role,
            mesh_ip: mesh_ip.to_string(),
            pod_cidr: pod_cidr.to_string(),
        })?;
        info!(node = %name, ?role, "node k0s role assigned");
        Ok(())
    }

    /// set the cluster-wide k0s phase (+ optional control-plane node / reset flag).
    pub fn set_k0s_phase(
        &self,
        phase: spur_core::k0s::K0sPhase,
        control_plane_node: Option<String>,
        reset_requested: bool,
    ) -> anyhow::Result<()> {
        self.propose(WalOperation::K0sSetPhase {
            phase,
            control_plane_node,
            reset_requested,
        })?;
        info!(?phase, "k0s cluster phase set");
        Ok(())
    }

    /// snapshot of the current cluster-wide k0s state.
    pub fn k0s_state(&self) -> spur_core::k0s::K0sClusterState {
        self.k0s.read().clone()
    }

    /// Reconcile node liveness state with heartbeat data.
    /// Marks stale nodes Down and recovers nodes whose heartbeat has resumed.
    /// Returns finalized jobs from eviction so callers can send cancel RPCs.
    pub fn check_node_health(&self, timeout_secs: u64) -> Vec<JobFinalized> {
        let actions = {
            let nodes = self.nodes.read();
            let refs: Vec<&Node> = nodes.values().collect();
            evaluate_node_health(&refs, Utc::now(), timeout_secs)
        };
        self.apply_health_actions(actions)
    }

    fn apply_health_actions(&self, actions: Vec<HealthAction>) -> Vec<JobFinalized> {
        let mut evicted = Vec::new();
        for action in actions {
            match action {
                HealthAction::MarkDown {
                    name,
                    old_state,
                    admin_locked,
                } => {
                    warn!(node = %name, "node marked DOWN (heartbeat timeout)");
                    match self.propose(WalOperation::NodeStateChange {
                        name: name.clone(),
                        old_state,
                        new_state: NodeState::Down,
                        reason: Some("Not responding".into()),
                        admin_locked,
                    }) {
                        Ok(resp) => {
                            self.run_all_finalized_side_effects(&resp);
                            evicted.extend(resp.jobs_finalized);
                        }
                        Err(e) => {
                            warn!(error = %e, "failed to propose node DOWN");
                            continue;
                        }
                    }
                }
                HealthAction::Recover { name, old_state } => {
                    info!(node = %name, "node recovered (heartbeat resumed)");
                    if let Err(e) = self.propose(WalOperation::NodeStateChange {
                        name,
                        old_state,
                        new_state: NodeState::Idle,
                        reason: None,
                        admin_locked: false,
                    }) {
                        warn!(error = %e, "failed to propose node recovery");
                    }
                }
            }
        }
        evicted
    }

    /// Drain a node: stop scheduling new jobs on it. Running jobs finish naturally.
    /// Returns (actual_state, running_job_count).
    pub fn drain_node(
        &self,
        name: &str,
        reason: Option<String>,
    ) -> anyhow::Result<(NodeState, u32)> {
        let (old_state, running_count) = {
            let nodes = self.nodes.read();
            let node = nodes
                .get(name)
                .ok_or_else(|| anyhow::anyhow!("node '{}' not found", name))?;
            let jobs = self.jobs.read();
            let count = jobs
                .values()
                .filter(|j| {
                    matches!(
                        j.state,
                        JobState::Running | JobState::Completing | JobState::Suspended
                    ) && j.allocated_nodes.iter().any(|n| n == name)
                })
                .count() as u32;
            (node.state, count)
        };
        let target_state = if running_count > 0 {
            NodeState::Draining
        } else {
            NodeState::Drain
        };
        self.propose(WalOperation::NodeStateChange {
            name: name.to_string(),
            old_state,
            new_state: target_state,
            reason,
            admin_locked: true,
        })?;
        info!(node = %name, state = %target_state, "node drain requested");
        Ok((target_state, running_count))
    }

    /// Remove a node from the cluster. If `force`, evict running jobs first.
    /// Returns finalized jobs from eviction so callers can send cancel RPCs.
    pub fn remove_node(
        &self,
        name: &str,
        force: bool,
        reason: Option<String>,
    ) -> anyhow::Result<Vec<JobFinalized>> {
        {
            let nodes = self.nodes.read();
            if !nodes.contains_key(name) {
                anyhow::bail!("node '{}' not found", name);
            }
        }
        if !force {
            let jobs = self.jobs.read();
            let has_running = jobs.values().any(|j| {
                matches!(
                    j.state,
                    JobState::Running | JobState::Completing | JobState::Suspended
                ) && j.allocated_nodes.iter().any(|n| n == name)
            });
            if has_running {
                anyhow::bail!(
                    "node '{}' has running jobs; use --force to evict them",
                    name
                );
            }
        }

        let resp = self.propose(WalOperation::NodeRemove {
            name: name.to_string(),
            reason,
        })?;
        self.run_all_finalized_side_effects(&resp);
        Ok(resp.jobs_finalized)
    }

    /// Create a job step durably via Raft.
    pub fn create_step(&self, step: JobStep) -> anyhow::Result<()> {
        let job_id = step.job_id;
        let step_id = step.step_id;
        self.propose(WalOperation::JobStepCreate {
            step: Box::new(step),
        })?;
        debug!(job_id, step_id, "step created");
        Ok(())
    }

    /// Record an srun step's completion via Raft so the step exit code and the
    /// job's running-max DerivedExitCode are durable and replay-consistent.
    #[allow(clippy::result_large_err)]
    pub fn record_step_complete(
        &self,
        job_id: JobId,
        step_id: u32,
        exit_code: i32,
    ) -> anyhow::Result<()> {
        self.propose(WalOperation::JobStepComplete {
            job_id,
            step_id,
            exit_code,
        })?;
        Ok(())
    }

    /// Get all steps for a job.
    pub fn get_steps(&self, job_id: JobId) -> Vec<JobStep> {
        self.steps
            .read()
            .iter()
            .filter(|((jid, _), _)| *jid == job_id)
            .map(|(_, step)| step.clone())
            .collect()
    }

    /// Get pending jobs sorted by priority, filtering out held and dependency-blocked jobs.
    /// Recomputes effective priority using QoS, age, and partition tier before sorting.
    pub fn pending_jobs(&self) -> Vec<Job> {
        let jobs = self.jobs.read();
        let mut pending: Vec<Job> = jobs
            .values()
            .filter(|j| j.state == JobState::Pending && !j.pending_reason.is_scheduling_hold())
            .cloned()
            .collect();

        // Drop jobs blocked by partition state/config (shares partition_block()
        // with tag_blocked_pending_reasons() so drop and shown reason agree).
        {
            let partitions = self.partitions.read();
            pending.retain(|job| partition_block(job, &partitions).is_none());
        }

        // Check dependencies
        let get_job = |id: JobId| -> Option<Job> { jobs.get(&id).cloned() };
        let get_array_tasks = |id: JobId| -> Vec<Job> {
            jobs.values()
                .filter(|j| j.spec.array_job_id == Some(id))
                .cloned()
                .collect()
        };
        let get_jobs_by_name_user = |name: &str, user: &str| -> Vec<Job> {
            jobs.values()
                .filter(|j| j.spec.name == name && j.spec.user == user)
                .cloned()
                .collect()
        };

        pending.retain(|job| {
            if job.spec.dependency.is_empty() {
                return true;
            }
            use spur_core::dependency::{check_dependencies, DependencyResult};
            match check_dependencies(job, &get_job, &get_array_tasks, &get_jobs_by_name_user) {
                DependencyResult::Satisfied => true,
                // Waiting and Failed are both filtered out of scheduling here.
                // Failed jobs are separately cancelled by
                // cancel_unsatisfiable_dependency_jobs() in the scheduler loop,
                // which can take the write lock this read-locked scan cannot.
                DependencyResult::Waiting | DependencyResult::Failed => false,
            }
        });

        // Filter out jobs whose begin_time is in the future (not yet eligible)
        {
            let now = Utc::now();
            pending.retain(|job| {
                if let Some(begin) = job.spec.begin_time {
                    if now < begin {
                        return false; // Not yet eligible
                    }
                }
                true
            });
        }

        // Enforce array max_concurrent: suppress tasks if too many siblings already running
        let running_array_counts: std::collections::HashMap<JobId, u32> = {
            let mut counts = std::collections::HashMap::new();
            for j in jobs.values() {
                if j.state == JobState::Running {
                    if let Some(aid) = j.spec.array_job_id {
                        *counts.entry(aid).or_insert(0) += 1;
                    }
                }
            }
            counts
        };
        pending.retain(|job| {
            if let (Some(aid), Some(max)) = (job.spec.array_job_id, job.spec.array_max_concurrent) {
                let running = running_array_counts.get(&aid).copied().unwrap_or(0);
                if running >= max {
                    return false; // Throttled — too many siblings running
                }
            }
            true
        });

        // Resolve once, reuse for both the QoS-limit check and the priority
        // adjustment below, instead of looking it up twice per job.
        let qos_by_job: HashMap<JobId, Qos> = pending
            .iter()
            .map(|job| (job.job_id, self.resolve_qos(job)))
            .collect();

        // Reservation validation: reject jobs targeting expired/nonexistent reservations
        {
            let reservations = self.get_reservations();
            let now = Utc::now();
            pending.retain(|job| reservation_block(job, &reservations, now).is_none());
        }

        // Recompute effective priority with age + partition tier
        let now = Utc::now();
        let partitions = self.partitions.read();
        let reservations = self.get_reservations();
        for job in &mut pending {
            let age_minutes = (now - job.submit_time).num_minutes().max(0);
            let partition_tier =
                spur_core::partition::max_priority_tier(job.spec.partition.as_deref(), &partitions);
            let fair_share = self
                .fairshare_cache
                .get(&job.spec.user, job.spec.account.as_deref().unwrap_or(""));
            job.priority = compute_effective_priority(
                job.priority,
                fair_share,
                age_minutes,
                partition_tier,
                &qos_by_job[&job.job_id],
            );
        }
        drop(partitions);

        // QoS floors priority at 1, making ties more likely; break on job_id
        // for deterministic ordering.
        pending.sort_by(|a, b| {
            let a_res = reservation::job_has_active_reservation(a, &reservations, now);
            let b_res = reservation::job_has_active_reservation(b, &reservations, now);
            b_res
                .cmp(&a_res)
                .then(b.priority.cmp(&a.priority))
                .then(a.job_id.cmp(&b.job_id))
        });

        // Account/association then QoS aggregate enforcement, in priority order.
        // Each kept job reserves its footprint against the group (`grp_tres`) and
        // per-user caps so lower-priority jobs later in the same pass see the
        // reduced headroom — otherwise several pending jobs could each fit against
        // the static running total yet collectively exceed the cap. Mirrors the
        // license/BB reservation passes below. A job whose own request already
        // exceeds the cap against the running total is reported by
        // tag_blocked_pending_reasons(); a job deferred only because same-pass
        // higher-priority siblings consumed the headroom is picked up on the next
        // cycle once those siblings are Running (same transient the license/BB
        // reservation passes already have).
        {
            let mut reserved = PassReservations::default();
            pending.retain(|job| {
                if account_block_with(job, &self.association_cache, &jobs, &reserved).is_some() {
                    return false;
                }
                if qos_block_with(job, &qos_by_job[&job.job_id], &jobs, &reserved).is_some() {
                    return false;
                }
                reserved.reserve(job);
                true
            });
        }

        // License reservation, in priority order. `remaining` starts from current
        // availability (config total minus licenses held by running jobs) and each
        // kept job reserves its licenses so lower-priority jobs in the same pass see
        // the reduced availability — preventing a single pass from over-subscribing.
        // An absolute shortage is also reported as `Licenses` by
        // tag_blocked_pending_reasons() via license_block().
        {
            let mut remaining = self.available_licenses_with(&jobs);
            pending.retain(|job| {
                let req = extract_license_requirements(&job.spec);
                if req
                    .iter()
                    .any(|(lic, n)| remaining.get(lic).copied().unwrap_or(0) < *n)
                {
                    return false;
                }
                for (lic, n) in &req {
                    if let Some(avail) = remaining.get_mut(lic) {
                        *avail = avail.saturating_sub(*n);
                    }
                }
                true
            });
        }

        // Burst-buffer gate, after licenses (the most downstream consumable;
        // staging happens last, just before dispatch). Two holds:
        //   - capacity shortage: a job needing more BB than is free is dropped
        //     (also reported `BurstBufferResources` by tag_blocked...).
        //   - mid-stage-in: a job whose capacity is reserved but whose stage-in
        //     has not completed (`Staging`) is NOT dispatchable yet; only a
        //     `Ready` (or no-BB) job passes. `remaining` reserves capacity
        //     highest-priority-first so one pass can't over-subscribe the pool.
        {
            let mut remaining = self.available_bb_with(&jobs);
            pending.retain(|job| {
                let req = extract_bb_requirement(&job.spec);
                if req == 0 {
                    return true; // no BB -> unaffected by the gate
                }
                match job.bb_stage_state {
                    // Capacity already reserved in bb_capacity_in_use(), so
                    // don't double-count `remaining`.
                    BbStageState::Ready => true,
                    BbStageState::Staging => false,
                    BbStageState::None => {
                        if req > remaining {
                            return false;
                        }
                        remaining = remaining.saturating_sub(req);
                        false
                    }
                }
            });
        }

        pending
    }

    /// Licenses held by jobs actively occupying resources
    /// (Running/Suspended/Completing). Pending and terminal jobs hold none.
    fn licenses_in_use(jobs: &HashMap<JobId, Job>) -> HashMap<String, u64> {
        let mut used: HashMap<String, u64> = HashMap::new();
        for job in jobs.values() {
            if matches!(
                job.state,
                JobState::Running | JobState::Suspended | JobState::Completing
            ) {
                for (lic, n) in extract_license_requirements(&job.spec) {
                    *used.entry(lic).or_insert(0) += n;
                }
            }
        }
        used
    }

    /// Currently-available licenses: configured total minus licenses in use.
    /// Derived from the live job set, so it always reflects config and cannot
    /// drift (no mutable pool). Caller supplies the already-locked jobs map.
    fn available_licenses_with(&self, jobs: &HashMap<JobId, Job>) -> HashMap<String, u64> {
        let total = self.license_pool.read();
        let used = Self::licenses_in_use(jobs);
        total
            .iter()
            .map(|(lic, tot)| {
                (
                    lic.clone(),
                    tot.saturating_sub(used.get(lic).copied().unwrap_or(0)),
                )
            })
            .collect()
    }

    /// Currently-available licenses (locks the job table). See
    /// [`available_licenses_with`](Self::available_licenses_with).
    #[cfg(test)]
    fn available_licenses(&self) -> HashMap<String, u64> {
        let jobs = self.jobs.read();
        self.available_licenses_with(&jobs)
    }

    /// Burst-buffer capacity (GB) reserved by jobs that have entered staging or
    /// are actively occupying resources. A BB job reserves its capacity when it
    /// transitions to `Staging`; it holds the reservation through Ready, Running,
    /// Suspended, and Completing, releasing only when it leaves the active set.
    /// Pending jobs that have not yet staged (`BbStageState::None`) hold nothing.
    fn bb_capacity_in_use(jobs: &HashMap<JobId, Job>) -> u64 {
        let mut used = 0u64;
        for job in jobs.values() {
            let holds = match job.state {
                JobState::Running | JobState::Suspended | JobState::Completing => true,
                JobState::Pending => job.bb_stage_state != BbStageState::None,
                _ => false,
            };
            if holds {
                used = used.saturating_add(extract_bb_requirement(&job.spec));
            }
        }
        used
    }

    /// Currently-free BB capacity (GB): configured total minus capacity reserved
    /// by staging/active jobs. Derived from the live job set so it always tracks
    /// config and cannot drift. Caller supplies the already-locked jobs map.
    fn available_bb_with(&self, jobs: &HashMap<JobId, Job>) -> u64 {
        let total = *self.burst_buffer_total_gb.read();
        spur_core::burst_buffer::free_capacity_gb(total, Self::bb_capacity_in_use(jobs))
    }

    /// Currently-free BB capacity (locks the job table). See
    /// [`available_bb_with`](Self::available_bb_with).
    #[cfg(test)]
    fn available_bb(&self) -> u64 {
        let jobs = self.jobs.read();
        self.available_bb_with(&jobs)
    }

    /// Advance burst-buffer staging for pending BB jobs (leader-only; takes the
    /// write lock). Reserves capacity highest-priority-first for jobs that have
    /// not yet staged, moving them `None -> Staging`. Stage-in itself is
    /// performed out-of-band; [`complete_bb_stage_in`](Self::complete_bb_stage_in)
    /// advances `Staging -> Ready`. A `Ready` job is dispatchable; a `Staging`
    /// job is held with `BurstBufferStageIn`. Returns the ids moved into staging.
    ///
    /// NOTE: the actual data movement (the real stage-in) is a follow-up; this
    /// drives the controller-side state machine and the scheduler hold only.
    pub fn advance_bb_staging(&self) -> Vec<JobId> {
        let mut started = Vec::new();
        let mut jobs = self.jobs.write();
        let total = *self.burst_buffer_total_gb.read();
        let mut remaining =
            spur_core::burst_buffer::free_capacity_gb(total, Self::bb_capacity_in_use(&jobs));

        // Reserve highest-priority-first so a scarce pool is not over-subscribed
        // and low-priority jobs do not jump ahead of blocked high-priority ones.
        let mut candidates: Vec<JobId> = jobs
            .values()
            .filter(|j| {
                j.state == JobState::Pending
                    && j.bb_stage_state == BbStageState::None
                    && !j.pending_reason.is_scheduling_hold()
                    && j.pending_reason != PendingReason::DeadLine
                    && extract_bb_requirement(&j.spec) > 0
            })
            .map(|j| j.job_id)
            .collect();
        // Sort by priority desc, then job_id asc for determinism.
        candidates.sort_by_key(|id| {
            jobs.get(id)
                .map(|j| (std::cmp::Reverse(j.priority), *id))
                .unwrap_or((std::cmp::Reverse(0), *id))
        });

        for id in candidates {
            let req = jobs
                .get(&id)
                .map(|j| extract_bb_requirement(&j.spec))
                .unwrap_or(0);
            if req == 0 || req > remaining {
                continue;
            }
            if let Some(job) = jobs.get_mut(&id) {
                job.bb_stage_state = BbStageState::Staging;
                job.pending_reason = PendingReason::BurstBufferStageIn;
                remaining = remaining.saturating_sub(req);
                started.push(id);
            }
        }
        started
    }

    /// Drive in-flight burst-buffer stage-ins to completion and return the ids
    /// advanced to `Ready`. Leader-only; called once per scheduler cycle.
    ///
    /// FOLLOW-UP SEAM: real stage-in is asynchronous data movement performed by
    /// the node agent, which would call `complete_bb_stage_in()` over a gRPC
    /// report once the bytes land. Until that round-trip exists, the controller
    /// completes staging here so the lifecycle (`None -> Staging -> Ready ->
    /// dispatch`) is end-to-end functional. Replacing this with an agent report
    /// is the only remaining work; the state machine and scheduler hold are real.
    pub fn drive_bb_stage_in(&self) -> Vec<JobId> {
        let staging: Vec<JobId> = {
            let jobs = self.jobs.read();
            jobs.values()
                .filter(|j| {
                    j.state == JobState::Pending && j.bb_stage_state == BbStageState::Staging
                })
                .map(|j| j.job_id)
                .collect()
        };
        staging
            .into_iter()
            .filter(|id| self.complete_bb_stage_in(*id))
            .collect()
    }

    /// Mark a job's burst-buffer stage-in complete (`Staging -> Ready`), making
    /// it dispatchable. Returns true if the job was advanced. The agent-side
    /// data mover calls this once the bytes have landed (follow-up); tests and
    /// the controller drive it directly.
    pub fn complete_bb_stage_in(&self, job_id: JobId) -> bool {
        let mut jobs = self.jobs.write();
        if let Some(job) = jobs.get_mut(&job_id) {
            if job.state == JobState::Pending && job.bb_stage_state == BbStageState::Staging {
                job.bb_stage_state = BbStageState::Ready;
                if job.pending_reason == PendingReason::BurstBufferStageIn {
                    job.pending_reason = PendingReason::None;
                }
                self.scheduler_notify.notify_one();
                return true;
            }
        }
        false
    }

    /// Cancel pending jobs whose dependencies can never be satisfied (Slurm's
    /// `DependencyNeverSatisfied`) and tag still-waiting ones with
    /// `PendingReason::Dependency`. Returns the cancelled ids. Leader-only; takes
    /// the write lock `pending_jobs()` cannot. Closes the silent-deadlock gap
    /// where a `Failed` dependency left the job PENDING forever.
    pub fn cancel_unsatisfiable_dependency_jobs(&self) -> Vec<JobId> {
        use spur_core::dependency::{check_dependencies, DependencyResult};
        use spur_core::job::PendingReason;

        // Snapshot under a read lock to evaluate dependencies.
        let (to_cancel, to_wait): (Vec<JobId>, Vec<JobId>) = {
            let jobs = self.jobs.read();
            let get_job = |id: JobId| -> Option<Job> { jobs.get(&id).cloned() };
            let get_array_tasks = |id: JobId| -> Vec<Job> {
                jobs.values()
                    .filter(|j| j.spec.array_job_id == Some(id))
                    .cloned()
                    .collect()
            };
            let get_jobs_by_name_user = |name: &str, user: &str| -> Vec<Job> {
                jobs.values()
                    .filter(|j| j.spec.name == name && j.spec.user == user)
                    .cloned()
                    .collect()
            };

            let mut cancel = Vec::new();
            let mut wait = Vec::new();
            for job in jobs.values() {
                if job.state != JobState::Pending
                    || job.spec.dependency.is_empty()
                    || job.pending_reason.is_scheduling_hold()
                {
                    continue;
                }
                match check_dependencies(job, &get_job, &get_array_tasks, &get_jobs_by_name_user) {
                    DependencyResult::Failed => cancel.push(job.job_id),
                    DependencyResult::Waiting => wait.push(job.job_id),
                    DependencyResult::Satisfied => {}
                }
            }
            (cancel, wait)
        };

        // Tag waiting jobs (write lock).
        if !to_wait.is_empty() {
            let mut jobs = self.jobs.write();
            for id in &to_wait {
                if let Some(j) = jobs.get_mut(id) {
                    // Don't clobber Held or DeadLine — matches
                    // update_pending_reasons().
                    if j.state == JobState::Pending
                        && !j.pending_reason.is_scheduling_hold()
                        && j.pending_reason != PendingReason::DeadLine
                    {
                        j.pending_reason = PendingReason::Dependency;
                    }
                }
            }
        }

        // Finalize unsatisfiable jobs via the WAL so resources/accounting fire.
        let mut cancelled = Vec::new();
        for id in to_cancel {
            // Re-check Pending: the snapshot's read lock was released, so the
            // job may have started concurrently. Running -> Cancelled is a valid
            // WAL transition that would otherwise destroy live work.
            if self.jobs.read().get(&id).map(|j| j.state) != Some(JobState::Pending) {
                continue;
            }
            match self.propose(WalOperation::JobComplete {
                job_id: id,
                exit_code: -1,
                state: JobState::Cancelled,
            }) {
                Ok(resp) => {
                    self.run_all_finalized_side_effects(&resp);
                    info!(job_id = id, "job cancelled: dependency never satisfied");
                    cancelled.push(id);
                }
                Err(e) => {
                    warn!(job_id = id, error = %e, "failed to cancel unsatisfiable-dependency job");
                }
            }
        }
        cancelled
    }

    /// Set `pending_reason` for jobs `pending_jobs()` drops from scheduling
    /// (dependency/QoS/license/reservation), which never reach
    /// `update_pending_reasons()` and would otherwise show a stale reason.
    /// Leader-only; mirrors `cancel_unsatisfiable_dependency_jobs()`.
    pub fn tag_blocked_pending_reasons(&self) {
        use spur_core::job::PendingReason;

        // Evaluate under read locks; release before taking the write lock.
        let blocked: Vec<(JobId, PendingReason)> = {
            let jobs = self.jobs.read();
            let reservations = self.get_reservations();
            let now = Utc::now();
            let available = self.available_licenses_with(&jobs);
            let partitions = self.partitions.read();
            let bb_free = self.available_bb_with(&jobs);

            // Dependency outranks QoS/Licenses/Reservation in pending_jobs() and
            // is tagged just before this pass, so re-check it first (same closures).
            use spur_core::dependency::{check_dependencies, DependencyResult};
            let get_job = |id: JobId| -> Option<Job> { jobs.get(&id).cloned() };
            let get_array_tasks = |id: JobId| -> Vec<Job> {
                jobs.values()
                    .filter(|j| j.spec.array_job_id == Some(id))
                    .cloned()
                    .collect()
            };
            let get_jobs_by_name_user = |name: &str, user: &str| -> Vec<Job> {
                jobs.values()
                    .filter(|j| j.spec.name == name && j.spec.user == user)
                    .cloned()
                    .collect()
            };
            let dependency_block = |job: &Job| -> Option<PendingReason> {
                if job.spec.dependency.is_empty() {
                    return None;
                }
                match check_dependencies(job, &get_job, &get_array_tasks, &get_jobs_by_name_user) {
                    DependencyResult::Waiting | DependencyResult::Failed => {
                        Some(PendingReason::Dependency)
                    }
                    DependencyResult::Satisfied => None,
                }
            };

            // A BB job mid-stage-in displays `BurstBufferStageIn`; one short of
            // free capacity displays `BurstBufferResources`. Staging is checked
            // first so a job that already reserved capacity isn't mislabeled as
            // a resource shortage.
            let bb_block = |job: &Job| -> Option<PendingReason> {
                if job.bb_stage_state == BbStageState::Staging {
                    return Some(PendingReason::BurstBufferStageIn);
                }
                burst_buffer_block(job, bb_free)
            };

            jobs.values()
                .filter(|job| {
                    let begin_held = job.pending_reason == PendingReason::BeginTime
                        && job.spec.begin_time.is_some_and(|b| now < b);
                    job.state == JobState::Pending
                        && !job.pending_reason.is_scheduling_hold()
                        && job.pending_reason != PendingReason::DeadLine
                        && !begin_held
                })
                .filter_map(|job| {
                    // Same drop order as pending_jobs(): Part -> Dep -> Assoc ->
                    // QoS -> Resv -> Lic -> BB (partition block is permanent, so
                    // first; BB is last because staging runs just before dispatch).
                    partition_block(job, &partitions)
                        .or_else(|| dependency_block(job))
                        .or_else(|| account_block_for(job, &self.association_cache, &jobs))
                        .or_else(|| qos_block_for(job, &self.resolve_qos(job), &jobs))
                        .or_else(|| reservation_block(job, &reservations, now))
                        .or_else(|| license_block(job, &available))
                        .or_else(|| bb_block(job))
                        .map(|reason| (job.job_id, reason))
                })
                .collect()
        };

        if blocked.is_empty() {
            return;
        }

        let mut jobs = self.jobs.write();
        let now = Utc::now();
        for (id, reason) in blocked {
            if let Some(j) = jobs.get_mut(&id) {
                // Re-check under the write lock: the read snapshot was released,
                // so the job may have started or been held/deadlined since.
                let begin_held = j.pending_reason == PendingReason::BeginTime
                    && j.spec.begin_time.is_some_and(|b| now < b);
                if j.state == JobState::Pending
                    && !j.pending_reason.is_scheduling_hold()
                    && j.pending_reason != PendingReason::DeadLine
                    && !begin_held
                {
                    j.pending_reason = reason;
                }
            }
        }
    }

    /// Create a new reservation (validated, persisted via Raft).
    pub fn create_reservation(&self, mut res: Reservation) -> Result<(), ReservationError> {
        if self.reservations.read().iter().any(|r| r.name == res.name) {
            return Err(ReservationError::already_exists(format!(
                "reservation '{}' already exists",
                res.name
            )));
        }
        let known: std::collections::HashSet<String> = self.nodes.read().keys().cloned().collect();
        res.nodes = normalize_node_list(&res.nodes, &known).map_err(ReservationError::invalid)?;
        self.validate_reservation_job_overlap(&res, None)
            .map_err(|e| ReservationError::invalid(e.to_string()))?;
        self.validate_reservation_storage_overlap(&res, None)
            .map_err(|e| ReservationError::invalid(e.to_string()))?;
        let name = res.name.clone();
        let resp = self
            .propose(WalOperation::ReservationCreate { reservation: res })
            .map_err(|e| ReservationError::raft(e.to_string()))?;
        if !resp.reservation_created {
            return Err(ReservationError::already_exists(format!(
                "reservation '{}' already exists",
                name
            )));
        }
        Ok(())
    }

    /// Update an existing reservation (validated, persisted via Raft). The
    /// requesting `user` must be the reservation owner, root, or empty (trusted
    /// internal calls); legacy reservations with no recorded owner are
    /// modifiable by anyone.
    #[allow(clippy::too_many_arguments)]
    pub fn update_reservation(
        &self,
        name: &str,
        duration_minutes: u32,
        add_nodes: &[String],
        remove_nodes: &[String],
        add_users: &[String],
        remove_users: &[String],
        add_accounts: &[String],
        remove_accounts: &[String],
        user: &str,
    ) -> Result<(), ReservationError> {
        let mut preview = self
            .reservations
            .read()
            .iter()
            .find(|r| r.name == name)
            .cloned()
            .ok_or_else(|| {
                ReservationError::not_found(format!("reservation '{}' not found", name))
            })?;

        if !preview.can_be_managed_by(user) {
            return Err(ReservationError::permission_denied(format!(
                "user '{}' cannot modify reservation '{}' owned by '{}'",
                user, name, preview.owner
            )));
        }

        if duration_minutes > 0 {
            preview.end_time =
                preview.start_time + chrono::Duration::minutes(duration_minutes as i64);
        }
        let known: std::collections::HashSet<String> = self.nodes.read().keys().cloned().collect();
        let mut add_expanded = Vec::new();
        for n in add_nodes {
            add_expanded.extend(
                normalize_node_list(std::slice::from_ref(n), &known)
                    .map_err(ReservationError::invalid)?,
            );
        }
        for node in &add_expanded {
            if !preview.nodes.contains(node) {
                preview.nodes.push(node.clone());
            }
        }
        preview.nodes.retain(|n| !remove_nodes.contains(n));
        for add_user in add_users {
            if !preview.users.contains(add_user) {
                preview.users.push(add_user.clone());
            }
        }
        preview.users.retain(|u| !remove_users.contains(u));
        for account in add_accounts {
            if !preview.accounts.contains(account) {
                preview.accounts.push(account.clone());
            }
        }
        preview.accounts.retain(|a| !remove_accounts.contains(a));

        self.validate_reservation_job_overlap(&preview, Some(name))
            .map_err(|e| ReservationError::invalid(e.to_string()))?;
        self.validate_reservation_storage_overlap(&preview, Some(name))
            .map_err(|e| ReservationError::invalid(e.to_string()))?;

        self.propose(WalOperation::ReservationUpdate {
            name: name.to_string(),
            duration_minutes,
            add_nodes: add_expanded,
            remove_nodes: remove_nodes.to_vec(),
            add_users: add_users.to_vec(),
            remove_users: remove_users.to_vec(),
            add_accounts: add_accounts.to_vec(),
            remove_accounts: remove_accounts.to_vec(),
        })
        .map_err(|e| ReservationError::raft(e.to_string()))?;
        Ok(())
    }

    /// Delete a reservation by name (persisted via Raft). The requesting `user`
    /// must be the reservation owner, root, or empty (trusted internal calls);
    /// legacy reservations with no recorded owner are deletable by anyone.
    pub fn delete_reservation(&self, name: &str, user: &str) -> Result<(), ReservationError> {
        {
            let reservations = self.reservations.read();
            let res = reservations
                .iter()
                .find(|r| r.name == name)
                .ok_or_else(|| {
                    ReservationError::not_found(format!("reservation '{}' not found", name))
                })?;
            if !res.can_be_managed_by(user) {
                return Err(ReservationError::permission_denied(format!(
                    "user '{}' cannot delete reservation '{}' owned by '{}'",
                    user, name, res.owner
                )));
            }
        }

        for job in self.jobs.read().values() {
            if !matches!(
                job.state,
                JobState::Running | JobState::Completing | JobState::Suspended
            ) {
                continue;
            }
            if job.spec.reservation.as_deref() == Some(name) {
                return Err(ReservationError::invalid(format!(
                    "reservation '{}' in use by running job {}",
                    name, job.job_id
                )));
            }
        }

        self.propose(WalOperation::ReservationDelete {
            name: name.to_string(),
        })
        .map_err(|e| ReservationError::raft(e.to_string()))?;
        Ok(())
    }

    /// Remove reservations past their end time when no jobs still reference them.
    pub fn purge_expired_reservations(&self) {
        let now = Utc::now();
        let expired: Vec<String> = self
            .reservations
            .read()
            .iter()
            .filter(|r| r.is_expired(now))
            .map(|r| r.name.clone())
            .collect();
        for name in expired {
            let in_use = self.jobs.read().values().any(|job| {
                matches!(
                    job.state,
                    JobState::Running | JobState::Completing | JobState::Suspended
                ) && job.spec.reservation.as_deref() == Some(name.as_str())
            });
            if in_use {
                continue;
            }
            if let Err(e) = self.propose(WalOperation::ReservationDelete { name: name.clone() }) {
                warn!(name = %name, error = %e, "failed to purge expired reservation");
            }
        }
    }

    /// Cancel running jobs whose reservation window has ended (after optional grace).
    pub fn enforce_reservation_end_times(&self) {
        let now = Utc::now();
        let grace = chrono::Duration::minutes(self.config.scheduler.resv_overrun_minutes as i64);
        let reservations: std::collections::HashMap<String, Reservation> = self
            .get_reservations()
            .into_iter()
            .map(|r| (r.name.clone(), r))
            .collect();
        let to_cancel: Vec<JobId> = self
            .jobs
            .read()
            .values()
            .filter_map(|job| {
                if !matches!(
                    job.state,
                    JobState::Running | JobState::Completing | JobState::Suspended
                ) {
                    return None;
                }
                let res_name = job.spec.reservation.as_ref()?;
                let res = reservations.get(res_name)?;
                if now > res.end_time + grace {
                    Some(job.job_id)
                } else {
                    None
                }
            })
            .collect();
        for job_id in to_cancel {
            if let Err(e) = self.complete_job(job_id, -1, JobState::Cancelled) {
                warn!(job_id, error = %e, "failed to cancel job after reservation ended");
            }
        }
    }

    fn validate_reservation_job_overlap(
        &self,
        res: &Reservation,
        except_name: Option<&str>,
    ) -> anyhow::Result<()> {
        if res.flags.ignore_jobs {
            return Ok(());
        }
        let jobs = self.jobs.read();
        if let Some((job_id, node)) =
            running_jobs_overlap_start(&jobs, &res.nodes, res.start_time, except_name)
        {
            anyhow::bail!(
                "requested nodes are busy (job {} on {} until after reservation start)",
                job_id,
                node
            );
        }
        Ok(())
    }

    fn validate_reservation_storage_overlap(
        &self,
        res: &Reservation,
        except_name: Option<&str>,
    ) -> anyhow::Result<()> {
        for existing in self.reservations.read().iter() {
            if except_name == Some(existing.name.as_str()) {
                continue;
            }
            if !reservation::reservations_overlap(res, existing) {
                continue;
            }
            if reservation::overlap_allowed(res, existing) {
                continue;
            }
            anyhow::bail!(
                "reservation overlaps with existing reservation '{}'",
                existing.name
            );
        }
        Ok(())
    }

    fn hold_jobs_for_deleted_reservation_jobs(jobs: &mut HashMap<JobId, Job>, name: &str) {
        for job in jobs.values_mut() {
            if job.state != JobState::Pending {
                continue;
            }
            if job.spec.reservation.as_deref() != Some(name) {
                continue;
            }
            if job.pending_reason.is_scheduling_hold() {
                continue;
            }
            job.priority = 0;
            job.pending_reason = PendingReason::ReservationDeleted;
        }
    }

    fn detach_jobs_from_deleted_reservation_jobs(jobs: &mut HashMap<JobId, Job>, name: &str) {
        for job in jobs.values_mut() {
            if job.state != JobState::Pending {
                continue;
            }
            if job.spec.reservation.as_deref() != Some(name) {
                continue;
            }
            job.spec.reservation = None;
            if job.pending_reason == PendingReason::ReservationDeleted {
                job.pending_reason = PendingReason::None;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_reservation_update_locked(
        reservations: &mut [Reservation],
        name: &str,
        duration_minutes: u32,
        add_nodes: &[String],
        remove_nodes: &[String],
        add_users: &[String],
        remove_users: &[String],
        add_accounts: &[String],
        remove_accounts: &[String],
    ) {
        let Some(res) = reservations.iter_mut().find(|r| r.name == name) else {
            return;
        };
        if duration_minutes > 0 {
            res.end_time = res.start_time + chrono::Duration::minutes(duration_minutes as i64);
        }
        for node in add_nodes {
            if !res.nodes.contains(node) {
                res.nodes.push(node.clone());
            }
        }
        res.nodes.retain(|n| !remove_nodes.contains(n));
        for user in add_users {
            if !res.users.contains(user) {
                res.users.push(user.clone());
            }
        }
        res.users.retain(|u| !remove_users.contains(u));
        for account in add_accounts {
            if !res.accounts.contains(account) {
                res.accounts.push(account.clone());
            }
        }
        res.accounts.retain(|a| !remove_accounts.contains(a));
    }

    /// Get all reservations.
    pub fn get_reservations(&self) -> Vec<Reservation> {
        self.reservations.read().clone()
    }

    /// Update pending_reason for jobs the scheduler couldn't schedule.
    ///
    /// Called after each scheduling cycle so that `squeue` shows a meaningful
    /// reason instead of always displaying "Priority".
    ///
    /// - `Resources`: no suitable node exists for the job right now
    ///   (partition mismatch, full, constraint not met, etc.)
    /// - `Priority`: suitable nodes exist but they're reserved for
    ///   higher-priority jobs (backfill timeline is in the future)
    /// - `NodeDown`: all nodes in the target partition are down/drained
    pub fn update_pending_reasons(
        &self,
        unscheduled: &[&spur_core::job::Job],
        cluster_state: &spur_sched::traits::ClusterState,
    ) {
        use spur_core::job::PendingReason;

        let mut jobs = self.jobs.write();

        for job in unscheduled {
            let job_entry = match jobs.get_mut(&job.job_id) {
                Some(j) => j,
                None => continue,
            };

            // Don't overwrite held jobs
            if job_entry.pending_reason.is_scheduling_hold() {
                continue;
            }
            // Don't overwrite a DeadLine reason set by the deadline-enforcement
            // path — the job is about to transition to JobState::Deadline this
            // tick; clobbering with Resources/NodeDown would mislead any
            // observer that polls in between.
            if job_entry.pending_reason == PendingReason::DeadLine {
                continue;
            }
            // Keep an active BeginTime hold (e.g. requeue-by-preemption) until
            // it lapses, then fall through to the real wait reason.
            if job_entry.pending_reason == PendingReason::BeginTime
                && job_entry.spec.begin_time.is_some_and(|b| Utc::now() < b)
            {
                continue;
            }

            if let Some(reason) = reservation_fence_reason(job, cluster_state) {
                job_entry.pending_reason = reason;
                continue;
            }

            // Reuse the scheduler's matcher so the reason can't disagree with
            // what backfill actually does.
            let placement = spur_sched::node_match::NodePlacement::new(job);
            let now = chrono::Utc::now();

            let needed = (job.spec.num_nodes as usize).max(1);

            let eligible: Vec<&spur_core::node::Node> = cluster_state
                .nodes
                .iter()
                .filter(|n| placement.eligible(n, cluster_state.reservations, now))
                .collect();

            let required = spur_sched::backfill::job_resource_request(job);
            if placement.nodelist_is_additive()
                && eligible.iter().any(|node| {
                    placement.is_listed(&node.name)
                        && node.total_resources.can_satisfy(&required)
                        && (!placement.matches(node, cluster_state.reservations, now)
                            || !node.can_satisfy_request(&required))
                })
            {
                job_entry.pending_reason = PendingReason::ReqNodeNotAvail;
                continue;
            }

            // Fewer eligible nodes than requested: unschedulable as written.
            if eligible.len() < needed {
                let partition_size = cluster_state
                    .nodes
                    .iter()
                    .filter(|n| placement.in_partition(n))
                    .count();

                job_entry.pending_reason = if needed > partition_size {
                    PendingReason::PartitionNodeLimit
                } else if job.spec.constraint.is_some() && eligible.is_empty() {
                    PendingReason::BadConstraints
                } else if job.spec.nodelist.as_deref().is_some_and(|s| !s.is_empty())
                    && !placement.nodelist_is_additive()
                {
                    PendingReason::ReqNodeNotAvail
                } else {
                    PendingReason::Resources
                };
                continue;
            }

            // Eligible nodes exist but none are up. is_up() keeps a busy
            // `Allocated` cluster out of NodeDown; a nodelist pin to down nodes
            // is ReqNodeNotAvail (Slurm parity).
            if eligible.iter().all(|n| !n.state.is_up()) {
                job_entry.pending_reason =
                    if job.spec.nodelist.as_deref().is_some_and(|s| !s.is_empty()) {
                        PendingReason::ReqNodeNotAvail
                    } else {
                        PendingReason::NodeDown
                    };
                continue;
            }

            // Fewer nodes free (schedulable, available resources) than needed →
            // Resources; otherwise queued behind higher priority.
            let free_now = eligible
                .iter()
                .filter(|n| placement.matches(n, cluster_state.reservations, now))
                .filter(|n| {
                    if n.alloc_resources.cpus >= n.total_resources.cpus
                        && n.total_resources.cpus > 0
                    {
                        return false;
                    }
                    n.can_satisfy_request(&required)
                })
                .count();

            job_entry.pending_reason = if free_now < needed {
                PendingReason::Resources
            } else {
                PendingReason::Priority
            };
        }
    }

    /// Send a job event notification via webhook (if configured).
    ///
    /// Uses `curl` as a subprocess to avoid pulling in an HTTP client dependency.
    fn send_notification(&self, job_id: JobId, event: &str, spec: &JobSpec) {
        let webhook_url = self.config.notifications.webhook_url.clone();
        if let Some(url) = webhook_url {
            let event = event.to_string();
            let user = spec.user.clone();
            let mail_user = spec.mail_user.clone();
            let job_name = spec.name.clone();
            tokio::spawn(async move {
                let payload = serde_json::json!({
                    "job_id": job_id,
                    "event": event,
                    "job_name": job_name,
                    "user": user,
                    "mail_user": mail_user,
                });
                let payload_str = payload.to_string();
                match tokio::process::Command::new("curl")
                    .args([
                        "-s",
                        "-X",
                        "POST",
                        "-H",
                        "Content-Type: application/json",
                        "-d",
                        &payload_str,
                        &url,
                    ])
                    .output()
                    .await
                {
                    Ok(output) => {
                        if !output.status.success() {
                            tracing::warn!(
                                job_id,
                                %event,
                                "notification webhook returned non-zero exit"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            job_id,
                            %event,
                            error = %e,
                            "failed to send notification webhook"
                        );
                    }
                }
            });
        }

        // SMTP email notification via sendmail-compatible command
        if let Some(ref smtp_cmd) = self.config.notifications.smtp_command {
            let from = self
                .config
                .notifications
                .from_address
                .as_deref()
                .unwrap_or("spur@localhost");
            let user = spec.user.clone();
            let mail_user = spec.mail_user.clone();
            let to = mail_user.as_deref().unwrap_or(&user).to_string();
            let subject = format!("Spur Job {}: {}", job_id, event);
            let body = format!("Job ID: {}\nEvent: {}\nUser: {}\n", job_id, event, user);
            let email = format!(
                "From: {}\nTo: {}\nSubject: {}\n\n{}",
                from, to, subject, body
            );

            let smtp_cmd = smtp_cmd.clone();
            tokio::spawn(async move {
                let mut child = tokio::process::Command::new("sh")
                    .args(["-c", &smtp_cmd])
                    .stdin(std::process::Stdio::piped())
                    .spawn();
                if let Ok(ref mut child) = child {
                    if let Some(ref mut stdin) = child.stdin.take() {
                        use tokio::io::AsyncWriteExt;
                        let _ = stdin.write_all(email.as_bytes()).await;
                    }
                    let _ = child.wait().await;
                }
            });
        }
    }

    pub fn set_raft(&self, raft: SpurRaft) {
        *self.raft.write() = Some(raft);
    }

    pub fn set_accounting(&self, notifier: AccountingNotifier) {
        *self.accounting.write() = Some(notifier);
    }

    pub fn set_sched_stats(&self, stats: Arc<SchedStatsCollector>) {
        let _ = self.sched_stats.set(stats);
    }

    pub(crate) fn record_sched_cycle(
        &self,
        cycle_time_us: u64,
        schedule_time_us: u64,
        jobs_started: u64,
        hit_depth_limit: bool,
    ) {
        if let Some(stats) = self.sched_stats.get() {
            stats.record_cycle(
                cycle_time_us,
                schedule_time_us,
                jobs_started,
                hit_depth_limit,
            );
        }
    }

    pub fn fairshare_cache(&self) -> &Arc<FairshareCache> {
        &self.fairshare_cache
    }

    pub fn qos_cache(&self) -> &Arc<QosCache> {
        &self.qos_cache
    }

    pub fn association_cache(&self) -> &Arc<AssociationCache> {
        &self.association_cache
    }

    /// Resolve a job's QoS from the cache; unknown/absent name → limitless default.
    pub(crate) fn resolve_qos(&self, job: &Job) -> Qos {
        match job.spec.qos.as_deref() {
            Some(name) => self.qos_cache.get(name).unwrap_or_default(),
            None => Qos::default(),
        }
    }

    /// Recompute a job's live effective priority (a running job's stored
    /// `priority` is stale). Takes `qos` pre-resolved so it can be reused,
    /// and `partitions` so callers iterating over multiple jobs don't pay
    /// for a separate lock acquisition per call.
    pub(crate) fn current_effective_priority_with_qos(
        &self,
        job: &Job,
        qos: &Qos,
        partitions: &[Partition],
    ) -> u32 {
        let now = Utc::now();
        let age_minutes = (now - job.submit_time).num_minutes().max(0);
        let partition_tier =
            spur_core::partition::max_priority_tier(job.spec.partition.as_deref(), partitions);
        let fair_share = self
            .fairshare_cache
            .get(&job.spec.user, job.spec.account.as_deref().unwrap_or(""));
        compute_effective_priority(job.priority, fair_share, age_minutes, partition_tier, qos)
    }

    /// Persist a mutation via Raft consensus. The apply callback
    /// (`StateMachineApply`) handles in-memory state on all nodes.
    fn complete_job_steps(&self, job_id: &JobId, exit_code: i32, timestamp: DateTime<Utc>) {
        let mut steps = self.steps.write();
        for step in steps.values_mut() {
            if step.job_id == *job_id && !step.state.is_terminal() {
                step.state = if exit_code == 0 {
                    StepState::Completed
                } else {
                    StepState::Failed
                };
                step.exit_code = Some(exit_code);
                step.end_time = Some(timestamp);
            }
        }
        drop(steps);
        // Licenses are not returned here: usage is derived from running jobs, so a
        // job leaving the running set frees its licenses automatically.
    }

    /// Finalize steps for all evicted jobs returned by remove_node / health check.
    pub fn complete_evicted_steps(&self, evicted: &[JobFinalized]) {
        let now = Utc::now();
        for fin in evicted {
            self.complete_job_steps(&fin.job_id, fin.exit_code, now);
        }
    }

    #[allow(clippy::result_large_err)]
    fn propose(&self, op: WalOperation) -> anyhow::Result<ClientResponse> {
        let raft = self
            .raft
            .read()
            .clone()
            .expect("raft must be set before propose is called");
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async { raft.client_write(op).await })
        })
        .map(|res| res.data)
        .map_err(|e| anyhow::anyhow!("raft propose failed: {}", e))
    }

    /// Clear a job's run-state fields so it's schedulable again after requeue.
    /// Does not bump either counter; callers do that based on why they're requeuing.
    fn clear_run_state_for_requeue(job: &mut Job) {
        job.start_time = None;
        job.exit_code = None;
        job.allocated_nodes.clear();
        job.allocated_resources = None;
        job.per_node_alloc.clear();
        job.pending_reason = PendingReason::None;
        // Stale after requeue (points at nodes the job left); next dispatch resets it.
        job.actual_stdout_path = None;
        job.actual_stderr_path = None;
    }

    /// Requeue after a dispatch failure or Timeout/NodeFail: counts against
    /// `max_batch_requeue`.
    fn reset_job_for_requeue(job: &mut Job) {
        job.requeue_count += 1;
        Self::clear_run_state_for_requeue(job);
    }

    /// Requeue after preemption: tracked separately since it isn't a failure
    /// signal and must never contribute to the `max_batch_requeue` hold.
    fn reset_job_for_preempt_requeue(job: &mut Job) {
        job.preempt_requeue_count += 1;
        Self::clear_run_state_for_requeue(job);
    }

    /// Evict a single job by ID: transition to NodeFail, then free its
    /// allocations on every node it spans. Transition is validated first
    /// so allocations are never freed for a job that can't be evicted.
    /// Nodes that already reported completion via `JobNodeComplete` (which
    /// frees a node's slice as it arrives, ahead of the whole job finishing)
    /// are skipped so their resources aren't subtracted twice.
    fn evict_job_locked(
        job_id: JobId,
        jobs: &mut HashMap<JobId, Job>,
        nodes: &mut HashMap<String, Node>,
        timestamp: chrono::DateTime<Utc>,
        reason: PendingReason,
    ) -> Option<JobFinalized> {
        let job = jobs.get_mut(&job_id)?;

        if let Some(since) = job.suspended_at.take() {
            job.suspended_secs += (timestamp - since).num_seconds().max(0);
        }
        if let Err(e) = job.transition(JobState::NodeFail) {
            warn!(job_id, error = %e, "evict: invalid transition to NodeFail");
            return None;
        }
        job.exit_code = Some(-1);
        job.end_time = Some(timestamp);
        job.pending_reason = reason;
        let already_deallocated: Vec<String> = job.node_completions.keys().cloned().collect();
        job.node_completions.clear();

        let alloc_nodes = job.allocated_nodes.clone();
        if let Some(ref total) = job.allocated_resources {
            let node_count = alloc_nodes.len().max(1) as u32;
            for alloc_node in &alloc_nodes {
                if already_deallocated.iter().any(|n| n == alloc_node) {
                    continue;
                }
                if let Some(node) = nodes.get_mut(alloc_node) {
                    let slice = job
                        .per_node_alloc
                        .get(alloc_node)
                        .cloned()
                        .unwrap_or_else(|| {
                            ResourceAllocations::with_scalar(
                                total.cpus / node_count,
                                total.memory_mb / node_count as u64,
                            )
                        });
                    node.alloc_resources.subtract(&slice);
                    node.update_state_from_alloc();
                    if node.state == NodeState::Draining
                        && node.alloc_resources.cpus == 0
                        && !node.alloc_resources.has_devices()
                    {
                        node.state = NodeState::Drain;
                    }
                }
            }
        }

        Some(JobFinalized {
            job_id,
            state: JobState::NodeFail,
            exit_code: -1,
        })
    }

    /// Fail all running/completing/suspended jobs on a node, releasing
    /// allocations on **every** node each job spans.
    fn evict_jobs_on_node(
        node_name: &str,
        jobs: &mut HashMap<JobId, Job>,
        nodes: &mut HashMap<String, Node>,
        timestamp: chrono::DateTime<Utc>,
        response: &mut ClientResponse,
    ) {
        let affected: Vec<JobId> = jobs
            .iter()
            .filter(|(_, j)| {
                matches!(
                    j.state,
                    JobState::Running | JobState::Completing | JobState::Suspended
                ) && j.allocated_nodes.iter().any(|n| n == node_name)
            })
            .map(|(&id, _)| id)
            .collect();

        for jid in affected {
            if let Some(fin) =
                Self::evict_job_locked(jid, jobs, nodes, timestamp, PendingReason::NodeDown)
            {
                response.jobs_finalized.push(fin);
            }
        }
    }

    /// Apply a WalOperation to in-memory state.
    /// Called by Raft's `apply_to_state_machine` on commit.
    fn apply_operation(&self, op: &WalOperation) -> ClientResponse {
        let mut response = ClientResponse::default();
        let mut jobs = self.jobs.write();
        let mut nodes = self.nodes.write();
        let mut next_id = self.next_job_id.load(Ordering::Relaxed);
        let timestamp = Utc::now();

        match op {
            WalOperation::JobSubmit { job_id, spec } => {
                let mut job = Job::new(*job_id, (**spec).clone());
                if let Some(het_group) = spec.het_group {
                    job.het_group = Some(het_group);
                    if het_group > 0 {
                        let anchor = jobs.values().find(|j| {
                            j.het_group == Some(0)
                                && j.spec.user == spec.user
                                && j.spec.name == spec.name
                                && j.state == JobState::Pending
                        });
                        if let Some(a) = anchor {
                            job.het_job_id = Some(a.job_id);
                        }
                    }
                }
                jobs.insert(*job_id, job);
                next_id = next_id.max(job_id + 1);
            }
            WalOperation::JobStateChange {
                job_id,
                new_state,
                pending_reason,
                pending_priority,
                ..
            } => {
                if let Some(job) = jobs.get_mut(job_id) {
                    let outcome = match job.apply_transition(*new_state) {
                        Ok(outcome) => outcome,
                        Err(e) => {
                            warn!(job_id = *job_id, error = %e, "invalid state transition in WAL apply");
                            TransitionOutcome::NoOp
                        }
                    };
                    // Gated on a real transition so a replay doesn't re-wipe
                    // fields or double-count requeue_count.
                    if outcome == TransitionOutcome::Applied && *new_state == JobState::Pending {
                        let max = self.config.controller.max_batch_requeue;
                        if job.requeue_count < max {
                            Self::reset_job_for_requeue(job);
                        } else {
                            Self::clear_run_state_for_requeue(job);
                        }
                        job.pending_reason = pending_reason.clone().unwrap_or(PendingReason::None);
                        if let Some(priority) = pending_priority {
                            job.priority = *priority;
                        }
                    }
                }
            }
            WalOperation::JobPreemptRequeue { job_id, begin_time } => {
                // Only a running job is preempted; on replay the job is already
                // Pending, so this is a NoOp (no re-dealloc, no double requeue).
                let freed_nodes;
                let allocated_resources;
                let per_node_map;
                {
                    let Some(job) = jobs.get_mut(job_id) else {
                        return ClientResponse::default();
                    };
                    if job.state != JobState::Running {
                        return ClientResponse::default();
                    }
                    // Route through Preempted so the state machine and accounting
                    // see a finished run, then requeue to Pending — one atomic
                    // apply; the intermediate Preempted never escapes the lock.
                    if let Err(e) = job.transition(JobState::Preempted) {
                        warn!(job_id = *job_id, error = %e, "invalid preempt transition in WAL apply");
                        return ClientResponse::default();
                    }
                    job.exit_code = Some(-1);
                    job.end_time = Some(timestamp);
                    if let Some(since) = job.suspended_at.take() {
                        job.suspended_secs += (timestamp - since).num_seconds().max(0);
                    }
                    freed_nodes = job.allocated_nodes.clone();
                    allocated_resources = job.allocated_resources.clone();
                    per_node_map = job.per_node_alloc.clone();
                    job.node_completions.clear();

                    if let Err(e) = job.transition(JobState::Pending) {
                        warn!(job_id = *job_id, error = %e, "invalid requeue transition in WAL apply");
                        return ClientResponse::default();
                    }
                    Self::reset_job_for_preempt_requeue(job);
                    job.spec.begin_time = Some(*begin_time);
                    job.pending_reason = PendingReason::BeginTime;
                }
                if let Some(ref total) = allocated_resources {
                    let node_count = freed_nodes.len().max(1) as u32;
                    for name in &freed_nodes {
                        if let Some(node) = nodes.get_mut(name) {
                            let slice = per_node_map.get(name).cloned().unwrap_or_else(|| {
                                warn!(job_id = *job_id, node = %name, "per_node_alloc missing at preempt deallocation, using scalar fallback");
                                ResourceAllocations::with_scalar(
                                    total.cpus / node_count,
                                    total.memory_mb / node_count as u64,
                                )
                            });
                            node.alloc_resources.subtract(&slice);
                            node.update_state_from_alloc();
                            if node.state == NodeState::Draining
                                && node.alloc_resources.cpus == 0
                                && !node.alloc_resources.has_devices()
                            {
                                node.state = NodeState::Drain;
                            }
                        }
                    }
                }
                drop(jobs);
                drop(nodes);
                // Complete steps and fire accounting for the terminated run as
                // PREEMPTED, even though the job itself is now Pending-with-hold.
                self.complete_job_steps(job_id, -1, timestamp);
                self.next_job_id.store(next_id, Ordering::Relaxed);
                return ClientResponse {
                    jobs_finalized: vec![JobFinalized {
                        job_id: *job_id,
                        state: JobState::Preempted,
                        exit_code: -1,
                    }],
                    ..Default::default()
                };
            }
            WalOperation::JobSuspend { job_id, at } => {
                if let Some(job) = jobs.get_mut(job_id) {
                    match job.apply_transition(JobState::Suspended) {
                        Ok(TransitionOutcome::Applied) => job.suspended_at = Some(*at),
                        Ok(TransitionOutcome::NoOp) => {}
                        Err(e) => {
                            warn!(job_id = *job_id, error = %e, "invalid suspend transition in WAL apply")
                        }
                    }
                }
            }
            WalOperation::JobResume { job_id, at } => {
                if let Some(job) = jobs.get_mut(job_id) {
                    match job.apply_transition(JobState::Running) {
                        Ok(TransitionOutcome::Applied) => {
                            if let Some(since) = job.suspended_at.take() {
                                job.suspended_secs += (*at - since).num_seconds().max(0);
                            }
                        }
                        Ok(TransitionOutcome::NoOp) => {}
                        Err(e) => {
                            warn!(job_id = *job_id, error = %e, "invalid resume transition in WAL apply")
                        }
                    }
                }
            }
            WalOperation::JobEvict { job_id } => {
                if let Some(fin) = Self::evict_job_locked(
                    *job_id,
                    &mut jobs,
                    &mut nodes,
                    timestamp,
                    PendingReason::JobLaunchFailure,
                ) {
                    response.jobs_finalized.push(fin);
                }
            }
            WalOperation::JobStart {
                job_id,
                nodes: node_names,
                resources,
                per_node_alloc,
                srun_step_dispatch,
                run_attempt,
            } => {
                if let Some(job) = jobs.get_mut(job_id) {
                    job.start_time = Some(timestamp);
                    job.allocated_nodes = node_names.clone();
                    job.allocated_resources = Some(resources.clone());
                    job.per_node_alloc = per_node_alloc.clone();
                    job.pending_reason = PendingReason::None;
                    job.srun_step_dispatch = *srun_step_dispatch;
                    job.run_attempt = *run_attempt;
                }
                let node_count = node_names.len().max(1) as u32;
                for name in node_names {
                    if let Some(node) = nodes.get_mut(name) {
                        let slice = per_node_alloc.get(name).cloned().unwrap_or_else(|| {
                            warn!(job_id = *job_id, node = %name, "per_node_alloc missing at allocation, using scalar fallback");
                            ResourceAllocations::with_scalar(
                                resources.cpus / node_count,
                                resources.memory_mb / node_count as u64,
                            )
                        });
                        node.alloc_resources.add(&slice);
                        node.update_state_from_alloc();
                    }
                }
                // Licenses are not mutated here: usage is derived on demand from
                // running jobs (see available_licenses()), so the config total is
                // authoritative and cannot drift.
            }
            WalOperation::JobNodeComplete {
                job_id,
                node_name,
                exit_code,
                signal,
            } => {
                let finalized = {
                    let Some(job) = jobs.get_mut(job_id) else {
                        return ClientResponse::default();
                    };
                    // A completion for a non-active job is stale/replayed; skip
                    // it rather than forcing an illegal finalize transition.
                    if !job.state.is_active() {
                        return ClientResponse::default();
                    }

                    let already_reported = job.node_completions.contains_key(node_name);
                    job.node_completions.insert(
                        node_name.clone(),
                        spur_core::job::NodeCompletion {
                            code: *exit_code,
                            signal: *signal,
                        },
                    );

                    if let Some(ref total) = job.allocated_resources {
                        if !already_reported {
                            let node_count = job.allocated_nodes.len().max(1) as u32;
                            if let Some(node) = nodes.get_mut(node_name) {
                                let slice = job.per_node_alloc.get(node_name).cloned().unwrap_or_else(|| {
                                    warn!(job_id = *job_id, node = %node_name, "per_node_alloc missing at node deallocation, using scalar fallback");
                                    ResourceAllocations::with_scalar(
                                        total.cpus / node_count,
                                        total.memory_mb / node_count as u64,
                                    )
                                });
                                node.alloc_resources.subtract(&slice);
                                node.update_state_from_alloc();
                                if node.state == NodeState::Draining
                                    && node.alloc_resources.cpus == 0
                                    && !node.alloc_resources.has_devices()
                                {
                                    node.state = NodeState::Drain;
                                }
                            }
                        }
                    }

                    // Suspended jobs route through Completing too, so an
                    // out-of-band task death finalizes instead of stranding.
                    if matches!(job.state, JobState::Running | JobState::Suspended) {
                        if let Err(e) = job.transition(JobState::Completing) {
                            warn!(job_id = *job_id, error = %e, "invalid transition to Completing");
                        }
                        job.end_time = Some(timestamp);
                    }

                    if job.all_nodes_completed() {
                        // Primary = batch node (allocated_nodes[0]); empty when
                        // none allocated, where derived_completion falls back to
                        // the worst completion.
                        let primary = job.allocated_nodes.first().cloned().unwrap_or_default();
                        // spurd flags an OOM kill via a sentinel bit in the signal;
                        // detect it, then strip the bit so the stored signal is the
                        // real SIGKILL and the job reports OUT_OF_MEMORY.
                        let oom = job
                            .node_completions
                            .values()
                            .any(|c| c.signal & spur_core::job::OOM_SIGNAL_FLAG != 0);
                        let (derived_state, final_exit, raw_signal) =
                            Job::derived_completion(&job.node_completions, &primary);
                        let final_signal = raw_signal & !spur_core::job::OOM_SIGNAL_FLAG;
                        let final_state = if oom {
                            JobState::OutOfMemory
                        } else {
                            derived_state
                        };
                        match job.transition(final_state) {
                            Ok(()) => {
                                job.exit_code = Some(final_exit);
                                job.exit_signal = final_signal;
                                // DerivedExitCode is the running max over srun
                                // steps, accumulated live by JobStepComplete; a
                                // job with no srun steps keeps 0 (Slurm parity),
                                // not the batch exit. Left as-is here.
                                job.pending_reason = if oom {
                                    PendingReason::OutOfMemory
                                } else if final_signal != 0 {
                                    PendingReason::RaisedSignal
                                } else if final_exit != 0 {
                                    PendingReason::NonZeroExitCode
                                } else {
                                    PendingReason::None
                                };
                                job.end_time = Some(timestamp);
                                job.node_completions.clear();
                                Some((final_state, final_exit))
                            }
                            Err(e) => {
                                warn!(
                                    job_id = *job_id,
                                    error = %e,
                                    "invalid final completion transition"
                                );
                                None
                            }
                        }
                    } else {
                        None
                    }
                };

                if let Some((final_state, final_exit)) = finalized {
                    drop(jobs);
                    drop(nodes);
                    self.complete_job_steps(job_id, final_exit, timestamp);
                    self.next_job_id.store(next_id, Ordering::Relaxed);
                    return ClientResponse {
                        jobs_finalized: vec![JobFinalized {
                            job_id: *job_id,
                            state: final_state,
                            exit_code: final_exit,
                        }],
                        ..Default::default()
                    };
                }
            }
            WalOperation::JobComplete {
                job_id,
                exit_code,
                state,
            } => {
                let freed_nodes;
                let allocated_resources;
                let already_deallocated;
                if let Some(job) = jobs.get_mut(job_id) {
                    // is_finalized (incl. Preempted): a stale/replayed complete
                    // is a silent no-op, not a rejected-transition warning.
                    // Preempted is finalized for the ended run but may still cancel.
                    if job.state.is_finalized()
                        && !(job.state == JobState::Preempted && *state == JobState::Cancelled)
                    {
                        return ClientResponse::default();
                    }
                    if let Err(e) = job.transition(*state) {
                        warn!(
                            job_id = *job_id,
                            error = %e,
                            "invalid state transition in WAL apply"
                        );
                        return ClientResponse::default();
                    }
                    if state.is_terminal() {
                        response.jobs_finalized.push(JobFinalized {
                            job_id: *job_id,
                            state: *state,
                            exit_code: *exit_code,
                        });
                    }
                    job.exit_code = Some(*exit_code);
                    job.end_time = Some(timestamp);
                    // Suspended -> terminal: fold the final suspended interval in
                    // and clear suspended_at so it never lingers on a terminal job.
                    if let Some(since) = job.suspended_at.take() {
                        job.suspended_secs += (timestamp - since).num_seconds().max(0);
                    }
                    freed_nodes = job.allocated_nodes.clone();
                    allocated_resources = job.allocated_resources.clone();
                    already_deallocated = job.node_completions.keys().cloned().collect::<Vec<_>>();
                    job.node_completions.clear();
                } else {
                    return ClientResponse::default();
                }
                // Deallocate node resources not already freed during COMPLETING
                let per_node_map = jobs
                    .get(job_id)
                    .map(|j| j.per_node_alloc.clone())
                    .unwrap_or_default();
                if let Some(ref total) = allocated_resources {
                    let node_count = freed_nodes.len().max(1) as u32;
                    for name in &freed_nodes {
                        if already_deallocated.iter().any(|n| n == name) {
                            continue;
                        }
                        if let Some(node) = nodes.get_mut(name) {
                            let slice = per_node_map.get(name).cloned().unwrap_or_else(|| {
                                warn!(job_id = *job_id, node = %name, "per_node_alloc missing at deallocation, using scalar fallback");
                                ResourceAllocations::with_scalar(
                                    total.cpus / node_count,
                                    total.memory_mb / node_count as u64,
                                )
                            });
                            node.alloc_resources.subtract(&slice);
                            node.update_state_from_alloc();
                            if node.state == NodeState::Draining
                                && node.alloc_resources.cpus == 0
                                && !node.alloc_resources.has_devices()
                            {
                                node.state = NodeState::Drain;
                            }
                        }
                    }
                }
                drop(jobs);
                drop(nodes);
                self.complete_job_steps(job_id, *exit_code, timestamp);
            }
            WalOperation::JobStepComplete {
                job_id,
                step_id,
                exit_code,
            } => {
                // Record the step's own exit code/state.
                {
                    let mut steps = self.steps.write();
                    if let Some(step) = steps.get_mut(&(*job_id, *step_id)) {
                        step.state = if *exit_code == 0 {
                            StepState::Completed
                        } else {
                            StepState::Failed
                        };
                        step.exit_code = Some(*exit_code);
                        step.end_time = Some(timestamp);
                    }
                }
                // DerivedExitCode is the running max over srun steps (the batch
                // step is excluded — it carries the job's own exit, not a step
                // result). Maintained live so `scontrol show job` reflects it
                // mid-run, matching Slurm.
                if *step_id < STEP_RESERVED_MIN {
                    if let Some(job) = jobs.get_mut(job_id) {
                        job.derived_exit_code = job.derived_exit_code.max(*exit_code);
                    }
                }
            }
            WalOperation::JobStepCreate { step } => match jobs.get(&step.job_id) {
                None => {
                    warn!(
                        job_id = step.job_id,
                        step_id = step.step_id,
                        "JobStepCreate: skipping step for unknown job"
                    );
                }
                Some(job) if job.state.is_terminal() => {
                    warn!(
                        job_id = step.job_id,
                        step_id = step.step_id,
                        state = ?job.state,
                        "JobStepCreate: skipping step for terminal job"
                    );
                }
                Some(_) => {
                    let mut steps = self.steps.write();
                    steps
                        .entry((step.job_id, step.step_id))
                        .or_insert_with(|| (**step).clone());
                }
            },
            WalOperation::JobPriorityChange {
                job_id,
                new_priority,
                pending_reason,
                reset_requeue_count,
                clear_reservation,
                ..
            } => {
                if let Some(job) = jobs.get_mut(job_id) {
                    job.priority = *new_priority;
                    if let Some(reason) = pending_reason {
                        job.pending_reason = reason.clone();
                    }
                    if *reset_requeue_count {
                        job.requeue_count = 0;
                    }
                    if *clear_reservation {
                        job.spec.reservation = None;
                    }
                }
            }
            WalOperation::NodeRegister {
                name,
                resources,
                address,
                port,
                wg_pubkey,
                version,
                labels,
            } => {
                let mut node = Node::new(name.clone(), resources.clone());
                node.address = Some(address.clone());
                node.port = *port;
                node.labels = labels.clone();
                if !wg_pubkey.is_empty() {
                    node.wg_pubkey = Some(wg_pubkey.clone());
                }
                if !version.is_empty() {
                    node.version = Some(version.clone());
                }
                node.last_heartbeat = Some(Utc::now());
                node.state = node
                    .state
                    .transition(&NodeEvent::Register, false)
                    .unwrap_or(NodeState::Idle);

                // Assign partitions: match by hostlist OR label selector (union)
                drop(nodes);
                let partitions = self.partitions.read();
                for part in partitions.iter() {
                    if partition_matches_node(part, name, labels) {
                        node.partitions.push(part.name.clone());
                    }
                }
                if node.partitions.is_empty() {
                    if let Some(dp) = partitions.iter().find(|p| p.is_default) {
                        node.partitions.push(dp.name.clone());
                    } else if let Some(first) = partitions.first() {
                        node.partitions.push(first.name.clone());
                    }
                }
                drop(partitions);

                self.apply_node_config_policy(&mut node);

                let mut nodes = self.nodes.write();
                nodes.insert(name.clone(), node);
                self.next_job_id.store(next_id, Ordering::Relaxed);
                return ClientResponse::default();
            }
            WalOperation::NodeUpdate {
                name,
                resources,
                address,
                port,
                wg_pubkey,
                version,
            } => {
                if let Some(node) = nodes.get_mut(name) {
                    node.total_resources = resources.clone();
                    node.address = Some(address.clone());
                    node.port = *port;
                    if !wg_pubkey.is_empty() {
                        node.wg_pubkey = Some(wg_pubkey.clone());
                    }
                    if !version.is_empty() {
                        node.version = Some(version.clone());
                    }
                    node.last_heartbeat = Some(Utc::now());
                }
            }
            WalOperation::NodeStateChange {
                name,
                new_state,
                reason,
                admin_locked,
                ..
            } => {
                if let Some(node) = nodes.get_mut(name) {
                    node.state = *new_state;
                    node.state_reason = reason.clone();
                    node.admin_locked = *admin_locked;
                }
                if *new_state == NodeState::Down {
                    Self::evict_jobs_on_node(name, &mut jobs, &mut nodes, timestamp, &mut response);
                }
            }
            WalOperation::NodeLabelsUpdate { name, set, remove } => {
                if let Some(node) = nodes.get_mut(name) {
                    for (k, v) in set {
                        node.labels.insert(k.clone(), v.clone());
                    }
                    for k in remove {
                        node.labels.remove(k);
                    }
                    // Re-evaluate partition membership after label change
                    let partitions = self.partitions.read();
                    let mut matched = Vec::new();
                    for part in partitions.iter() {
                        if partition_matches_node(part, &node.name, &node.labels) {
                            matched.push(part.name.clone());
                        }
                    }
                    if matched.is_empty() {
                        if let Some(dp) = partitions.iter().find(|p| p.is_default) {
                            matched.push(dp.name.clone());
                        } else if let Some(first) = partitions.first() {
                            matched.push(first.name.clone());
                        }
                    }
                    node.partitions = matched;

                    self.apply_node_config_policy(node);
                }
            }
            WalOperation::NodeRemove { name, reason } => {
                Self::evict_jobs_on_node(name, &mut jobs, &mut nodes, timestamp, &mut response);
                if let Some(node) = nodes.get(name) {
                    if node.alloc_resources.cpus > 0 || node.alloc_resources.has_devices() {
                        warn!(
                            node = %name,
                            reason = reason.as_deref().unwrap_or(""),
                            "removing node with nonzero allocations"
                        );
                    }
                }
                nodes.remove(name);
                info!(
                    node = %name,
                    reason = reason.as_deref().unwrap_or(""),
                    "node removed from cluster"
                );
            }
            WalOperation::TokenCreate { token } => {
                self.tokens.write().insert(token.id.clone(), token.clone());
            }
            WalOperation::TokenRevoke { token_id } => {
                if let Some(t) = self.tokens.write().get_mut(token_id) {
                    t.revoked = true;
                }
            }
            WalOperation::ReservationCreate { reservation } => {
                let mut reservations = self.reservations.write();
                if reservations.iter().any(|r| r.name == reservation.name) {
                    warn!(
                        name = %reservation.name,
                        "duplicate reservation create in WAL apply, ignoring"
                    );
                } else {
                    reservations.push(reservation.clone());
                    response.reservation_created = true;
                    info!(name = %reservation.name, "reservation created");
                }
            }
            WalOperation::ReservationUpdate {
                name,
                duration_minutes,
                add_nodes,
                remove_nodes,
                add_users,
                remove_users,
                add_accounts,
                remove_accounts,
            } => {
                let mut reservations = self.reservations.write();
                Self::apply_reservation_update_locked(
                    reservations.as_mut(),
                    name,
                    *duration_minutes,
                    add_nodes,
                    remove_nodes,
                    add_users,
                    remove_users,
                    add_accounts,
                    remove_accounts,
                );
                info!(name, "reservation updated");
            }
            WalOperation::ReservationDelete { name } => {
                let deleted = {
                    let reservations = self.reservations.read();
                    reservations.iter().find(|r| r.name == *name).cloned()
                };
                let mut reservations = self.reservations.write();
                let len_before = reservations.len();
                reservations.retain(|r| r.name != *name);
                if reservations.len() < len_before {
                    if let Some(res) = deleted {
                        if res.flags.no_hold_jobs {
                            Self::detach_jobs_from_deleted_reservation_jobs(&mut jobs, name);
                        } else {
                            Self::hold_jobs_for_deleted_reservation_jobs(&mut jobs, name);
                        }
                        info!(name, "reservation deleted");
                    }
                }
            }

            // Native k0s cluster operations. All idempotent/replay-safe: NodeK0sAssign is
            // keyed by node name, token insert/revoke are keyed by id, phase is a last-write set.
            WalOperation::NodeK0sAssign {
                name,
                role,
                mesh_ip,
                pod_cidr,
            } => {
                // Reuses the `nodes` write guard from the top of this fn.
                if let Some(node) = nodes.get_mut(name) {
                    node.k0s_role = Some(*role);
                    node.k0s_mesh_ip = Some(mesh_ip.clone());
                    node.k0s_pod_cidr = Some(pod_cidr.clone());
                }
            }
            WalOperation::K0sSetPhase {
                phase,
                control_plane_node,
                reset_requested,
            } => {
                let mut k0s = self.k0s.write();
                k0s.phase = *phase;
                if control_plane_node.is_some() {
                    k0s.control_plane_node = control_plane_node.clone();
                }
                k0s.reset_requested = *reset_requested;
            }
        }
        self.next_job_id.store(next_id, Ordering::Relaxed);
        response
    }
}

/// Snapshot data for Raft serialization.
/// Must include all durable cluster state so a follower can fully restore from it.
#[derive(serde::Serialize, serde::Deserialize)]
struct ClusterSnapshot {
    jobs: Vec<Job>,
    nodes: Vec<Node>,
    reservations: Vec<Reservation>,
    steps: Vec<JobStep>,
    license_pool: HashMap<String, u64>,
    #[serde(default)]
    tokens: Vec<spur_core::admission::AdmissionToken>,
    /// Configured BB total (immutable; serialized for observability but, like
    /// `license_pool`, NOT restored — config stays authoritative). Per-job
    /// staging phase rides along on each `Job`.
    #[serde(default)]
    burst_buffer_total_gb: u64,
    /// cluster-wide k0s state (phase, control-plane node, join-token metadata). Unlike
    /// license_pool/burst_buffer this is runtime-authoritative allocated state and MUST be
    /// restored (see restore_from_snapshot).
    #[serde(default)]
    k0s: spur_core::k0s::K0sClusterState,
}

impl ClusterManager {
    /// Apply features/weight from the first matching NodeConfig, reverting to
    /// node defaults when none matches so stale policy from a previously matching
    /// entry does not persist.
    fn apply_node_config_policy(&self, node: &mut Node) {
        for nc in &self.config.nodes {
            if node_config_matches(nc, &node.name, &node.labels) {
                node.features = nc.features.clone();
                node.weight = nc.weight;
                return;
            }
        }
        node.reset_config_policy();
    }

    /// Re-evaluate partition membership and NodeConfig policy (features, weight)
    /// for all nodes against the current config. Called after snapshot restore to
    /// handle config changes that occurred between snapshot creation and restart.
    fn reconcile_partitions(&self, nodes: &mut HashMap<String, Node>) {
        let partitions = self.partitions.read();
        for node in nodes.values_mut() {
            let mut matched = Vec::new();
            for part in partitions.iter() {
                if partition_matches_node(part, &node.name, &node.labels) {
                    matched.push(part.name.clone());
                }
            }
            if matched.is_empty() {
                if let Some(dp) = partitions.iter().find(|p| p.is_default) {
                    matched.push(dp.name.clone());
                } else if let Some(first) = partitions.first() {
                    matched.push(first.name.clone());
                }
            }
            node.partitions = matched;

            self.apply_node_config_policy(node);
        }
    }
}

impl StateMachineApply for ClusterManager {
    fn apply_operation(&self, op: &WalOperation) -> ClientResponse {
        self.apply_operation(op)
    }

    fn snapshot_state(&self) -> Result<Vec<u8>, anyhow::Error> {
        let snap = ClusterSnapshot {
            jobs: self.jobs.read().values().cloned().collect(),
            nodes: self.nodes.read().values().cloned().collect(),
            reservations: self.reservations.read().clone(),
            steps: self.steps.read().values().cloned().collect(),
            license_pool: self.license_pool.read().clone(),
            tokens: self.tokens.read().values().cloned().collect(),
            burst_buffer_total_gb: *self.burst_buffer_total_gb.read(),
            k0s: self.k0s.read().clone(),
        };
        serde_json::to_vec(&snap).map_err(Into::into)
    }

    fn restore_from_snapshot(&self, data: &[u8]) -> Result<(), anyhow::Error> {
        let snap = serde_json::from_slice::<ClusterSnapshot>(data)?;

        let mut next_id = self.config.controller.first_job_id;
        let mut jobs = self.jobs.write();
        jobs.clear();
        for job in snap.jobs {
            next_id = next_id.max(job.job_id + 1);
            jobs.insert(job.job_id, job);
        }

        let mut nodes = self.nodes.write();
        nodes.clear();
        for node in snap.nodes {
            nodes.insert(node.name.clone(), node);
        }

        *self.reservations.write() = snap.reservations;

        let mut steps = self.steps.write();
        steps.clear();
        for step in snap.steps {
            steps.insert((step.job_id, step.step_id), step);
        }

        // license_pool is the configured total (immutable); it is intentionally
        // NOT restored from the snapshot so config stays authoritative and any
        // historical drift in old snapshots is discarded. Availability is
        // derived from the restored jobs. burst_buffer_total_gb follows the
        // same rule; per-job BB staging phase rides along on each restored Job.

        let mut tokens = self.tokens.write();
        tokens.clear();
        for token in snap.tokens {
            tokens.insert(token.id.clone(), token);
        }

        // k0s phase + join-token metadata are runtime-authoritative allocated state
        // (NOT config-derived like license_pool/burst_buffer) — restore them.
        *self.k0s.write() = snap.k0s;

        self.next_job_id.store(next_id, Ordering::Relaxed);

        // Re-evaluate partition membership and NodeConfig policy
        // for all nodes against the current config.
        self.reconcile_partitions(&mut nodes);

        info!(
            jobs = jobs.len(),
            nodes = nodes.len(),
            "restored cluster state from Raft snapshot"
        );
        Ok(())
    }
}

fn job_candidate_node_names(job: &Job, nodes: &[spur_core::node::Node]) -> Vec<String> {
    let placement = spur_sched::node_match::NodePlacement::new(job);
    let required = spur_sched::backfill::job_resource_request(job);

    nodes
        .iter()
        .filter(|node| {
            placement.allows_name(&node.name)
                && placement.in_partition(node)
                && placement.has_features(node)
                && node.total_resources.can_satisfy(&required)
        })
        .map(|n| n.name.clone())
        .collect()
}

fn reservation_fence_reason(
    job: &Job,
    cluster_state: &spur_sched::traits::ClusterState,
) -> Option<PendingReason> {
    let placement = spur_sched::node_match::NodePlacement::new(job);
    let candidates = job_candidate_node_names(job, cluster_state.nodes);
    if candidates.is_empty() {
        return None;
    }

    let now = Utc::now();
    let duration = job.spec.time_limit.unwrap_or(chrono::Duration::hours(1));
    let mut maint_block = false;
    let mut blocked = 0;
    let mut listed_blocked = false;
    let mut unblocked = 0;

    for node_name in &candidates {
        let mut node_blocked = false;
        for res in cluster_state.reservations {
            if reservation::prospective_overlap(job, res, node_name, now, duration) {
                node_blocked = true;
                if res.flags.maint {
                    maint_block = true;
                }
            }
        }
        if node_blocked {
            blocked += 1;
            listed_blocked |= placement.nodelist_is_additive() && placement.is_listed(node_name);
        } else {
            unblocked += 1;
        }
    }

    let needed = (job.spec.num_nodes as usize).max(1);
    if blocked == 0 || (!listed_blocked && unblocked >= needed) {
        return None;
    }
    if maint_block {
        Some(PendingReason::ReservedMaintenance)
    } else {
        Some(PendingReason::ReqNodeNotAvail)
    }
}

/// `Reservation` if the job's `--reservation` is absent/inactive/expired or
/// denies it, else `None`. Shared by `pending_jobs()` (drop) and
/// `tag_blocked_pending_reasons()` (displayed reason) so the two agree.
fn reservation_block(
    job: &Job,
    reservations: &[Reservation],
    now: chrono::DateTime<Utc>,
) -> Option<spur_core::job::PendingReason> {
    use spur_core::job::PendingReason;
    let res_name = job.spec.reservation.as_ref()?;
    if res_name.is_empty() {
        return None;
    }
    match reservations.iter().find(|r| r.name == *res_name) {
        Some(r)
            if r.is_active(now) && r.allows_user(&job.spec.user, job.spec.account.as_deref()) =>
        {
            None
        }
        _ => Some(PendingReason::Reservation),
    }
}

/// QoS is added on top of the fairshare/age/tier product rather than fed
/// into it, so it's a constant offset instead of an amplified/diluted one.
fn compute_effective_priority(
    base_priority: u32,
    fair_share: f64,
    age_minutes: i64,
    partition_tier: u32,
    qos: &Qos,
) -> u32 {
    let raw = spur_sched::priority::effective_priority(
        base_priority,
        fair_share,
        age_minutes,
        partition_tier,
    );
    qos_adjusted_priority(raw, qos)
}

/// Reason a job is ineligible because currently-available licenses cannot satisfy
/// its `license:` GRES requests, or `None`. Reported as `Licenses`. `available`
/// is the configured total minus licenses held by active jobs.
fn license_block(job: &Job, pool: &HashMap<String, u64>) -> Option<spur_core::job::PendingReason> {
    use spur_core::job::PendingReason;
    let lic_req = extract_license_requirements(&job.spec);
    for (lic, count) in &lic_req {
        if pool.get(lic).copied().unwrap_or(0) < *count {
            return Some(PendingReason::Licenses);
        }
    }
    None
}

/// Shared by `pending_jobs()` and `tag_blocked_pending_reasons()` so the drop
/// decision and displayed reason always agree. Caller resolves the `Qos`.
fn qos_block_for(
    job: &Job,
    qos: &Qos,
    jobs: &HashMap<JobId, Job>,
) -> Option<spur_core::job::PendingReason> {
    qos_block_with(job, qos, jobs, &PassReservations::default())
}

/// Like [`qos_block_for`] but folds `reserved` (headroom already claimed by
/// higher-priority jobs earlier in the same scheduling pass) into the running
/// aggregates, so a single pass can't over-subscribe a QOS group/per-user cap.
fn qos_block_with(
    job: &Job,
    qos: &Qos,
    jobs: &HashMap<JobId, Job>,
    reserved: &PassReservations,
) -> Option<spur_core::job::PendingReason> {
    let qos_name = job.spec.qos.as_ref()?;
    let user = &job.spec.user;
    let mut running_count = jobs
        .values()
        .filter(|j| {
            j.state == JobState::Running
                && j.spec.user == *user
                && j.spec.qos.as_deref() == Some(qos_name.as_str())
        })
        .count() as u32;
    // Count only earlier-submitted jobs (lower job_id) so a later job never
    // makes an earlier, within-limit job retroactively blocked.
    let submitted_count = jobs
        .values()
        .filter(|j| {
            j.job_id < job.job_id
                && (j.state == JobState::Pending || j.state == JobState::Running)
                && j.spec.user == *user
                && j.spec.qos.as_deref() == Some(qos_name.as_str())
        })
        .count() as u32;
    let mut user_running_tres = sum_running_tres(jobs, |j| {
        j.spec.user == *user && j.spec.qos.as_deref() == Some(qos_name.as_str())
    });
    let mut qos_running_tres =
        sum_running_tres(jobs, |j| j.spec.qos.as_deref() == Some(qos_name.as_str()));

    let user_key = (user.clone(), qos_name.clone());
    running_count += reserved.qos_user_count.get(&user_key).copied().unwrap_or(0);
    if let Some(t) = reserved.qos_user_tres.get(&user_key) {
        user_running_tres.add(t);
    }
    if let Some(t) = reserved.qos_grp.get(qos_name) {
        qos_running_tres.add(t);
    }

    match check_qos_limits(
        job,
        qos,
        running_count,
        submitted_count,
        &user_running_tres,
        &qos_running_tres,
    ) {
        QosCheckResult::Allowed => None,
        QosCheckResult::Blocked(reason) => Some(reason),
    }
}

/// Shared by `pending_jobs()` and `tag_blocked_pending_reasons()` so the drop
/// decision and displayed reason always agree. Looks up the job's (user,
/// account) association limits from `AssociationCache`; a job with no account
/// (or an association the cache has no limits for) is unconstrained.
fn account_block_for(
    job: &Job,
    assoc_cache: &AssociationCache,
    jobs: &HashMap<JobId, Job>,
) -> Option<spur_core::job::PendingReason> {
    account_block_with(job, assoc_cache, jobs, &PassReservations::default())
}

/// Like [`account_block_for`] but folds `reserved` (headroom already claimed by
/// higher-priority jobs earlier in the same scheduling pass) into the running
/// aggregates, so a single pass can't over-subscribe an account group cap.
fn account_block_with(
    job: &Job,
    assoc_cache: &AssociationCache,
    jobs: &HashMap<JobId, Job>,
    reserved: &PassReservations,
) -> Option<spur_core::job::PendingReason> {
    let account = job.spec.account.as_deref().filter(|a| !a.is_empty())?;
    let user = &job.spec.user;
    let limits = assoc_cache.limits(user, account);

    let mut running_count = jobs
        .values()
        .filter(|j| {
            j.state == JobState::Running
                && j.spec.user == *user
                && j.spec.account.as_deref() == Some(account)
        })
        .count() as u32;
    // Count only earlier-submitted jobs (lower job_id) so a later job never
    // makes an earlier, within-limit job retroactively blocked.
    let submitted_count = jobs
        .values()
        .filter(|j| {
            j.job_id < job.job_id
                && (j.state == JobState::Pending || j.state == JobState::Running)
                && j.spec.user == *user
                && j.spec.account.as_deref() == Some(account)
        })
        .count() as u32;
    let mut account_running_tres =
        sum_running_tres(jobs, |j| j.spec.account.as_deref() == Some(account));

    running_count += reserved
        .account_user_count
        .get(&(user.clone(), account.to_string()))
        .copied()
        .unwrap_or(0);
    if let Some(t) = reserved.account_grp.get(account) {
        account_running_tres.add(t);
    }

    match check_account_limits(
        job,
        &limits,
        running_count,
        submitted_count,
        &account_running_tres,
    ) {
        AccountCheckResult::Allowed => None,
        AccountCheckResult::Blocked(reason) => Some(reason),
    }
}

/// For a partition OR-list, returns `None` when any requested partition is Up
/// and permits the request. Unknown names and limits rejected by every Up
/// alternative return `PartitionConfig`; all-inactive alternatives return
/// `PartitionInactive`.
fn partition_block(job: &Job, partitions: &[Partition]) -> Option<spur_core::job::PendingReason> {
    use spur_core::job::PendingReason;
    use spur_core::partition::PartitionState;

    let partition_spec = job
        .spec
        .partition
        .as_deref()
        .filter(|spec| !spec.is_empty())?;
    let requested = requested_partition_names(Some(partition_spec))
        .map(|name| partitions.iter().find(|part| part.name == name))
        .collect::<Option<Vec<_>>>();
    let Some(requested) = requested else {
        return Some(PendingReason::PartitionConfig);
    };
    if requested.is_empty() {
        return Some(PendingReason::PartitionConfig);
    }

    let mut has_up_partition = false;
    for part in requested {
        if part.state != PartitionState::Up {
            continue;
        }
        has_up_partition = true;
        if partition_limits_allow(job, part) {
            return None;
        }
    }

    Some(if has_up_partition {
        PendingReason::PartitionConfig
    } else {
        PendingReason::PartitionInactive
    })
}

fn partition_limits_allow(job: &Job, part: &Partition) -> bool {
    if let Some(max) = part.max_nodes {
        if job.spec.num_nodes > max {
            return false;
        }
    }
    if part.min_nodes > 0 && job.spec.num_nodes < part.min_nodes {
        return false;
    }
    if let (Some(max_mins), Some(tl)) = (part.max_time_minutes, &job.spec.time_limit) {
        if tl.num_minutes() > i64::from(max_mins) {
            return false;
        }
    }
    true
}

fn sum_running_tres(jobs: &HashMap<JobId, Job>, pred: impl Fn(&Job) -> bool) -> TresRecord {
    let mut tres = TresRecord::new();
    for j in jobs.values() {
        if j.state != JobState::Running || !pred(j) {
            continue;
        }
        tres.add(&job_tres(j));
    }
    tres
}

/// The TRES a single job occupies (its scheduling footprint). Shared by the
/// running-usage sum and the same-pass reservation accounting so both count a
/// job's CPU/node/mem/GPU footprint identically.
fn job_tres(job: &Job) -> TresRecord {
    let mut tres = TresRecord::new();
    tres.set(
        TresType::Cpu,
        (job.spec.num_tasks * job.spec.cpus_per_task) as u64,
    );
    tres.set(TresType::Node, job.spec.num_nodes as u64);
    tres.set(
        TresType::Memory,
        effective_memory_mb(&job.spec, job.spec.num_nodes),
    );
    tres.set(TresType::Gpu, effective_gpus(&job.spec, job.spec.num_nodes));
    tres
}

/// Headroom claimed by jobs already kept earlier in a single `pending_jobs()`
/// pass, folded into the running aggregates so aggregate QOS/account caps
/// (`grp_tres`, per-user TRES, per-user running-job counts) can't be
/// over-subscribed within one pass — the same guarantee licenses and
/// burst-buffer capacity already get via priority-ordered reservation.
#[derive(Default)]
struct PassReservations {
    qos_grp: HashMap<String, TresRecord>,
    qos_user_tres: HashMap<(String, String), TresRecord>,
    qos_user_count: HashMap<(String, String), u32>,
    account_grp: HashMap<String, TresRecord>,
    account_user_count: HashMap<(String, String), u32>,
}

impl PassReservations {
    /// Record that `job` (kept this pass) now occupies its footprint against the
    /// QOS and account aggregates, so lower-priority jobs later in the pass see
    /// the reduced headroom.
    fn reserve(&mut self, job: &Job) {
        let tres = job_tres(job);
        let user = job.spec.user.clone();
        if let Some(qos) = job.spec.qos.as_ref().filter(|q| !q.is_empty()) {
            self.qos_grp.entry(qos.clone()).or_default().add(&tres);
            let key = (user.clone(), qos.clone());
            self.qos_user_tres
                .entry(key.clone())
                .or_default()
                .add(&tres);
            *self.qos_user_count.entry(key).or_insert(0) += 1;
        }
        if let Some(account) = job.spec.account.as_deref().filter(|a| !a.is_empty()) {
            let account = account.to_string();
            self.account_grp
                .entry(account.clone())
                .or_default()
                .add(&tres);
            *self.account_user_count.entry((user, account)).or_insert(0) += 1;
        }
    }
}

/// Burst-buffer capacity (GB) a job's `--bb` string reserves cluster-wide.
/// Shares the grammar with the agent's stage wrapper via `spur_core`.
fn extract_bb_requirement(spec: &JobSpec) -> u64 {
    spec.burst_buffer
        .as_deref()
        .map(spur_core::burst_buffer::parse_capacity_gb)
        .unwrap_or(0)
}

/// `BurstBufferResources` if the job needs more BB capacity than is currently
/// free, else `None`. Reported when an absolute shortage means the job can
/// never stage in the current cluster state. `free_gb` is the configured total
/// minus capacity reserved by staging/active jobs. Shared by `pending_jobs()`
/// (drop) and `tag_blocked_pending_reasons()` (displayed reason) so they agree.
fn burst_buffer_block(job: &Job, free_gb: u64) -> Option<spur_core::job::PendingReason> {
    use spur_core::job::PendingReason;
    // A job that already reserved capacity (Staging/Ready) is not blocked on
    // resources — it is either staging or dispatchable.
    if job.bb_stage_state != BbStageState::None {
        return None;
    }
    let req = extract_bb_requirement(&job.spec);
    if req > 0 && req > free_gb {
        Some(PendingReason::BurstBufferResources)
    } else {
        None
    }
}

fn extract_license_requirements(spec: &JobSpec) -> HashMap<String, u64> {
    let mut licenses = HashMap::new();
    for gres in &spec.gres {
        if let Some((name, ltype, count)) = spur_core::resource::parse_gres(gres) {
            if name == "license" {
                let lic_name = ltype.unwrap_or_else(|| "unknown".to_string());
                *licenses.entry(lic_name).or_insert(0) += count as u64;
            }
        }
    }
    licenses
}

#[derive(Debug, PartialEq)]
pub(crate) enum RegistrationAction {
    Skip,
    Update,
    Register,
}

pub(crate) fn evaluate_registration(
    existing: Option<&Node>,
    incoming_resources: &ResourceSet,
) -> RegistrationAction {
    match existing {
        None => RegistrationAction::Register,
        Some(node) if node.total_resources != *incoming_resources => RegistrationAction::Update,
        Some(_) => RegistrationAction::Skip,
    }
}

/// Returns true if a node matches a partition's membership criteria.
/// Match occurs if the node satisfies EITHER the hostlist OR the label selector.
pub(crate) fn partition_matches_node(
    partition: &spur_core::partition::Partition,
    node_name: &str,
    labels: &HashMap<String, String>,
) -> bool {
    let matches_selector = !partition.selector.is_empty()
        && partition
            .selector
            .iter()
            .all(|(k, v)| labels.get(k) == Some(v));

    let matches_hostlist = if partition.nodes.is_empty() {
        false
    } else if partition.nodes.eq_ignore_ascii_case("ALL") {
        true
    } else {
        spur_core::hostlist::expand(&partition.nodes)
            .map(|hosts| hosts.iter().any(|h| h == node_name))
            .unwrap_or(false)
    };

    matches_selector || matches_hostlist
}

/// Returns true if a NodeConfig entry applies to a node (by hostname pattern OR
/// label selector).
pub(crate) fn node_config_matches(
    nc: &spur_core::config::NodeConfig,
    node_name: &str,
    labels: &HashMap<String, String>,
) -> bool {
    let matches_names = if nc.names.is_empty() {
        false
    } else if nc.names.eq_ignore_ascii_case("ALL") {
        true
    } else {
        spur_core::hostlist::expand(&nc.names)
            .map(|hosts| hosts.iter().any(|h| h == node_name))
            .unwrap_or(false)
    };

    let matches_selector =
        !nc.selector.is_empty() && nc.selector.iter().all(|(k, v)| labels.get(k) == Some(v));

    matches_names || matches_selector
}

#[derive(Debug, PartialEq)]
pub(crate) enum HealthAction {
    MarkDown {
        name: String,
        old_state: NodeState,
        admin_locked: bool,
    },
    Recover {
        name: String,
        old_state: NodeState,
    },
}

pub(crate) fn evaluate_node_health(
    nodes: &[&Node],
    now: DateTime<Utc>,
    timeout_secs: u64,
) -> Vec<HealthAction> {
    let threshold = chrono::Duration::seconds(timeout_secs as i64);
    let mut actions = Vec::new();

    for node in nodes {
        let Some(hb) = node.last_heartbeat else {
            continue;
        };
        let stale = now - hb > threshold;

        if stale {
            if node
                .state
                .transition(&NodeEvent::HeartbeatTimeout, node.admin_locked)
                .is_some()
            {
                actions.push(HealthAction::MarkDown {
                    name: node.name.clone(),
                    old_state: node.state,
                    admin_locked: node.admin_locked,
                });
            }
        } else if node
            .state
            .transition(&NodeEvent::HeartbeatRecovered, node.admin_locked)
            .is_some()
        {
            actions.push(HealthAction::Recover {
                name: node.name.clone(),
                old_state: node.state,
            });
        }
    }
    actions
}

fn apply_default_partition(spec: &mut JobSpec, partitions: &[Partition]) {
    if spec.partition.as_deref().is_some_and(|p| p.is_empty()) {
        spec.partition = None;
    }
    if spec.partition.is_none() {
        if let Some(default_part) = partitions.iter().find(|p| p.is_default) {
            spec.partition = Some(default_part.name.clone());
        } else if let Some(first) = partitions.first() {
            spec.partition = Some(first.name.clone());
        }
    }
}

fn apply_default_time_limit(spec: &mut JobSpec, partitions: &[Partition]) {
    if spec.time_limit.is_some() {
        return;
    }
    let partition = spec
        .partition
        .as_deref()
        .and_then(|name| partitions.iter().find(|p| p.name == name))
        .or_else(|| partitions.iter().find(|p| p.is_default))
        .or_else(|| partitions.first());
    if let Some(minutes) = partition.and_then(|p| p.default_time_minutes) {
        spec.time_limit = Some(chrono::Duration::minutes(minutes as i64));
    }
}

/// Resolve the submitting user's default account from the association cache
/// when `--account` was not provided (mirrors `apply_default_partition`).
fn apply_default_account(spec: &mut JobSpec, assoc_cache: &AssociationCache) {
    if !assoc_cache.is_loaded() {
        return;
    }
    if spec.account.as_deref().is_some_and(|a| !a.is_empty()) {
        return;
    }
    let (account, _) = assoc_cache.resolve(&spec.user, None);
    if let Some(acct) = account.filter(|a| !a.is_empty()) {
        spec.account = Some(acct);
    }
}

/// Reject a client-supplied account that is not a real user→account association.
fn validate_user_account(
    spec: &JobSpec,
    assoc_cache: &AssociationCache,
) -> Result<(), SubmitError> {
    let Some(account) = spec.account.as_deref().filter(|a| !a.is_empty()) else {
        return Ok(());
    };
    match assoc_cache.account_membership(&spec.user, account) {
        AccountMembership::CacheUnavailable | AccountMembership::Member => Ok(()),
        AccountMembership::NotMember(valid_accounts) if valid_accounts.is_empty() => {
            Err(SubmitError::invalid(format!(
                "user '{}' has no account associations. Contact your cluster admin to run: sacctmgr add user name={} account=<account>",
                spec.user, spec.user
            )))
        }
        AccountMembership::NotMember(valid_accounts) => Err(SubmitError::invalid(format!(
            "user '{}' is not associated with account '{account}'. Accounts associated with this user: [{}].",
            spec.user,
            valid_accounts.join(", ")
        ))),
    }
}

/// Resolve a job's QOS at submit, in Slurm's order: explicit `--qos` (must
/// exist and be permitted for the association) → association default →
/// cluster fallback (`accounting.default_qos`) → reject if
/// `accounting.require_qos`, else accept with no QOS.
fn apply_default_qos(
    spec: &mut JobSpec,
    assoc_cache: &AssociationCache,
    qos_cache: &QosCache,
    accounting: &spur_core::config::AccountingConfig,
) -> Result<(), SubmitError> {
    let given_account = spec.account.as_deref().filter(|a| !a.is_empty());

    if let Some(name) = spec.qos.as_deref().filter(|n| !n.is_empty()) {
        if qos_cache.get(name).is_none() {
            return Err(SubmitError::invalid(format!("QOS '{name}' does not exist")));
        }
        let (account, _) = assoc_cache.resolve(&spec.user, given_account);
        if let Some(account) = account.as_deref() {
            if !assoc_cache.qos_allowed(&spec.user, account, name) {
                return Err(SubmitError::invalid(format!(
                    "QOS '{name}' is not permitted for user '{}' under account '{account}'",
                    spec.user
                )));
            }
        }
        return Ok(());
    }

    let (account, default_qos) = assoc_cache.resolve(&spec.user, given_account);
    if let Some(default_qos) = default_qos {
        if qos_cache.get(&default_qos).is_some() {
            spec.qos = Some(default_qos);
            return Ok(());
        }
        warn!(
            user = %spec.user,
            account = account.as_deref().unwrap_or_default(),
            qos = %default_qos,
            "association default QOS no longer exists, ignoring"
        );
    }

    // A configured fallback naming a nonexistent QOS is a hard error — silently
    // ignoring it would leave the job unenforced, the gap this closes.
    let fallback = accounting.default_qos.trim();
    if !fallback.is_empty() {
        if qos_cache.get(fallback).is_none() {
            return Err(SubmitError::invalid(format!(
                "configured default QOS '{fallback}' does not exist"
            )));
        }
        spec.qos = Some(fallback.to_string());
        return Ok(());
    }

    if accounting.require_qos {
        return Err(SubmitError::invalid(
            "no QOS specified and no default QOS is configured for this user/account",
        ));
    }

    Ok(())
}

/// Expand a job spec into one or more submittable specs. For non-array jobs,
/// returns the spec unchanged. For array jobs, returns N task specs with
/// array metadata populated and `array_spec` cleared.
fn expand_job_specs(spec: JobSpec, parent_job_id: JobId) -> anyhow::Result<Vec<JobSpec>> {
    let Some(ref array_spec_str) = spec.array_spec else {
        return Ok(vec![spec]);
    };

    let array = spur_core::array::parse_array_spec(array_spec_str)
        .map_err(|e| anyhow::anyhow!("invalid array spec: {}", e))?;

    let max_concurrent = if array.max_concurrent > 0 {
        Some(array.max_concurrent)
    } else {
        None
    };

    Ok(array
        .task_ids
        .iter()
        .map(|&task_id| {
            let mut task_spec = spec.clone();
            task_spec.array_spec = None;
            task_spec.array_job_id = Some(parent_job_id);
            task_spec.array_task_id = Some(task_id);
            task_spec.array_max_concurrent = max_concurrent;
            task_spec
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use spur_core::job::JobSpec;
    use spur_core::resource::{ResourceAllocations, ResourceSet};
    use spur_metrics::job::JobMetricsSnapshot;
    use tempfile::TempDir;

    #[test]
    fn submission_size_accepts_normal_script() {
        let spec = JobSpec {
            script: Some("#!/bin/bash\necho hello\n".into()),
            ..Default::default()
        };
        assert!(check_submission_size(&spec).is_ok());
    }

    #[test]
    fn submission_size_rejects_oversized_script() {
        let spec = JobSpec {
            script: Some("x".repeat(MAX_JOB_SPEC_SIZE + 1)),
            ..Default::default()
        };
        let err = check_submission_size(&spec).expect_err("oversized script must be rejected");
        assert!(matches!(err, SubmitError::InvalidArgument(_)));
        assert!(err.to_string().contains("too large"));
    }

    #[test]
    fn submission_size_counts_all_fields() {
        // Split the payload across environment and argv (not script) to prove the
        // check bounds the whole serialized spec, not just the script field.
        let big = "y".repeat(MAX_JOB_SPEC_SIZE / 2 + 1024);
        let spec = JobSpec {
            environment: std::iter::once(("BIG".to_string(), big.clone())).collect(),
            argv: vec![big],
            ..Default::default()
        };
        let err =
            check_submission_size(&spec).expect_err("env + argv over the cap must be rejected");
        assert!(matches!(err, SubmitError::InvalidArgument(_)));
    }

    fn test_config() -> SlurmConfig {
        SlurmConfig {
            cluster_name: "test".into(),
            controller: spur_core::config::ControllerConfig {
                first_job_id: 1,
                ..Default::default()
            },
            accounting: Default::default(),
            scheduler: Default::default(),
            auth: Default::default(),
            partitions: vec![spur_core::config::PartitionConfig {
                name: "default".into(),
                default: true,
                state: "UP".into(),
                nodes: "ALL".into(),
                selector: Default::default(),
                max_time: None,
                default_time: None,
                max_nodes: None,
                min_nodes: 1,
                allow_accounts: Vec::new(),
                allow_groups: Vec::new(),
                deny_accounts: Vec::new(),
                priority_tier: 1,
                preempt_mode: String::new(),
            }],
            nodes: Vec::new(),
            network: Default::default(),
            logging: Default::default(),
            kubernetes: Default::default(),
            cluster: Default::default(),
            notifications: Default::default(),
            power: Default::default(),
            federation: Default::default(),
            topology: None,
            isolation: Default::default(),
            licenses: HashMap::new(),
            burst_buffer: Default::default(),
            update: Default::default(),
            metrics: Default::default(),
            rest_api: Default::default(),
            hooks: Default::default(),
            devices: Default::default(),
            admission: Default::default(),
            rlimits: Default::default(),
        }
    }

    async fn test_cluster(dir: &TempDir) -> Arc<ClusterManager> {
        test_cluster_with_config(dir, test_config()).await
    }

    async fn test_cluster_with_config(dir: &TempDir, config: SlurmConfig) -> Arc<ClusterManager> {
        let cm = Arc::new(ClusterManager::new(config, dir.path()).unwrap());
        let handle = crate::raft::start_raft(1, &["[::1]:0".into()], dir.path(), cm.clone())
            .await
            .unwrap();
        // Wait for the single-node Raft to self-elect before returning.
        // Without this, the first propose() call may hit a not-yet-leader
        // node and silently fail.
        handle
            .raft
            .wait(Some(std::time::Duration::from_secs(5)))
            .metrics(|m| m.current_leader == Some(1), "leader elected")
            .await
            .expect("single-node raft did not self-elect within 5s");
        cm.set_raft(handle.raft);
        cm
    }

    fn basic_spec(name: &str) -> JobSpec {
        JobSpec {
            name: name.into(),
            user: "testuser".into(),
            num_nodes: 1,
            num_tasks: 1,
            cpus_per_task: 1,
            work_dir: "/tmp".into(),
            ..Default::default()
        }
    }

    fn srun_spec(name: &str) -> JobSpec {
        let mut spec = basic_spec(name);
        spec.srun_job = true;
        spec
    }

    fn scalar_alloc(cpus: u32, memory_mb: u64) -> ResourceAllocations {
        ResourceAllocations::with_scalar(cpus, memory_mb)
    }

    fn per_node_for(
        nodes: &[&str],
        alloc: ResourceAllocations,
    ) -> HashMap<String, ResourceAllocations> {
        nodes
            .iter()
            .map(|n| ((*n).to_string(), alloc.clone()))
            .collect()
    }

    /// Spin until a Raft-proposed mutation is visible in memory.
    /// In tests, `propose()` can be called before the single-node Raft
    /// has finished its initial self-election, causing `client_write` to
    /// fail silently. This helper retries until the election completes
    /// and the mutation is applied.
    fn wait_for<F: Fn() -> bool>(label: &str, f: F) {
        for _ in 0..200 {
            if f() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("timed out waiting for: {label}");
    }

    fn register_node(cm: &ClusterManager, name: &str, cpus: u32, mem: u64) {
        cm.register_node(
            name.into(),
            ResourceSet {
                cpus,
                memory_mb: mem,
                ..Default::default()
            },
            "127.0.0.1".into(),
            6818,
            String::new(),
            String::new(),
            spur_core::node::NodeSource::NativeHost,
            HashMap::new(),
        )
        .unwrap();
        let n = name.to_string();
        wait_for(&format!("node '{n}' registered"), || {
            cm.get_node(&n).is_some()
        });
    }

    fn submit_and_wait(cm: &ClusterManager, spec: JobSpec) -> JobId {
        let id = cm.submit_job(spec).unwrap();
        wait_for(&format!("job {id} applied"), || cm.get_job(id).is_some());
        id
    }

    /// Wait for a job to reach the expected state.
    /// Handles the test-only race where propose() is called before the
    /// single-node Raft has self-elected.
    fn settle(cm: &ClusterManager, job_id: JobId, expected: JobState) {
        wait_for(&format!("job {job_id} -> {expected:?}"), || {
            cm.get_job(job_id).is_some_and(|j| j.state == expected)
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_job_submit() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        let spec = basic_spec("test-job");
        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(spec.clone()),
        });

        let job = cm.get_job(1).unwrap();
        assert_eq!(job.job_id, 1);
        assert_eq!(job.spec.name, "test-job");
        assert_eq!(job.state, JobState::Pending);
        assert!(cm.next_job_id.load(Ordering::Relaxed) >= 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cold_start_respects_first_job_id() {
        let dir = TempDir::new().unwrap();
        let mut config = test_config();
        config.controller.first_job_id = 500;
        let cm = test_cluster_with_config(&dir, config).await;

        assert_eq!(cm.next_job_id.load(Ordering::Relaxed), 500);

        let job_id = submit_and_wait(&cm, basic_spec("first-job"));
        assert_eq!(job_id, 500);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_job_state_change() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(basic_spec("j")),
        });
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id: 1,
            old_state: JobState::Pending,
            new_state: JobState::Running,
            pending_reason: None,
            pending_priority: None,
        });

        let job = cm.get_job(1).unwrap();
        assert_eq!(job.state, JobState::Running);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_job_start_allocates_resources() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        register_node(&cm, "node1", 8, 16000);
        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(basic_spec("j")),
        });

        let resources = scalar_alloc(4, 8000);
        cm.apply_operation(&WalOperation::JobStart {
            job_id: 1,
            nodes: vec!["node1".into()],
            resources: resources.clone(),
            per_node_alloc: per_node_for(&["node1"], resources),
            srun_step_dispatch: false,
            run_attempt: 0,
        });

        let job = cm.get_job(1).unwrap();
        assert!(job.start_time.is_some());
        assert_eq!(job.allocated_nodes, vec!["node1"]);

        let node = cm.get_node("node1").unwrap();
        assert_eq!(node.alloc_resources.cpus, 4);
        assert_eq!(node.alloc_resources.memory_mb, 8000);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_job_complete_deallocates_resources() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        register_node(&cm, "node1", 8, 16000);
        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(basic_spec("j")),
        });
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id: 1,
            old_state: JobState::Pending,
            new_state: JobState::Running,
            pending_reason: None,
            pending_priority: None,
        });
        let alloc = scalar_alloc(4, 8000);
        cm.apply_operation(&WalOperation::JobStart {
            job_id: 1,
            nodes: vec!["node1".into()],
            resources: alloc.clone(),
            per_node_alloc: per_node_for(&["node1"], alloc),
            srun_step_dispatch: false,
            run_attempt: 0,
        });

        cm.apply_operation(&WalOperation::JobComplete {
            job_id: 1,
            exit_code: 0,
            state: JobState::Completed,
        });

        let job = cm.get_job(1).unwrap();
        assert_eq!(job.state, JobState::Completed);
        assert_eq!(job.exit_code, Some(0));
        assert!(job.end_time.is_some());

        let node = cm.get_node("node1").unwrap();
        assert_eq!(node.alloc_resources.cpus, 0);
        assert_eq!(node.alloc_resources.memory_mb, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_suspend_then_resume_accumulates_suspended_secs() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(basic_spec("s")),
        });
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id: 1,
            old_state: JobState::Pending,
            new_state: JobState::Running,
            pending_reason: None,
            pending_priority: None,
        });
        let t0 = chrono::Utc::now();
        cm.apply_operation(&WalOperation::JobSuspend { job_id: 1, at: t0 });
        assert_eq!(cm.get_job(1).unwrap().state, JobState::Suspended);
        cm.apply_operation(&WalOperation::JobResume {
            job_id: 1,
            at: t0 + chrono::Duration::seconds(25),
        });
        let job = cm.get_job(1).unwrap();
        assert_eq!(job.state, JobState::Running);
        assert_eq!(job.suspended_secs, 25);
        assert!(job.suspended_at.is_none());
    }

    // ── suspend_job / resume_job method guards ───────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn suspend_job_rejects_pending() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        let id = submit_and_wait(&cm, basic_spec("p"));
        // Job is Pending (never started).
        let err = cm.suspend_job(id, "u").unwrap_err();
        assert!(
            err.to_string().contains("not running"),
            "unexpected error: {err}"
        );
        assert_eq!(cm.get_job(id).unwrap().state, JobState::Pending);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resume_job_rejects_pending() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        let id = submit_and_wait(&cm, basic_spec("p"));
        let err = cm.resume_job(id, "u").unwrap_err();
        assert!(
            err.to_string().contains("not suspended"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resume_job_rejects_running() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        let id = submit_and_wait(&cm, basic_spec("r"));
        let res = scalar_alloc(2, 4000);
        cm.start_job(
            id,
            vec!["n1".into()],
            res.clone(),
            per_node_for(&["n1"], res),
        )
        .unwrap();
        settle(&cm, id, JobState::Running);
        // Resuming a running (not suspended) job is rejected.
        assert!(cm.resume_job(id, "u").is_err());
        assert_eq!(cm.get_job(id).unwrap().state, JobState::Running);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn suspend_resume_unknown_job_errors() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        assert!(cm
            .suspend_job(9999, "u")
            .unwrap_err()
            .to_string()
            .contains("not found"));
        assert!(cm
            .resume_job(9999, "u")
            .unwrap_err()
            .to_string()
            .contains("not found"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn double_suspend_is_rejected() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        let id = submit_and_wait(&cm, basic_spec("d"));
        let res = scalar_alloc(2, 4000);
        cm.start_job(
            id,
            vec!["n1".into()],
            res.clone(),
            per_node_for(&["n1"], res),
        )
        .unwrap();
        settle(&cm, id, JobState::Running);
        cm.suspend_job(id, "testuser").unwrap();
        settle(&cm, id, JobState::Suspended);
        // Second suspend on an already-suspended job is rejected (not Running).
        assert!(cm.suspend_job(id, "testuser").is_err());
        assert_eq!(cm.get_job(id).unwrap().state, JobState::Suspended);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn double_resume_is_rejected() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        let id = submit_and_wait(&cm, basic_spec("d"));
        let res = scalar_alloc(2, 4000);
        cm.start_job(
            id,
            vec!["n1".into()],
            res.clone(),
            per_node_for(&["n1"], res),
        )
        .unwrap();
        settle(&cm, id, JobState::Running);
        cm.suspend_job(id, "testuser").unwrap();
        settle(&cm, id, JobState::Suspended);
        cm.resume_job(id, "testuser").unwrap();
        settle(&cm, id, JobState::Running);
        // Second resume on an already-running job is rejected.
        assert!(cm.resume_job(id, "testuser").is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn suspend_retains_node_allocation() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        let id = submit_and_wait(&cm, basic_spec("a"));
        let res = scalar_alloc(2, 4000);
        cm.start_job(
            id,
            vec!["n1".into()],
            res.clone(),
            per_node_for(&["n1"], res),
        )
        .unwrap();
        settle(&cm, id, JobState::Running);
        assert_eq!(cm.get_node("n1").unwrap().alloc_resources.cpus, 2);

        cm.suspend_job(id, "testuser").unwrap();
        settle(&cm, id, JobState::Suspended);
        // Allocation is retained while suspended (plain scontrol suspend parity).
        let job = cm.get_job(id).unwrap();
        assert_eq!(job.allocated_nodes, vec!["n1".to_string()]);
        assert_eq!(
            cm.get_node("n1").unwrap().alloc_resources.cpus,
            2,
            "node resources must stay allocated while job is suspended"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_suspend_cycles_accumulate_seconds() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(basic_spec("acc")),
        });
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id: 1,
            old_state: JobState::Pending,
            new_state: JobState::Running,
            pending_reason: None,
            pending_priority: None,
        });
        let t0 = chrono::Utc::now();
        // Cycle 1: 10s suspended.
        cm.apply_operation(&WalOperation::JobSuspend { job_id: 1, at: t0 });
        cm.apply_operation(&WalOperation::JobResume {
            job_id: 1,
            at: t0 + chrono::Duration::seconds(10),
        });
        // Cycle 2: 15s suspended.
        let t1 = t0 + chrono::Duration::seconds(40);
        cm.apply_operation(&WalOperation::JobSuspend { job_id: 1, at: t1 });
        cm.apply_operation(&WalOperation::JobResume {
            job_id: 1,
            at: t1 + chrono::Duration::seconds(15),
        });
        let job = cm.get_job(1).unwrap();
        assert_eq!(job.state, JobState::Running);
        assert_eq!(job.suspended_secs, 25, "10 + 15 accumulated");
        assert!(job.suspended_at.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_while_suspended_finalizes_suspended_at() {
        // Copilot review: a Suspended -> terminal transition must clear
        // suspended_at (so it never lingers on a terminal job and
        // `suspended_at.is_some()` keeps meaning "currently suspended") and fold
        // the final suspended interval into suspended_secs.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(basic_spec("cancel-susp")),
        });
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id: 1,
            old_state: JobState::Pending,
            new_state: JobState::Running,
            pending_reason: None,
            pending_priority: None,
        });
        // Suspended 30s ago, then cancelled now (JobComplete stamps Utc::now()).
        let since = chrono::Utc::now() - chrono::Duration::seconds(30);
        cm.apply_operation(&WalOperation::JobSuspend {
            job_id: 1,
            at: since,
        });
        cm.apply_operation(&WalOperation::JobComplete {
            job_id: 1,
            exit_code: 0,
            state: JobState::Cancelled,
        });
        let job = cm.get_job(1).unwrap();
        assert_eq!(job.state, JobState::Cancelled);
        assert!(
            job.suspended_at.is_none(),
            "suspended_at must be cleared on a Suspended -> terminal transition"
        );
        assert!(
            job.suspended_secs >= 30,
            "final suspended interval folded into suspended_secs (got {})",
            job.suspended_secs
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn suspended_job_excluded_from_timelimit_scan() {
        // The time-limit enforcer scans only [Running, Completing] jobs, so a
        // suspended job is never warned/killed while frozen. Assert the exact
        // query the enforcer uses does not return a suspended job.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        let id = submit_and_wait(&cm, basic_spec("t"));
        let res = scalar_alloc(2, 4000);
        cm.start_job(
            id,
            vec!["n1".into()],
            res.clone(),
            per_node_for(&["n1"], res),
        )
        .unwrap();
        settle(&cm, id, JobState::Running);
        cm.suspend_job(id, "testuser").unwrap();
        settle(&cm, id, JobState::Suspended);

        let scanned = cm.get_jobs(
            &[JobState::Running, JobState::Completing],
            None,
            None,
            None,
            None,
            &[],
        );
        assert!(
            !scanned.iter().any(|j| j.job_id == id),
            "suspended job must not appear in the enforcer's Running/Completing scan"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_node_register() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        cm.apply_operation(&WalOperation::NodeRegister {
            name: "gpu-node".into(),
            resources: ResourceSet {
                cpus: 64,
                memory_mb: 256000,
                ..Default::default()
            },
            address: "10.0.0.1".into(),
            port: 6818,
            wg_pubkey: String::new(),
            version: "1.0".into(),
            labels: HashMap::new(),
        });

        let node = cm.get_node("gpu-node").unwrap();
        assert_eq!(node.total_resources.cpus, 64);
        assert_eq!(node.state, NodeState::Idle);
        assert_eq!(node.address, Some("10.0.0.1".into()));
        // Dynamically registered nodes get the default partition
        assert!(
            !node.partitions.is_empty(),
            "node should be assigned to default partition"
        );
        assert_eq!(node.partitions[0], "default");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_node_state_change() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        register_node(&cm, "n1", 4, 8000);
        cm.apply_operation(&WalOperation::NodeStateChange {
            name: "n1".into(),
            old_state: NodeState::Idle,
            new_state: NodeState::Drain,
            reason: Some("maintenance".into()),
            admin_locked: true,
        });

        let node = cm.get_node("n1").unwrap();
        assert_eq!(node.state, NodeState::Drain);
        assert_eq!(node.state_reason, Some("maintenance".into()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_job_priority_change() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(basic_spec("j")),
        });
        cm.apply_operation(&WalOperation::JobPriorityChange {
            job_id: 1,
            old_priority: 1000,
            new_priority: 5000,
            pending_reason: None,
            reset_requeue_count: false,
            clear_reservation: false,
        });

        let job = cm.get_job(1).unwrap();
        assert_eq!(job.priority, 5000);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submit_job_assigns_id_and_applies() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        let id = submit_and_wait(&cm, basic_spec("my-job"));
        assert!(id >= 1);

        let job = cm.get_job(id).unwrap();
        assert_eq!(job.spec.name, "my-job");
        assert_eq!(job.state, JobState::Pending);
        assert_eq!(job.spec.partition, Some("default".into()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submit_multiple_jobs_increments_ids() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        let id1 = submit_and_wait(&cm, basic_spec("a"));
        let id2 = submit_and_wait(&cm, basic_spec("b"));
        let id3 = submit_and_wait(&cm, basic_spec("c"));

        assert!(id2 > id1);
        assert!(id3 > id2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_and_complete_job_lifecycle() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        register_node(&cm, "worker1", 8, 16000);
        let job_id = submit_and_wait(&cm, basic_spec("lifecycle"));

        let resources = scalar_alloc(2, 4000);
        cm.start_job(
            job_id,
            vec!["worker1".into()],
            resources.clone(),
            per_node_for(&["worker1"], resources),
        )
        .unwrap();
        settle(&cm, job_id, JobState::Running);

        let job = cm.get_job(job_id).unwrap();
        assert_eq!(job.state, JobState::Running);
        assert!(job.start_time.is_some());

        let node = cm.get_node("worker1").unwrap();
        assert_eq!(node.alloc_resources.cpus, 2);

        cm.complete_job(job_id, 0, JobState::Completed).unwrap();
        settle(&cm, job_id, JobState::Completed);

        let job = cm.get_job(job_id).unwrap();
        assert_eq!(job.state, JobState::Completed);
        assert_eq!(job.exit_code, Some(0));

        let node = cm.get_node("worker1").unwrap();
        assert_eq!(node.alloc_resources.cpus, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sched_stats_track_submit_start_complete() {
        use std::sync::Arc;

        use crate::sched_stats::SchedStatsCollector;

        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        let stats = Arc::new(SchedStatsCollector::new("backfill"));
        cm.set_sched_stats(stats.clone());

        register_node(&cm, "worker1", 8, 16000);
        let job_id = submit_and_wait(&cm, basic_spec("stats-job"));
        assert_eq!(stats.snapshot().jobs_submitted, 1);

        let resources = scalar_alloc(2, 4000);
        cm.start_job(
            job_id,
            vec!["worker1".into()],
            resources.clone(),
            per_node_for(&["worker1"], resources),
        )
        .unwrap();
        cm.record_sched_cycle(0, 0, 1, false);
        assert_eq!(stats.snapshot().jobs_started, 1);

        cm.complete_job(job_id, 0, JobState::Completed).unwrap();
        settle(&cm, job_id, JobState::Completed);
        assert_eq!(stats.snapshot().jobs_finalized, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_job_node_complete_single_node() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        register_node(&cm, "worker1", 8, 16000);
        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(basic_spec("single-completing")),
        });
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id: 1,
            old_state: JobState::Pending,
            new_state: JobState::Running,
            pending_reason: None,
            pending_priority: None,
        });
        let alloc = scalar_alloc(2, 4000);
        cm.apply_operation(&WalOperation::JobStart {
            job_id: 1,
            nodes: vec!["worker1".into()],
            resources: alloc.clone(),
            per_node_alloc: per_node_for(&["worker1"], alloc),
            srun_step_dispatch: false,
            run_attempt: 0,
        });

        cm.apply_operation(&WalOperation::JobNodeComplete {
            job_id: 1,
            node_name: "worker1".into(),
            exit_code: 0,
            signal: 0,
        });

        let job = cm.get_job(1).unwrap();
        assert_eq!(job.state, JobState::Completed);
        assert_eq!(job.exit_code, Some(0));
        assert!(job.node_completions.is_empty());
        assert_eq!(cm.get_node("worker1").unwrap().alloc_resources.cpus, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_job_node_complete_oom_sets_out_of_memory() {
        // spurd reports an OOM kill as SIGKILL with the OOM sentinel bit OR'd in.
        // The job must finalize as OUT_OF_MEMORY / Reason=OutOfMemory, with the
        // sentinel stripped so the stored signal is the real SIGKILL (9).
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "worker1", 8, 16000);
        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(basic_spec("oom")),
        });
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id: 1,
            old_state: JobState::Pending,
            new_state: JobState::Running,
            pending_reason: None,
            pending_priority: None,
        });
        let alloc = scalar_alloc(2, 4000);
        cm.apply_operation(&WalOperation::JobStart {
            job_id: 1,
            nodes: vec!["worker1".into()],
            resources: alloc.clone(),
            per_node_alloc: per_node_for(&["worker1"], alloc),
            srun_step_dispatch: false,
            run_attempt: 0,
        });

        cm.apply_operation(&WalOperation::JobNodeComplete {
            job_id: 1,
            node_name: "worker1".into(),
            exit_code: 0,
            signal: spur_core::job::OOM_SIGNAL_FLAG | 9,
        });

        let job = cm.get_job(1).unwrap();
        assert_eq!(job.state, JobState::OutOfMemory);
        assert_eq!(job.pending_reason, PendingReason::OutOfMemory);
        assert_eq!(job.exit_signal, 9, "OOM sentinel must be stripped");
        assert_eq!(cm.get_node("worker1").unwrap().alloc_resources.cpus, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_job_node_complete_multi_node() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        for name in ["n1", "n2", "n3"] {
            register_node(&cm, name, 8, 16000);
        }

        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(basic_spec("multi-completing")),
        });
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id: 1,
            old_state: JobState::Pending,
            new_state: JobState::Running,
            pending_reason: None,
            pending_priority: None,
        });
        let alloc = scalar_alloc(2, 4000);
        cm.apply_operation(&WalOperation::JobStart {
            job_id: 1,
            nodes: vec!["n1".into(), "n2".into(), "n3".into()],
            resources: scalar_alloc(6, 12000),
            per_node_alloc: per_node_for(&["n1", "n2", "n3"], alloc),
            srun_step_dispatch: false,
            run_attempt: 0,
        });

        cm.apply_operation(&WalOperation::JobNodeComplete {
            job_id: 1,
            node_name: "n1".into(),
            exit_code: 0,
            signal: 0,
        });
        let job = cm.get_job(1).unwrap();
        assert_eq!(job.state, JobState::Completing);
        assert_eq!(job.node_completions.len(), 1);
        assert_eq!(cm.get_node("n1").unwrap().alloc_resources.cpus, 0);
        assert!(cm.get_node("n2").unwrap().alloc_resources.cpus > 0);

        cm.apply_operation(&WalOperation::JobNodeComplete {
            job_id: 1,
            node_name: "n2".into(),
            exit_code: 0,
            signal: 0,
        });
        assert_eq!(cm.get_job(1).unwrap().state, JobState::Completing);

        cm.apply_operation(&WalOperation::JobNodeComplete {
            job_id: 1,
            node_name: "n3".into(),
            exit_code: 42,
            signal: 0,
        });

        let job = cm.get_job(1).unwrap();
        // ExitCode follows the primary (batch) node n1 = allocated_nodes[0],
        // which exited 0 — so the job state/exit_code reflect a clean primary.
        assert_eq!(job.state, JobState::Completed);
        assert_eq!(job.exit_code, Some(0));
        // DerivedExitCode is the max over srun *steps* (Slurm parity), not node
        // completions. This job ran no srun steps, so it is 0 — the non-primary
        // node's exit 42 does not surface here.
        assert_eq!(job.derived_exit_code, 0);
        for name in ["n1", "n2", "n3"] {
            assert_eq!(cm.get_node(name).unwrap().alloc_resources.cpus, 0);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn step_complete_accumulates_derived_exit_code_running_max() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);

        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(basic_spec("steps")),
        });
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id: 1,
            old_state: JobState::Pending,
            new_state: JobState::Running,
            pending_reason: None,
            pending_priority: None,
        });
        cm.apply_operation(&WalOperation::JobStart {
            job_id: 1,
            nodes: vec!["n1".into()],
            resources: scalar_alloc(4, 8000),
            per_node_alloc: per_node_for(&["n1"], scalar_alloc(4, 8000)),
            srun_step_dispatch: false,
            run_attempt: 0,
        });

        // Three srun steps exit 7, 3, 2 (in that order). DerivedExitCode tracks
        // the running max live; ExitCode is unaffected (it is the batch exit).
        cm.apply_operation(&WalOperation::JobStepComplete {
            job_id: 1,
            step_id: 0,
            exit_code: 7,
        });
        assert_eq!(cm.get_job(1).unwrap().derived_exit_code, 7);
        cm.apply_operation(&WalOperation::JobStepComplete {
            job_id: 1,
            step_id: 1,
            exit_code: 3,
        });
        // 3 < 7, running max stays 7.
        assert_eq!(cm.get_job(1).unwrap().derived_exit_code, 7);
        cm.apply_operation(&WalOperation::JobStepComplete {
            job_id: 1,
            step_id: 2,
            exit_code: 2,
        });
        assert_eq!(cm.get_job(1).unwrap().derived_exit_code, 7);

        // Batch script exits 2 -> ExitCode=2:0, DerivedExitCode preserved at 7.
        cm.apply_operation(&WalOperation::JobNodeComplete {
            job_id: 1,
            node_name: "n1".into(),
            exit_code: 2,
            signal: 0,
        });
        let job = cm.get_job(1).unwrap();
        assert_eq!(job.state, JobState::Failed);
        assert_eq!(job.exit_code, Some(2));
        assert_eq!(job.derived_exit_code, 7);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn step_complete_batch_step_excluded_from_derived() {
        // The reserved batch step carries the job's own exit, not a step result,
        // so it must NOT contribute to DerivedExitCode.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);

        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(basic_spec("batch-only")),
        });
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id: 1,
            old_state: JobState::Pending,
            new_state: JobState::Running,
            pending_reason: None,
            pending_priority: None,
        });

        cm.apply_operation(&WalOperation::JobStepComplete {
            job_id: 1,
            step_id: STEP_BATCH,
            exit_code: 9,
        });
        // Reserved step id -> derived untouched.
        assert_eq!(cm.get_job(1).unwrap().derived_exit_code, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_job_node_complete_returns_finalized_once() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        for name in ["n1", "n2"] {
            register_node(&cm, name, 8, 16000);
        }

        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(basic_spec("finalize-response")),
        });
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id: 1,
            old_state: JobState::Pending,
            new_state: JobState::Running,
            pending_reason: None,
            pending_priority: None,
        });
        let alloc = scalar_alloc(2, 4000);
        cm.apply_operation(&WalOperation::JobStart {
            job_id: 1,
            nodes: vec!["n1".into(), "n2".into()],
            resources: scalar_alloc(4, 8000),
            per_node_alloc: per_node_for(&["n1", "n2"], alloc),
            srun_step_dispatch: false,
            run_attempt: 0,
        });

        let r1 = cm.apply_operation(&WalOperation::JobNodeComplete {
            job_id: 1,
            node_name: "n1".into(),
            exit_code: 0,
            signal: 0,
        });
        assert!(r1.jobs_finalized.is_empty());
        assert_eq!(cm.get_job(1).unwrap().state, JobState::Completing);

        let r2 = cm.apply_operation(&WalOperation::JobNodeComplete {
            job_id: 1,
            node_name: "n2".into(),
            exit_code: 0,
            signal: 0,
        });
        let f = r2
            .jobs_finalized
            .first()
            .expect("last node should finalize");
        assert_eq!(f.job_id, 1);
        assert_eq!(f.state, JobState::Completed);
        assert_eq!(f.exit_code, 0);
        assert_eq!(cm.get_job(1).unwrap().state, JobState::Completed);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_job_complete_returns_finalized() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        register_node(&cm, "worker1", 8, 16000);
        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(basic_spec("job-complete-response")),
        });
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id: 1,
            old_state: JobState::Pending,
            new_state: JobState::Running,
            pending_reason: None,
            pending_priority: None,
        });
        let alloc = scalar_alloc(2, 4000);
        cm.apply_operation(&WalOperation::JobStart {
            job_id: 1,
            nodes: vec!["worker1".into()],
            resources: alloc.clone(),
            per_node_alloc: per_node_for(&["worker1"], alloc),
            srun_step_dispatch: false,
            run_attempt: 0,
        });

        let resp = cm.apply_operation(&WalOperation::JobComplete {
            job_id: 1,
            exit_code: 0,
            state: JobState::Completed,
        });
        let f = resp
            .jobs_finalized
            .first()
            .expect("JobComplete should finalize");
        assert_eq!(f.job_id, 1);
        assert_eq!(f.state, JobState::Completed);
        assert_eq!(f.exit_code, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_job_complete_noop_when_already_terminal() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        register_node(&cm, "worker1", 8, 16000);
        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(basic_spec("double-complete")),
        });
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id: 1,
            old_state: JobState::Pending,
            new_state: JobState::Running,
            pending_reason: None,
            pending_priority: None,
        });
        let alloc = scalar_alloc(2, 4000);
        cm.apply_operation(&WalOperation::JobStart {
            job_id: 1,
            nodes: vec!["worker1".into()],
            resources: alloc.clone(),
            per_node_alloc: per_node_for(&["worker1"], alloc),
            srun_step_dispatch: false,
            run_attempt: 0,
        });

        let first = cm.apply_operation(&WalOperation::JobComplete {
            job_id: 1,
            exit_code: 0,
            state: JobState::Completed,
        });
        assert!(
            !first.jobs_finalized.is_empty(),
            "first JobComplete should finalize"
        );
        let node = cm.get_node("worker1").unwrap();
        assert_eq!(node.alloc_resources.cpus, 0);
        assert_eq!(node.alloc_resources.memory_mb, 0);

        let second = cm.apply_operation(&WalOperation::JobComplete {
            job_id: 1,
            exit_code: -1,
            state: JobState::Cancelled,
        });
        assert!(second.jobs_finalized.is_empty());

        let job = cm.get_job(1).unwrap();
        assert_eq!(job.state, JobState::Completed);
        assert_eq!(job.exit_code, Some(0));

        let node = cm.get_node("worker1").unwrap();
        assert_eq!(node.alloc_resources.cpus, 0);
        assert_eq!(node.alloc_resources.memory_mb, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn node_complete_penultimate_returns_completing() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        for name in ["n1", "n2", "n3"] {
            register_node(&cm, name, 8, 16000);
        }

        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(basic_spec("penultimate")),
        });
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id: 1,
            old_state: JobState::Pending,
            new_state: JobState::Running,
            pending_reason: None,
            pending_priority: None,
        });
        let alloc = scalar_alloc(2, 4000);
        cm.apply_operation(&WalOperation::JobStart {
            job_id: 1,
            nodes: vec!["n1".into(), "n2".into(), "n3".into()],
            resources: scalar_alloc(6, 12000),
            per_node_alloc: per_node_for(&["n1", "n2", "n3"], alloc),
            srun_step_dispatch: false,
            run_attempt: 0,
        });
        cm.apply_operation(&WalOperation::JobNodeComplete {
            job_id: 1,
            node_name: "n1".into(),
            exit_code: 0,
            signal: 0,
        });

        let result = cm.node_complete(1, "n2", 0, 0, 0).unwrap();
        assert_eq!(result, NodeCompleteResult::Completing);
        assert_eq!(cm.get_job(1).unwrap().state, JobState::Completing);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn node_complete_sets_signal_reason_and_derived() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        register_node(&cm, "n1", 8, 16000);

        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(basic_spec("signal-job")),
        });
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id: 1,
            old_state: JobState::Pending,
            new_state: JobState::Running,
            pending_reason: None,
            pending_priority: None,
        });
        cm.apply_operation(&WalOperation::JobStart {
            job_id: 1,
            nodes: vec!["n1".into()],
            resources: scalar_alloc(6, 12000),
            per_node_alloc: per_node_for(&["n1"], scalar_alloc(6, 12000)),
            srun_step_dispatch: false,
            run_attempt: 0,
        });

        cm.node_complete(1, "n1", 0, 9, 0).unwrap();
        let job = cm.get_job(1).unwrap();
        assert_eq!(job.state, JobState::Failed);
        assert_eq!(job.exit_code, Some(0));
        assert_eq!(job.exit_signal, 9);
        assert_eq!(job.derived_exit_code, 0);
        assert_eq!(job.pending_reason, PendingReason::RaisedSignal);
    }

    // Reproduces the two steps report_job_status performs (validate the wire
    // report, then node_complete) since ControllerService can't be built here.
    // A signaled job's report (Completed, exit_code=0, signal=9) must be accepted
    // and rederived to Failed / exit_signal=9 / RaisedSignal.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_path_signaled_completion_accepted_and_rederived_failed() {
        // Step 1: validate the wire report (Completed, exit_code=0) — must pass.
        JobState::validate_completion_report_state(JobState::Completed, 0)
            .expect("agent (Completed, exit_code=0) signaled report must pass RPC validation");

        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        register_node(&cm, "n1", 8, 16000);

        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(basic_spec("rpc-signal-job")),
        });
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id: 1,
            old_state: JobState::Pending,
            new_state: JobState::Running,
            pending_reason: None,
            pending_priority: None,
        });
        cm.apply_operation(&WalOperation::JobStart {
            job_id: 1,
            nodes: vec!["n1".into()],
            resources: scalar_alloc(6, 12000),
            per_node_alloc: per_node_for(&["n1"], scalar_alloc(6, 12000)),
            srun_step_dispatch: false,
            run_attempt: 0,
        });

        // Step 2: the call the RPC makes after validation (wire state dropped).
        cm.node_complete(1, "n1", 0, 9, 0).unwrap();

        let job = cm.get_job(1).unwrap();
        assert_eq!(job.state, JobState::Failed);
        assert_eq!(job.exit_code, Some(0));
        assert_eq!(job.exit_signal, 9);
        assert_eq!(job.pending_reason, PendingReason::RaisedSignal);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn node_complete_sets_nonzero_exit_reason() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        register_node(&cm, "n1", 8, 16000);

        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(basic_spec("exit-job")),
        });
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id: 1,
            old_state: JobState::Pending,
            new_state: JobState::Running,
            pending_reason: None,
            pending_priority: None,
        });
        cm.apply_operation(&WalOperation::JobStart {
            job_id: 1,
            nodes: vec!["n1".into()],
            resources: scalar_alloc(6, 12000),
            per_node_alloc: per_node_for(&["n1"], scalar_alloc(6, 12000)),
            srun_step_dispatch: false,
            run_attempt: 0,
        });

        cm.node_complete(1, "n1", 42, 0, 0).unwrap();
        let job = cm.get_job(1).unwrap();
        assert_eq!(job.state, JobState::Failed);
        assert_eq!(job.exit_code, Some(42));
        assert_eq!(job.exit_signal, 0);
        // No srun steps ran, so DerivedExitCode is 0 (Slurm parity) — the batch
        // exit (42) surfaces as ExitCode, not DerivedExitCode.
        assert_eq!(job.derived_exit_code, 0);
        assert_eq!(job.pending_reason, PendingReason::NonZeroExitCode);
    }

    // A completion report from a superseded run (older epoch) must be dropped,
    // not fail the re-dispatched run. Reproduces the preempt-requeue race.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn node_complete_drops_stale_run_report() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);

        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(basic_spec("stale-job")),
        });
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id: 1,
            old_state: JobState::Pending,
            new_state: JobState::Running,
            pending_reason: None,
            pending_priority: None,
        });
        // Re-dispatched run: current epoch is 2.
        cm.apply_operation(&WalOperation::JobStart {
            job_id: 1,
            nodes: vec!["n1".into()],
            resources: scalar_alloc(6, 12000),
            per_node_alloc: per_node_for(&["n1"], scalar_alloc(6, 12000)),
            srun_step_dispatch: false,
            run_attempt: 2,
        });

        // Stale SIGKILL report from epoch 1 must be ignored.
        let res = cm.node_complete(1, "n1", 0, 9, 1).unwrap();
        assert!(matches!(res, NodeCompleteResult::StaleReport));
        let job = cm.get_job(1).unwrap();
        assert_eq!(job.state, JobState::Running);

        // Current-epoch report is applied normally.
        cm.node_complete(1, "n1", 0, 9, 2).unwrap();
        assert_eq!(cm.get_job(1).unwrap().state, JobState::Failed);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_job_while_completing() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        for name in ["n1", "n2", "n3"] {
            register_node(&cm, name, 8, 16000);
        }

        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(basic_spec("cancel-while-cg")),
        });
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id: 1,
            old_state: JobState::Pending,
            new_state: JobState::Running,
            pending_reason: None,
            pending_priority: None,
        });
        let alloc = scalar_alloc(2, 4000);
        cm.apply_operation(&WalOperation::JobStart {
            job_id: 1,
            nodes: vec!["n1".into(), "n2".into(), "n3".into()],
            resources: scalar_alloc(6, 12000),
            per_node_alloc: per_node_for(&["n1", "n2", "n3"], alloc),
            srun_step_dispatch: false,
            run_attempt: 0,
        });

        cm.apply_operation(&WalOperation::JobNodeComplete {
            job_id: 1,
            node_name: "n1".into(),
            exit_code: 0,
            signal: 0,
        });

        let job = cm.get_job(1).unwrap();
        assert_eq!(job.state, JobState::Completing);
        assert_eq!(job.node_completions.len(), 1);
        assert_eq!(cm.get_node("n1").unwrap().alloc_resources.cpus, 0);
        assert!(cm.get_node("n2").unwrap().alloc_resources.cpus > 0);

        cm.cancel_job(1, "testuser").unwrap();
        settle(&cm, 1, JobState::Cancelled);

        let job = cm.get_job(1).unwrap();
        assert_eq!(job.state, JobState::Cancelled);
        assert_eq!(job.exit_code, Some(-1));
        assert!(job.node_completions.is_empty());
        for name in ["n1", "n2", "n3"] {
            assert_eq!(
                cm.get_node(name).unwrap().alloc_resources.cpus,
                0,
                "node {name} should be deallocated after cancel"
            );
        }

        cm.apply_operation(&WalOperation::JobNodeComplete {
            job_id: 1,
            node_name: "n2".into(),
            exit_code: 0,
            signal: 0,
        });

        let job = cm.get_job(1).unwrap();
        assert_eq!(job.state, JobState::Cancelled);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn node_complete_returns_already_terminal_after_cancel() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        for name in ["n1", "n2", "n3"] {
            register_node(&cm, name, 8, 16000);
        }

        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(basic_spec("nc-after-cancel")),
        });
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id: 1,
            old_state: JobState::Pending,
            new_state: JobState::Running,
            pending_reason: None,
            pending_priority: None,
        });
        let alloc = scalar_alloc(2, 4000);
        cm.apply_operation(&WalOperation::JobStart {
            job_id: 1,
            nodes: vec!["n1".into(), "n2".into(), "n3".into()],
            resources: scalar_alloc(6, 12000),
            per_node_alloc: per_node_for(&["n1", "n2", "n3"], alloc),
            srun_step_dispatch: false,
            run_attempt: 0,
        });
        cm.apply_operation(&WalOperation::JobNodeComplete {
            job_id: 1,
            node_name: "n1".into(),
            exit_code: 0,
            signal: 0,
        });

        cm.cancel_job(1, "testuser").unwrap();
        settle(&cm, 1, JobState::Cancelled);

        let result = cm.node_complete(1, "n2", 0, 0, 0).unwrap();
        assert_eq!(result, NodeCompleteResult::AlreadyTerminal);
        assert_eq!(cm.get_job(1).unwrap().state, JobState::Cancelled);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn job_metrics_track_lifecycle() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        assert_eq!(cm.job_metrics(), JobMetricsSnapshot::default());

        register_node(&cm, "worker1", 8, 16000);
        let job_id = submit_and_wait(&cm, basic_spec("metrics-job"));

        let m = cm.job_metrics();
        assert_eq!(m.total, 1);
        assert_eq!(m.count_state(JobState::Pending), 1);

        let resources = scalar_alloc(4, 8192);
        cm.start_job(
            job_id,
            vec!["worker1".into()],
            resources.clone(),
            per_node_for(&["worker1"], resources),
        )
        .unwrap();
        settle(&cm, job_id, JobState::Running);

        let m = cm.job_metrics();
        assert_eq!(m.count_state(JobState::Running), 1);
        assert_eq!(m.running_cpus, 4);
        assert_eq!(m.running_memory_bytes, 8192 * 1024 * 1024);

        cm.complete_job(job_id, 0, JobState::Completed).unwrap();
        settle(&cm, job_id, JobState::Completed);

        let m = cm.job_metrics();
        assert_eq!(m.count_state(JobState::Completed), 1);
        assert_eq!(m.running_cpus, 0);

        // Snapshot matches a full scan of the job map.
        let expected =
            JobMetricsSnapshot::collect(cm.get_jobs(&[], None, None, None, None, &[]).iter());
        assert_eq!(cm.job_metrics(), expected);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn node_metrics_track_lifecycle() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        assert_eq!(cm.node_metrics(), NodeMetricsSnapshot::default());

        register_node(&cm, "worker1", 8, 16000);
        register_node(&cm, "worker2", 8, 16000);

        let m = cm.node_metrics();
        assert_eq!(m.total, 2);
        assert_eq!(m.total_cpus, 16);
        assert_eq!(m.alloc_cpus, 0);
        assert_eq!(m.per_node.len(), 2);
        assert_eq!(m.per_node[0].name, "worker1");
        assert_eq!(m.per_node[1].name, "worker2");

        let job_id = submit_and_wait(&cm, basic_spec("node-metrics-job"));
        let resources = scalar_alloc(4, 8192);
        cm.start_job(
            job_id,
            vec!["worker1".into()],
            resources.clone(),
            per_node_for(&["worker1"], resources),
        )
        .unwrap();
        settle(&cm, job_id, JobState::Running);

        let m = cm.node_metrics();
        assert_eq!(m.alloc_cpus, 4);
        let w1 = m.per_node.iter().find(|n| n.name == "worker1").unwrap();
        assert_eq!(w1.alloc_cpus, 4);

        cm.complete_job(job_id, 0, JobState::Completed).unwrap();
        settle(&cm, job_id, JobState::Completed);

        let m = cm.node_metrics();
        assert_eq!(m.alloc_cpus, 0);

        // Snapshot matches a full scan of the node map.
        let expected = NodeMetricsSnapshot::collect(cm.get_nodes().iter());
        assert_eq!(cm.node_metrics(), expected);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_job() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        let job_id = submit_and_wait(&cm, basic_spec("cancel-me"));
        cm.cancel_job(job_id, "testuser").unwrap();
        settle(&cm, job_id, JobState::Cancelled);

        let job = cm.get_job(job_id).unwrap();
        assert_eq!(job.state, JobState::Cancelled);
    }

    // A cancelled job keeps its reported path (unlike requeue): the file was
    // created there, so scontrol should still point at it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_preserves_output_path() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 4, 8000);
        let id = submit_and_wait(&cm, basic_spec("cancel-me"));

        let alloc = scalar_alloc(2, 4000);
        cm.start_job(
            id,
            vec!["n1".into()],
            alloc.clone(),
            per_node_for(&["n1"], alloc),
        )
        .unwrap();
        settle(&cm, id, JobState::Running);

        cm.set_job_output_paths(id, "/tmp/spur.out".into(), "/tmp/spur.out".into());
        cm.cancel_job(id, "testuser").unwrap();
        settle(&cm, id, JobState::Cancelled);

        let job = cm.get_job(id).unwrap();
        assert_eq!(job.actual_stdout_path.as_deref(), Some("/tmp/spur.out"));
        assert_eq!(job.actual_stderr_path.as_deref(), Some("/tmp/spur.out"));
    }

    /// Drive a fresh job all the way to RUNNING on `node`, returning its id.
    fn run_job_on(cm: &ClusterManager, name: &str, node: &str) -> JobId {
        let job_id = submit_and_wait(cm, basic_spec(name));
        let resources = scalar_alloc(2, 4000);
        cm.start_job(
            job_id,
            vec![node.into()],
            resources.clone(),
            per_node_for(&[node], resources),
        )
        .unwrap();
        settle(cm, job_id, JobState::Running);
        job_id
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn preempt_job_requeue_returns_job_to_pending_and_frees_nodes() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "worker1", 8, 16000);

        // --requeue not set on the spec: requeue preempt mode must still
        // return the job to the queue rather than stranding it in PREEMPTED.
        let job_id = run_job_on(&cm, "preempt-requeue", "worker1");
        assert_eq!(cm.node_metrics().alloc_cpus, 2);

        let outcome = cm.preempt_job(job_id, PreemptMode::Requeue).unwrap();
        assert_eq!(outcome, PreemptOutcome::Killed);
        settle(&cm, job_id, JobState::Pending);

        let job = cm.get_job(job_id).unwrap();
        assert_eq!(job.state, JobState::Pending);
        assert!(job.allocated_nodes.is_empty());
        assert_eq!(cm.node_metrics().alloc_cpus, 0, "nodes must be freed");

        // The requeue must carry a future begin_time hold so the scheduler
        // cannot re-dispatch the job into its own in-flight preemption cancel,
        // and it must display BeginTime (Slurm parity).
        let begin = job
            .spec
            .begin_time
            .expect("requeue must set a begin_time hold");
        assert!(begin > Utc::now(), "begin_time hold must be in the future");
        assert_eq!(job.pending_reason, PendingReason::BeginTime);

        // While the hold is active the job is excluded from scheduling.
        assert!(
            !cm.pending_jobs().iter().any(|j| j.job_id == job_id),
            "held job must not be eligible for dispatch"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn preempt_requeue_preserves_later_user_begin() {
        // A user --begin further out than the preemption hold must win: the
        // requeue must not shorten the user's constraint.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "worker1", 8, 16000);

        let user_begin = Utc::now() + chrono::Duration::hours(1);
        let mut spec = basic_spec("user-begin");
        spec.begin_time = Some(user_begin);
        let job_id = submit_and_wait(&cm, spec);
        let resources = scalar_alloc(2, 4000);
        cm.start_job(
            job_id,
            vec!["worker1".into()],
            resources.clone(),
            per_node_for(&["worker1"], resources),
        )
        .unwrap();
        settle(&cm, job_id, JobState::Running);

        cm.preempt_job(job_id, PreemptMode::Requeue).unwrap();
        settle(&cm, job_id, JobState::Pending);

        assert_eq!(
            cm.get_job(job_id).unwrap().spec.begin_time,
            Some(user_begin),
            "user --begin beyond the hold must be preserved"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn preempt_requeue_hold_reason_survives_pending_reason_passes() {
        // The BeginTime hold reason must not be clobbered by the pending-reason
        // maintenance passes while the hold is still active.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "worker1", 8, 16000);

        let job_id = run_job_on(&cm, "hold-reason", "worker1");
        cm.preempt_job(job_id, PreemptMode::Requeue).unwrap();
        settle(&cm, job_id, JobState::Pending);
        assert_eq!(
            cm.get_job(job_id).unwrap().pending_reason,
            PendingReason::BeginTime
        );

        cm.tag_blocked_pending_reasons();
        assert_eq!(
            cm.get_job(job_id).unwrap().pending_reason,
            PendingReason::BeginTime,
            "tag_blocked_pending_reasons must not clobber an active BeginTime hold"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn preempt_requeue_hold_expires_and_job_reschedules() {
        // Once the hold lapses the job must become eligible and actually run
        // again — the hold must not permanently strand it.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "worker1", 8, 16000);

        let job_id = run_job_on(&cm, "hold-expiry", "worker1");
        cm.preempt_job(job_id, PreemptMode::Requeue).unwrap();
        settle(&cm, job_id, JobState::Pending);

        // Simulate the hold having elapsed by moving begin_time into the past,
        // exactly as the wall clock would after the hold window.
        {
            let mut jobs = cm.jobs.write();
            let job = jobs.get_mut(&job_id).unwrap();
            job.spec.begin_time = Some(Utc::now() - chrono::Duration::seconds(1));
        }

        assert!(
            cm.pending_jobs().iter().any(|j| j.job_id == job_id),
            "job must be eligible again once the hold lapses"
        );

        // And it can be started again (not stranded).
        let resources = scalar_alloc(2, 4000);
        cm.start_job(
            job_id,
            vec!["worker1".into()],
            resources.clone(),
            per_node_for(&["worker1"], resources),
        )
        .unwrap();
        settle(&cm, job_id, JobState::Running);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn preempt_job_cancel_terminates_and_frees_nodes() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "worker1", 8, 16000);

        let job_id = run_job_on(&cm, "preempt-cancel", "worker1");
        let outcome = cm.preempt_job(job_id, PreemptMode::Cancel).unwrap();
        assert_eq!(outcome, PreemptOutcome::Killed);
        settle(&cm, job_id, JobState::Cancelled);

        let job = cm.get_job(job_id).unwrap();
        assert_eq!(job.state, JobState::Cancelled);
        assert_eq!(cm.node_metrics().alloc_cpus, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn preempt_job_suspend_retains_allocation() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "worker1", 8, 16000);

        let job_id = run_job_on(&cm, "preempt-suspend", "worker1");
        let outcome = cm.preempt_job(job_id, PreemptMode::Suspend).unwrap();
        assert_eq!(outcome, PreemptOutcome::Suspended);
        settle(&cm, job_id, JobState::Suspended);

        // Suspend retains the allocation (the process is only SIGSTOP'd).
        assert_eq!(cm.node_metrics().alloc_cpus, 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn preempt_job_off_mode_is_rejected() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "worker1", 8, 16000);

        let job_id = run_job_on(&cm, "preempt-off", "worker1");
        assert!(cm.preempt_job(job_id, PreemptMode::Off).is_err());
        // Job keeps running; nothing was preempted.
        assert_eq!(cm.get_job(job_id).unwrap().state, JobState::Running);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn preempt_job_rejects_non_running() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        let job_id = submit_and_wait(&cm, basic_spec("still-pending"));
        assert!(cm.preempt_job(job_id, PreemptMode::Requeue).is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_job_state_change_idempotent_on_replay() {
        // WAL replay re-applies committed entries. A terminal job whose
        // completion entry is replayed must stay terminal without erroring or
        // re-running finalize side effects.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "worker1", 8, 16000);

        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(basic_spec("replay")),
        });
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id: 1,
            old_state: JobState::Pending,
            new_state: JobState::Running,
            pending_reason: None,
            pending_priority: None,
        });

        // Re-applying the same running transition is a NoOp, not an error.
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id: 1,
            old_state: JobState::Pending,
            new_state: JobState::Running,
            pending_reason: None,
            pending_priority: None,
        });
        assert_eq!(cm.get_job(1).unwrap().state, JobState::Running);

        let alloc = scalar_alloc(2, 4000);
        cm.apply_operation(&WalOperation::JobStart {
            job_id: 1,
            nodes: vec!["worker1".into()],
            resources: alloc.clone(),
            per_node_alloc: per_node_for(&["worker1"], alloc),
            srun_step_dispatch: false,
            run_attempt: 0,
        });
        cm.apply_operation(&WalOperation::JobComplete {
            job_id: 1,
            exit_code: 0,
            state: JobState::Completed,
        });

        // Replaying the terminal complete: still Completed, resources still freed.
        let replayed = cm.apply_operation(&WalOperation::JobComplete {
            job_id: 1,
            exit_code: 0,
            state: JobState::Completed,
        });
        assert!(replayed.jobs_finalized.is_empty());
        assert_eq!(cm.get_job(1).unwrap().state, JobState::Completed);
        assert_eq!(cm.get_node("worker1").unwrap().alloc_resources.cpus, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_requeue_state_change_not_double_counted_on_replay() {
        // A replayed Preempted->Pending entry must not double-increment
        // requeue_count or re-wipe allocation fields.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "worker1", 8, 16000);

        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(basic_spec("requeue-replay")),
        });
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id: 1,
            old_state: JobState::Pending,
            new_state: JobState::Running,
            pending_reason: None,
            pending_priority: None,
        });
        cm.apply_operation(&WalOperation::JobComplete {
            job_id: 1,
            exit_code: -1,
            state: JobState::Preempted,
        });
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id: 1,
            old_state: JobState::Preempted,
            new_state: JobState::Pending,
            pending_reason: None,
            pending_priority: None,
        });
        assert_eq!(cm.get_job(1).unwrap().requeue_count, 1);

        // Replay the same requeue transition (job already Pending): NoOp.
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id: 1,
            old_state: JobState::Preempted,
            new_state: JobState::Pending,
            pending_reason: None,
            pending_priority: None,
        });
        assert_eq!(
            cm.get_job(1).unwrap().requeue_count,
            1,
            "replayed requeue must not double-count"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_preempt_requeue_is_atomic_and_replay_deterministic() {
        // A single JobPreemptRequeue op takes a RUNNING job to Pending-with-hold
        // AND frees its nodes AND finalizes the prior run as PREEMPTED for
        // accounting — no intermediate state. Replay applies the exact begin_time
        // and is a NoOp (no double-count, no drift, no re-dealloc).
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "worker1", 8, 16000);

        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(basic_spec("preempt-replay")),
        });
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id: 1,
            old_state: JobState::Pending,
            new_state: JobState::Running,
            pending_reason: None,
            pending_priority: None,
        });
        let alloc = scalar_alloc(2, 4000);
        cm.apply_operation(&WalOperation::JobStart {
            job_id: 1,
            nodes: vec!["worker1".into()],
            resources: alloc.clone(),
            per_node_alloc: per_node_for(&["worker1"], alloc),
            srun_step_dispatch: false,
            run_attempt: 0,
        });
        assert_eq!(cm.get_node("worker1").unwrap().alloc_resources.cpus, 2);

        let begin_time = Utc::now() + chrono::Duration::seconds(5);
        let resp = cm.apply_operation(&WalOperation::JobPreemptRequeue {
            job_id: 1,
            begin_time,
        });
        // One op finalizes the prior run as PREEMPTED (drives accounting) ...
        assert_eq!(resp.jobs_finalized.len(), 1);
        assert_eq!(resp.jobs_finalized[0].state, JobState::Preempted);
        // ... and the job is Pending-with-hold with nodes freed.
        let job = cm.get_job(1).unwrap();
        assert_eq!(job.state, JobState::Pending);
        assert_eq!(job.spec.begin_time, Some(begin_time));
        assert_eq!(job.pending_reason, PendingReason::BeginTime);
        assert_eq!(job.preempt_requeue_count, 1);
        assert_eq!(job.requeue_count, 0);
        assert!(job.allocated_nodes.is_empty());
        assert_eq!(cm.get_node("worker1").unwrap().alloc_resources.cpus, 0);

        // Replay the identical entry: job is already Pending -> NoOp.
        let replay = cm.apply_operation(&WalOperation::JobPreemptRequeue {
            job_id: 1,
            begin_time,
        });
        assert!(
            replay.jobs_finalized.is_empty(),
            "replayed preempt-requeue must not re-finalize"
        );
        let job = cm.get_job(1).unwrap();
        assert_eq!(
            job.spec.begin_time,
            Some(begin_time),
            "instant must not drift"
        );
        assert_eq!(
            job.preempt_requeue_count, 1,
            "replayed preempt-requeue must not double-count"
        );
        assert_eq!(cm.get_node("worker1").unwrap().alloc_resources.cpus, 0);
    }

    /// Drive a job to RUNNING on `node` then finalize it as PREEMPTED via the
    /// apply path, returning its id. Mirrors the pre-fix WAL state that stranded
    /// jobs in PREEMPTED.
    fn preempted_job_on(cm: &ClusterManager, name: &str, node: &str) -> JobId {
        let job_id = run_job_on(cm, name, node);
        cm.apply_operation(&WalOperation::JobComplete {
            job_id,
            exit_code: -1,
            state: JobState::Preempted,
        });
        assert_eq!(cm.get_job(job_id).unwrap().state, JobState::Preempted);
        job_id
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_node_complete_on_preempted_job_is_noop() {
        // A late/replayed node-completion for an already-PREEMPTED job must not
        // force an illegal PREEMPTED -> COMPLETED finalize: no state change, no
        // JobFinalized side-effect.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "worker1", 8, 16000);

        let job_id = preempted_job_on(&cm, "late-nodecomplete", "worker1");

        let resp = cm.apply_operation(&WalOperation::JobNodeComplete {
            job_id,
            node_name: "worker1".into(),
            exit_code: 0,
            signal: 0,
        });
        assert!(
            resp.jobs_finalized.is_empty(),
            "stale node-complete must not re-finalize"
        );
        assert_eq!(cm.get_job(job_id).unwrap().state, JobState::Preempted);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_job_complete_on_preempted_job_is_noop() {
        // Replaying a terminal JobComplete over an already-PREEMPTED job is a
        // silent no-op (this is the WAL-replay case for jobs 75/177).
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "worker1", 8, 16000);

        let job_id = preempted_job_on(&cm, "replay-complete", "worker1");

        let resp = cm.apply_operation(&WalOperation::JobComplete {
            job_id,
            exit_code: 0,
            state: JobState::Completed,
        });
        assert!(
            resp.jobs_finalized.is_empty(),
            "replayed complete over PREEMPTED must not re-finalize"
        );
        assert_eq!(cm.get_job(job_id).unwrap().state, JobState::Preempted);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn deadline_job_transitions_pending_to_deadline_with_deadline_reason() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        let job_id = submit_and_wait(&cm, basic_spec("dl"));
        cm.deadline_job(job_id).unwrap();
        settle(&cm, job_id, JobState::Deadline);

        let job = cm.get_job(job_id).unwrap();
        assert_eq!(job.state, JobState::Deadline);
        assert_eq!(job.pending_reason, PendingReason::DeadLine);
        assert_eq!(job.exit_code, Some(-1));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn deadline_job_rejects_non_pending_states() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "worker1", 4, 8000);

        let job_id = submit_and_wait(&cm, basic_spec("running"));
        let resources = scalar_alloc(1, 1000);
        cm.start_job(
            job_id,
            vec!["worker1".into()],
            resources.clone(),
            per_node_for(&["worker1"], resources),
        )
        .unwrap();
        settle(&cm, job_id, JobState::Running);

        assert!(cm.deadline_job(job_id).is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn deadline_reason_survives_update_pending_reasons() {
        // Regression guard for the field bug: scheduler_loop fires the
        // deadline path while update_pending_reasons is also running each
        // tick. If the guard in update_pending_reasons regresses, the reason
        // gets clobbered to NodeDown/Resources just before the WAL apply,
        // and the user sees the wrong cause in any audit log.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        let job_id = submit_and_wait(&cm, basic_spec("dl-race"));

        // Manually mark DeadLine, then run update_pending_reasons over an
        // empty cluster_state (which would otherwise force Resources/NodeDown).
        {
            let mut jobs = cm.jobs.write();
            jobs.get_mut(&job_id).unwrap().pending_reason = PendingReason::DeadLine;
        }
        let empty_state = spur_sched::traits::ClusterState {
            nodes: &[],
            partitions: &[],
            reservations: &[],
            topology: None,
        };
        let snapshot = cm.get_job(job_id).unwrap();
        cm.update_pending_reasons(&[&snapshot], &empty_state);

        let job = cm.get_job(job_id).unwrap();
        assert_eq!(job.pending_reason, PendingReason::DeadLine);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fully_allocated_cluster_reports_resources_not_nodedown() {
        // Regression: a job waiting on a fully-busy cluster must report
        // Resources (matching Slurm), not NodeDown. An `Allocated` node is up,
        // just full; only genuine down/drain/error states are NodeDown.
        use spur_core::node::NodeState;
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 4, 8000);
        let job_id = submit_and_wait(&cm, basic_spec("busy"));
        let snapshot = cm.get_job(job_id).unwrap();

        // Fully-allocated (busy but UP) node -> Resources.
        let mut node = cm.get_node("n1").unwrap();
        node.state = NodeState::Allocated;
        node.alloc_resources = scalar_alloc(4, 8000);
        let nodes = vec![node];
        let state = spur_sched::traits::ClusterState {
            nodes: &nodes,
            partitions: &[],
            reservations: &[],
            topology: None,
        };
        cm.update_pending_reasons(&[&snapshot], &state);
        assert_eq!(
            cm.get_job(job_id).unwrap().pending_reason,
            PendingReason::Resources
        );

        // Genuinely down node -> NodeDown.
        let mut down = cm.get_node("n1").unwrap();
        down.state = NodeState::Down;
        let nodes = vec![down];
        let state = spur_sched::traits::ClusterState {
            nodes: &nodes,
            partitions: &[],
            reservations: &[],
            topology: None,
        };
        cm.update_pending_reasons(&[&snapshot], &state);
        assert_eq!(
            cm.get_job(job_id).unwrap().pending_reason,
            PendingReason::NodeDown
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nodelist_that_cannot_match_reports_req_node_not_avail() {
        // A job pinned to a node that isn't idle/usable must report
        // ReqNodeNotAvail, not Priority (as if merely queued behind others).
        use spur_core::node::NodeState;
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 4, 8000);
        register_node(&cm, "n2", 4, 8000);

        let mut spec = basic_spec("pinned");
        spec.nodelist = Some("n1".into());
        let job_id = submit_and_wait(&cm, spec);
        let snapshot = cm.get_job(job_id).unwrap();

        // n1 is drained (not schedulable); n2 is idle but excluded by nodelist.
        let mut n1 = cm.get_node("n1").unwrap();
        n1.state = NodeState::Drain;
        let n2 = cm.get_node("n2").unwrap();
        let nodes = vec![n1, n2];
        let state = spur_sched::traits::ClusterState {
            nodes: &nodes,
            partitions: &[],
            reservations: &[],
            topology: None,
        };
        cm.update_pending_reasons(&[&snapshot], &state);
        assert_eq!(
            cm.get_job(job_id).unwrap().pending_reason,
            PendingReason::ReqNodeNotAvail
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn additive_nodelist_fill_shortage_reports_resources() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 4, 8000);
        register_node(&cm, "n2", 4, 8000);
        register_node(&cm, "n3", 4, 8000);

        let mut spec = basic_spec("fill-shortage");
        spec.num_nodes = 3;
        spec.num_tasks = 3;
        spec.nodelist = Some("n1".into());
        spec.exclude = Some("n3".into());
        let job_id = submit_and_wait(&cm, spec);
        let snapshot = cm.get_job(job_id).unwrap();

        let nodes = vec![
            cm.get_node("n1").unwrap(),
            cm.get_node("n2").unwrap(),
            cm.get_node("n3").unwrap(),
        ];
        let state = spur_sched::traits::ClusterState {
            nodes: &nodes,
            partitions: &[],
            reservations: &[],
            topology: None,
        };
        cm.update_pending_reasons(&[&snapshot], &state);
        assert_eq!(
            cm.get_job(job_id).unwrap().pending_reason,
            PendingReason::Resources
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn additive_nodelist_unavailable_listed_node_reports_req_node_not_avail() {
        use spur_core::node::NodeState;
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 4, 8000);
        register_node(&cm, "n2", 4, 8000);
        register_node(&cm, "n3", 4, 8000);
        register_node(&cm, "n4", 4, 8000);

        let mut spec = basic_spec("listed-down");
        spec.num_nodes = 3;
        spec.num_tasks = 3;
        spec.nodelist = Some("n1".into());
        let job_id = submit_and_wait(&cm, spec);
        let snapshot = cm.get_job(job_id).unwrap();

        let mut n1 = cm.get_node("n1").unwrap();
        n1.state = NodeState::Drain;
        let nodes = vec![
            n1,
            cm.get_node("n2").unwrap(),
            cm.get_node("n3").unwrap(),
            cm.get_node("n4").unwrap(),
        ];
        let state = spur_sched::traits::ClusterState {
            nodes: &nodes,
            partitions: &[],
            reservations: &[],
            topology: None,
        };
        cm.update_pending_reasons(&[&snapshot], &state);
        assert_eq!(
            cm.get_job(job_id).unwrap().pending_reason,
            PendingReason::ReqNodeNotAvail
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn additive_nodelist_fill_blocked_by_maintenance_reports_reserved_maintenance() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 4, 8000);
        register_node(&cm, "n2", 4, 8000);
        register_node(&cm, "n3", 4, 8000);
        register_node(&cm, "n4", 4, 8000);

        let mut spec = basic_spec("fill-maintenance");
        spec.num_nodes = 3;
        spec.num_tasks = 3;
        spec.nodelist = Some("n1".into());
        let job_id = submit_and_wait(&cm, spec);
        let snapshot = cm.get_job(job_id).unwrap();

        let nodes = vec![
            cm.get_node("n1").unwrap(),
            cm.get_node("n2").unwrap(),
            cm.get_node("n3").unwrap(),
            cm.get_node("n4").unwrap(),
        ];
        let now = Utc::now();
        let reservations = vec![Reservation {
            name: "maintenance".into(),
            start_time: now + chrono::Duration::minutes(30),
            end_time: now + chrono::Duration::hours(2),
            nodes: vec!["n2".into(), "n3".into(), "n4".into()],
            accounts: Vec::new(),
            users: Vec::new(),
            flags: spur_core::reservation::ReservationFlags {
                maint: true,
                ..Default::default()
            },
            owner: String::new(),
        }];
        let state = spur_sched::traits::ClusterState {
            nodes: &nodes,
            partitions: &[],
            reservations: &reservations,
            topology: None,
        };
        cm.update_pending_reasons(&[&snapshot], &state);
        assert_eq!(
            cm.get_job(job_id).unwrap().pending_reason,
            PendingReason::ReservedMaintenance
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn additive_nodelist_listed_node_blocked_by_maintenance_reports_reserved_maintenance() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 4, 8000);
        register_node(&cm, "n2", 4, 8000);
        register_node(&cm, "n3", 4, 8000);
        register_node(&cm, "n4", 4, 8000);

        let mut spec = basic_spec("listed-maintenance");
        spec.num_nodes = 3;
        spec.num_tasks = 3;
        spec.nodelist = Some("n1".into());
        let job_id = submit_and_wait(&cm, spec);
        let snapshot = cm.get_job(job_id).unwrap();

        let nodes = vec![
            cm.get_node("n1").unwrap(),
            cm.get_node("n2").unwrap(),
            cm.get_node("n3").unwrap(),
            cm.get_node("n4").unwrap(),
        ];
        let now = Utc::now();
        let reservations = vec![Reservation {
            name: "maintenance".into(),
            start_time: now + chrono::Duration::minutes(30),
            end_time: now + chrono::Duration::hours(2),
            nodes: vec!["n1".into()],
            accounts: Vec::new(),
            users: Vec::new(),
            flags: spur_core::reservation::ReservationFlags {
                maint: true,
                ..Default::default()
            },
            owner: String::new(),
        }];
        let state = spur_sched::traits::ClusterState {
            nodes: &nodes,
            partitions: &[],
            reservations: &reservations,
            topology: None,
        };
        cm.update_pending_reasons(&[&snapshot], &state);
        assert_eq!(
            cm.get_job(job_id).unwrap().pending_reason,
            PendingReason::ReservedMaintenance
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn more_nodes_than_exist_reports_partition_node_limit() {
        // B-05: a job needing more nodes than the partition physically has must
        // report PartitionNodeLimit (Slurm parity, verified on slurm 25.11.6),
        // not Priority — it can never be scheduled by waiting.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 4, 8000);
        register_node(&cm, "n2", 4, 8000);

        let mut spec = basic_spec("toobig");
        spec.num_nodes = 3; // only 2 nodes exist
        spec.num_tasks = 3;
        let job_id = submit_and_wait(&cm, spec);
        let snapshot = cm.get_job(job_id).unwrap();

        let nodes = vec![cm.get_node("n1").unwrap(), cm.get_node("n2").unwrap()];
        let state = spur_sched::traits::ClusterState {
            nodes: &nodes,
            partitions: &[],
            reservations: &[],
            topology: None,
        };
        cm.update_pending_reasons(&[&snapshot], &state);
        assert_eq!(
            cm.get_job(job_id).unwrap().pending_reason,
            PendingReason::PartitionNodeLimit
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unmatchable_constraint_reports_bad_constraints() {
        // A --constraint no node carries can never schedule -> BadConstraints.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 4, 8000);

        let mut spec = basic_spec("feat");
        spec.constraint = Some("mi300x".into());
        let job_id = submit_and_wait(&cm, spec);
        let snapshot = cm.get_job(job_id).unwrap();

        // Node has no features.
        let nodes = vec![cm.get_node("n1").unwrap()];
        let state = spur_sched::traits::ClusterState {
            nodes: &nodes,
            partitions: &[],
            reservations: &[],
            topology: None,
        };
        cm.update_pending_reasons(&[&snapshot], &state);
        assert_eq!(
            cm.get_job(job_id).unwrap().pending_reason,
            PendingReason::BadConstraints
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tag_blocked_sets_partition_config_reason() {
        // Request exceeds partition max_nodes -> PartitionConfig, not Resources.
        let dir = TempDir::new().unwrap();
        let mut config = test_config();
        config.partitions[0].max_nodes = Some(1);
        let cm = test_cluster_with_config(&dir, config).await;

        let mut spec = basic_spec("toobig");
        spec.partition = Some("default".into());
        spec.num_nodes = 2;
        let job_id = submit_and_wait(&cm, spec);

        cm.tag_blocked_pending_reasons();
        assert_eq!(
            cm.get_job(job_id).unwrap().pending_reason,
            PendingReason::PartitionConfig
        );
        // pending_jobs() must agree: the job is dropped, not scheduled.
        assert!(
            !cm.pending_jobs().iter().any(|j| j.job_id == job_id),
            "structurally-unschedulable job must be dropped from scheduling"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tag_blocked_sets_partition_config_for_time_and_min_nodes() {
        // max_time and min_nodes are independent PartitionConfig triggers.
        let dir = TempDir::new().unwrap();
        let mut config = test_config();
        config.partitions[0].max_time = Some("00:10:00".into()); // 10 min cap
        config.partitions[0].min_nodes = 2;
        let cm = test_cluster_with_config(&dir, config).await;

        let mut over_time = basic_spec("overtime");
        over_time.partition = Some("default".into());
        over_time.time_limit = Some(chrono::Duration::hours(1));
        let t_id = submit_and_wait(&cm, over_time);

        let mut under_nodes = basic_spec("undernodes");
        under_nodes.partition = Some("default".into());
        under_nodes.num_nodes = 1; // below min_nodes=2
        let n_id = submit_and_wait(&cm, under_nodes);

        cm.tag_blocked_pending_reasons();
        assert_eq!(
            cm.get_job(t_id).unwrap().pending_reason,
            PendingReason::PartitionConfig,
            "time_limit over partition max_time -> PartitionConfig"
        );
        assert_eq!(
            cm.get_job(n_id).unwrap().pending_reason,
            PendingReason::PartitionConfig,
            "num_nodes below partition min_nodes -> PartitionConfig"
        );
        assert!(
            !cm.pending_jobs().iter().any(|j| j.job_id == t_id),
            "time-blocked job must be dropped from scheduling"
        );
        assert!(
            !cm.pending_jobs().iter().any(|j| j.job_id == n_id),
            "min_nodes-blocked job must be dropped from scheduling"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tag_blocked_sets_partition_inactive_when_not_up() {
        // Non-Up partition -> job admitted and held PENDING with PartitionInactive.
        let dir = TempDir::new().unwrap();
        let mut config = test_config();
        config.partitions[0].state = "DOWN".into();
        let cm = test_cluster_with_config(&dir, config).await;

        let mut spec = basic_spec("downpart");
        spec.partition = Some("default".into());
        let job_id = submit_and_wait(&cm, spec);

        cm.tag_blocked_pending_reasons();
        assert_eq!(
            cm.get_job(job_id).unwrap().pending_reason,
            PendingReason::PartitionInactive
        );
        assert!(
            !cm.pending_jobs().iter().any(|j| j.job_id == job_id),
            "job in a non-Up partition must be dropped from scheduling"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn multi_partition_job_reaches_scheduling_when_one_partition_is_eligible() {
        let dir = TempDir::new().unwrap();
        let mut config = test_config();
        config.partitions[0].state = "DOWN".into();
        config.partitions[0].max_nodes = Some(1);
        let mut batch = config.partitions[0].clone();
        batch.name = "batch".into();
        batch.default = false;
        batch.state = "UP".into();
        batch.max_nodes = Some(2);
        config.partitions.push(batch);
        let cm = test_cluster_with_config(&dir, config).await;

        let mut spec = basic_spec("multi-partition");
        spec.partition = Some("default, batch".into());
        spec.num_nodes = 2;
        spec.num_tasks = 2;
        let job_id = submit_and_wait(&cm, spec);

        assert!(cm.pending_jobs().iter().any(|job| job.job_id == job_id));
        assert_eq!(
            cm.get_job(job_id).unwrap().spec.partition.as_deref(),
            Some("default, batch")
        );
    }

    #[test]
    fn partition_block_rejects_mixed_known_and_unknown_partitions() {
        let job = Job::new(
            1,
            JobSpec {
                partition: Some("default,missing".into()),
                ..basic_spec("mixed-partitions")
            },
        );
        let partitions = vec![Partition {
            name: "default".into(),
            ..Default::default()
        }];

        assert_eq!(
            partition_block(&job, &partitions),
            Some(PendingReason::PartitionConfig)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submit_rejects_account_not_in_allow_accounts() {
        let dir = TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.partitions[0].allow_accounts = vec!["research".into(), "faculty".into()];
        let cm = test_cluster_with_config(&dir, cfg).await;
        cm.association_cache()
            .insert_association("testuser", "student");

        let mut spec = basic_spec("badacct");
        spec.account = Some("student".into());
        let err = cm.submit_job(spec).unwrap_err();
        assert_eq!(
            err,
            SubmitError::invalid("account 'student' not allowed on partition 'default'")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submit_accepts_account_in_allow_accounts() {
        let dir = TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.partitions[0].allow_accounts = vec!["research".into(), "faculty".into()];
        let cm = test_cluster_with_config(&dir, cfg).await;
        cm.association_cache()
            .insert_association("testuser", "research");

        let mut spec = basic_spec("goodacct");
        spec.account = Some("research".into());
        assert!(cm.submit_job(spec).is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submit_rejects_deny_accounts() {
        let dir = TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.partitions[0].deny_accounts = vec!["student".into()];
        let cm = test_cluster_with_config(&dir, cfg).await;
        cm.association_cache()
            .insert_association("testuser", "student");

        let mut spec = basic_spec("denied");
        spec.account = Some("student".into());
        let err = cm.submit_job(spec).unwrap_err();
        assert_eq!(
            err,
            SubmitError::invalid("account 'student' denied on partition 'default'")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submit_allow_accounts_uses_default_account_when_unset() {
        let dir = TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.partitions[0].allow_accounts = vec!["research".into()];
        let cm = test_cluster_with_config(&dir, cfg).await;
        cm.association_cache()
            .insert_default_account("testuser", "research");

        let spec = basic_spec("defaultacct");
        assert!(cm.submit_job(spec).is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submit_empty_partition_string_applies_default_and_enforces_allow_accounts() {
        let dir = TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.partitions[0].allow_accounts = vec!["research".into()];
        let cm = test_cluster_with_config(&dir, cfg).await;
        cm.association_cache()
            .insert_association("testuser", "student");

        let mut spec = basic_spec("emptypart");
        spec.partition = Some(String::new());
        spec.account = Some("student".into());
        let err = cm.submit_job(spec).unwrap_err();
        assert_eq!(
            err,
            SubmitError::invalid("account 'student' not allowed on partition 'default'")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submit_rejects_allow_accounts_when_account_unresolved() {
        let dir = TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.partitions[0].allow_accounts = vec!["research".into()];
        let cm = test_cluster_with_config(&dir, cfg).await;
        cm.association_cache().set_loaded_without_associations();

        let spec = basic_spec("noacct");
        let err = cm.submit_job(spec).unwrap_err();
        assert_eq!(
            err,
            SubmitError::invalid("no account for user 'testuser' on partition 'default'")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submit_rejects_deny_accounts_with_default_account() {
        let dir = TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.partitions[0].deny_accounts = vec!["student".into()];
        let cm = test_cluster_with_config(&dir, cfg).await;
        cm.association_cache()
            .insert_default_account("testuser", "student");

        let spec = basic_spec("denied-default");
        let err = cm.submit_job(spec).unwrap_err();
        assert_eq!(
            err,
            SubmitError::invalid("account 'student' denied on partition 'default'")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submit_rejects_spoofed_account_not_in_membership() {
        let dir = TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.partitions[0].allow_accounts = vec!["research".into()];
        let cm = test_cluster_with_config(&dir, cfg).await;
        cm.association_cache()
            .insert_association("testuser", "student-z");
        cm.association_cache()
            .insert_association("testuser", "student-a");

        let mut spec = basic_spec("spoof");
        spec.account = Some("research".into());
        let err = cm.submit_job(spec).unwrap_err();
        assert_eq!(
            err,
            SubmitError::invalid(
                "user 'testuser' is not associated with account 'research'. Accounts associated with this user: [student-a, student-z]."
            )
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submit_rejects_account_when_user_has_no_associations() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        cm.association_cache().set_loaded_without_associations();

        let mut spec = basic_spec("unassociated");
        spec.account = Some("research".into());
        let err = cm.submit_job(spec).unwrap_err();
        assert_eq!(
            err,
            SubmitError::invalid(
                "user 'testuser' has no account associations. Contact your cluster admin to run: sacctmgr add user name=testuser account=<account>"
            )
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submit_allow_accounts_ignores_deny_when_both_list_account() {
        let dir = TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.partitions[0].allow_accounts = vec!["research".into()];
        cfg.partitions[0].deny_accounts = vec!["research".into()];
        let cm = test_cluster_with_config(&dir, cfg).await;
        cm.association_cache()
            .insert_association("testuser", "research");

        let mut spec = basic_spec("overlap");
        spec.account = Some("research".into());
        assert!(cm.submit_job(spec).is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submit_skips_allow_accounts_when_association_cache_unavailable() {
        let dir = TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.partitions[0].allow_accounts = vec!["research".into()];
        let cm = test_cluster_with_config(&dir, cfg).await;

        let mut spec = basic_spec("nocache");
        spec.account = Some("student".into());
        assert!(cm.submit_job(spec).is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submit_still_rejects_nonexistent_partition() {
        // Unknown partition must still be rejected at submit, not held pending.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        let mut spec = basic_spec("badpart");
        spec.partition = Some("does-not-exist".into());
        assert!(
            cm.submit_job(spec).is_err(),
            "submitting to an unknown partition must error"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submit_rejects_nonexistent_partition_in_or_list() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        let mut spec = basic_spec("badpart");
        spec.partition = Some("default, does-not-exist".into());
        let err = cm.submit_job(spec).unwrap_err();
        assert_eq!(
            err,
            SubmitError::invalid("partition 'does-not-exist' not found")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submit_enforces_account_access_on_every_requested_partition() {
        let dir = TempDir::new().unwrap();
        let mut config = test_config();
        config.partitions[0].allow_accounts = vec!["research".into()];
        let mut restricted = config.partitions[0].clone();
        restricted.name = "restricted".into();
        restricted.default = false;
        restricted.allow_accounts = vec!["faculty".into()];
        config.partitions.push(restricted);
        let cm = test_cluster_with_config(&dir, config).await;
        cm.association_cache()
            .insert_association("testuser", "research");

        let mut spec = basic_spec("mixed-access");
        spec.partition = Some("default,restricted".into());
        spec.account = Some("research".into());
        let err = cm.submit_job(spec).unwrap_err();
        assert_eq!(
            err,
            SubmitError::invalid("account 'research' not allowed on partition 'restricted'")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submit_partition_not_found_returns_invalid_argument() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        let mut spec = basic_spec("badpart");
        spec.partition = Some("does-not-exist".into());
        let err = cm.submit_job(spec).unwrap_err();
        assert_eq!(
            err,
            SubmitError::invalid("partition 'does-not-exist' not found")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tag_blocked_sets_reservation_reason() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        let mut spec = basic_spec("resv");
        spec.reservation = Some("does-not-exist".into());
        let job_id = submit_and_wait(&cm, spec);

        cm.tag_blocked_pending_reasons();
        assert_eq!(
            cm.get_job(job_id).unwrap().pending_reason,
            PendingReason::Reservation
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reservation_overlap_rejected_without_flag() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        let now = chrono::Utc::now();
        let base = Reservation {
            name: String::new(),
            start_time: now,
            end_time: now + chrono::Duration::hours(2),
            nodes: vec!["n1".into()],
            accounts: Vec::new(),
            users: vec!["alice".into()],
            flags: Default::default(),
            owner: String::new(),
        };
        let mut r1 = base.clone();
        r1.name = "r1".into();
        cm.create_reservation(r1).unwrap();
        let mut r2 = base;
        r2.name = "r2".into();
        r2.start_time = now + chrono::Duration::hours(1);
        assert!(cm.create_reservation(r2).is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reservation_overlap_allowed_with_overlap_flag() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        let now = chrono::Utc::now();
        let base = Reservation {
            name: String::new(),
            start_time: now,
            end_time: now + chrono::Duration::hours(2),
            nodes: vec!["n1".into()],
            accounts: Vec::new(),
            users: vec!["alice".into()],
            flags: Default::default(),
            owner: String::new(),
        };
        let mut r1 = base.clone();
        r1.name = "r1".into();
        cm.create_reservation(r1).unwrap();
        let mut r2 = base;
        r2.name = "r2".into();
        r2.start_time = now + chrono::Duration::hours(1);
        r2.flags.overlap = true;
        assert!(cm.create_reservation(r2).is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_jobs_prioritize_active_reservation_targets() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        let now = chrono::Utc::now();
        cm.create_reservation(Reservation {
            name: "r1".into(),
            start_time: now - chrono::Duration::minutes(5),
            end_time: now + chrono::Duration::hours(2),
            nodes: vec!["n1".into()],
            accounts: Vec::new(),
            users: vec!["testuser".into()],
            flags: Default::default(),
            owner: String::new(),
        })
        .unwrap();

        let mut plain = basic_spec("plain");
        plain.priority = Some(5000);
        let plain_id = submit_and_wait(&cm, plain);

        let mut resv = basic_spec("resv");
        resv.priority = Some(1000);
        resv.reservation = Some("r1".into());
        let resv_id = submit_and_wait(&cm, resv);

        let pending = cm.pending_jobs();
        assert_eq!(pending.first().map(|j| j.job_id), Some(resv_id));
        assert!(pending.iter().any(|j| j.job_id == plain_id));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_jobs_applies_qos_priority_adjustment() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        cm.qos_cache().insert(Qos {
            name: "low".into(),
            priority: -500,
            ..Default::default()
        });
        cm.qos_cache().insert(Qos {
            name: "high".into(),
            priority: 5000,
            ..Default::default()
        });

        let mut low = basic_spec("low");
        low.priority = Some(1000);
        low.qos = Some("low".into());
        let low_id = submit_and_wait(&cm, low);

        let mut high = basic_spec("high");
        high.priority = Some(1000);
        high.qos = Some("high".into());
        let high_id = submit_and_wait(&cm, high);

        let pending = cm.pending_jobs();
        let low_priority = pending
            .iter()
            .find(|j| j.job_id == low_id)
            .unwrap()
            .priority;
        let high_priority = pending
            .iter()
            .find(|j| j.job_id == high_id)
            .unwrap()
            .priority;
        assert!(
            high_priority > low_priority,
            "high-QoS job ({high_priority}) should outrank low-QoS job ({low_priority}) \
             despite identical base priority"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_jobs_qos_ordering_holds_with_nonneutral_fairshare_and_age() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        cm.qos_cache().insert(Qos {
            name: "low".into(),
            priority: 100,
            ..Default::default()
        });
        cm.qos_cache().insert(Qos {
            name: "high".into(),
            priority: 5000,
            ..Default::default()
        });

        // Non-neutral fairshare/age on the low-QoS job would previously let
        // that unrelated boost amplify its QoS delta and outrank the high-QoS job.
        cm.fairshare_cache().set_for_test("low-user", "", 3.0);

        let mut low = basic_spec("low");
        low.user = "low-user".into();
        low.priority = Some(1000);
        low.qos = Some("low".into());
        let low_id = submit_and_wait(&cm, low);
        {
            let mut jobs = cm.jobs.write();
            jobs.get_mut(&low_id).unwrap().submit_time = Utc::now() - chrono::Duration::days(6);
        }

        let mut high = basic_spec("high");
        high.priority = Some(1000);
        high.qos = Some("high".into());
        let high_id = submit_and_wait(&cm, high);

        let pending = cm.pending_jobs();
        let low_priority = pending
            .iter()
            .find(|j| j.job_id == low_id)
            .unwrap()
            .priority;
        let high_priority = pending
            .iter()
            .find(|j| j.job_id == high_id)
            .unwrap()
            .priority;
        assert!(
            high_priority > low_priority,
            "high-QoS job ({high_priority}) should still outrank low-QoS job ({low_priority}) \
             once fairshare/age no longer amplify the QoS delta"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn current_effective_priority_multi_partition_uses_highest_priority_tier() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        {
            let mut partitions = cm.partitions.write();
            partitions.push(Partition {
                name: "low".into(),
                priority_tier: 1,
                ..Default::default()
            });
            partitions.push(Partition {
                name: "high".into(),
                priority_tier: 9,
                ..Default::default()
            });
        }

        // Built directly (not submitted) to isolate priority resolution from
        // unrelated eligibility filters like partition_block().
        let job = Job::new(
            1,
            JobSpec {
                partition: Some("low,high".into()),
                priority: Some(1000),
                ..basic_spec("multi")
            },
        );

        let priority =
            cm.current_effective_priority_with_qos(&job, &Qos::default(), &cm.get_partitions());
        assert_eq!(
            priority, 9000,
            "multi-partition job should use the highest matched priority_tier (9), \
             not fall back to 1 because \"low,high\" isn't an exact partition name"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resv_overrun_grace_delays_cancel() {
        let dir = TempDir::new().unwrap();
        let mut config = test_config();
        config.scheduler.resv_overrun_minutes = 30;
        let cm = test_cluster_with_config(&dir, config).await;
        register_node(&cm, "n1", 8, 16000);
        let now = chrono::Utc::now();
        cm.create_reservation(Reservation {
            name: "r1".into(),
            start_time: now - chrono::Duration::hours(1),
            end_time: now - chrono::Duration::minutes(5),
            nodes: vec!["n1".into()],
            accounts: Vec::new(),
            users: vec!["testuser".into()],
            flags: Default::default(),
            owner: String::new(),
        })
        .unwrap();

        let mut spec = basic_spec("resv-run");
        spec.reservation = Some("r1".into());
        let job_id = submit_and_wait(&cm, spec);
        let res = scalar_alloc(1, 1000);
        cm.start_job(
            job_id,
            vec!["n1".into()],
            res.clone(),
            per_node_for(&["n1"], res),
        )
        .unwrap();
        settle(&cm, job_id, JobState::Running);

        cm.enforce_reservation_end_times();
        assert_eq!(cm.get_job(job_id).unwrap().state, JobState::Running);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn preempt_skips_jobs_in_active_reservation_at_same_tier() {
        let dir = TempDir::new().unwrap();
        let mut config = test_config();
        config.partitions[0].preempt_mode = "cancel".into();
        let cm = test_cluster_with_config(&dir, config).await;
        register_node(&cm, "n1", 8, 16000);
        let now = chrono::Utc::now();
        cm.create_reservation(Reservation {
            name: "r1".into(),
            start_time: now - chrono::Duration::minutes(5),
            end_time: now + chrono::Duration::hours(2),
            nodes: vec!["n1".into()],
            accounts: Vec::new(),
            users: vec!["testuser".into()],
            flags: Default::default(),
            owner: String::new(),
        })
        .unwrap();

        let mut low = basic_spec("low");
        low.priority = Some(100);
        low.reservation = Some("r1".into());
        let low_id = submit_and_wait(&cm, low);
        let res = scalar_alloc(1, 1000);
        cm.start_job(
            low_id,
            vec!["n1".into()],
            res.clone(),
            per_node_for(&["n1"], res),
        )
        .unwrap();
        settle(&cm, low_id, JobState::Running);

        let mut high = basic_spec("high");
        high.priority = Some(10_000);
        let high_id = submit_and_wait(&cm, high);
        let high_job = cm.get_job(high_id).unwrap();
        let partitions = cm.get_partitions();

        crate::scheduler_loop::try_preempt(&cm, &partitions, &[&high_job]).await;
        assert_eq!(cm.get_job(low_id).unwrap().state, JobState::Running);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn preempt_skips_jobs_on_unrelated_nodes() {
        let dir = TempDir::new().unwrap();
        let mut config = test_config();
        config.partitions[0].preempt_mode = "cancel".into();
        let cm = test_cluster_with_config(&dir, config).await;
        register_node(&cm, "n1", 8, 16000);
        register_node(&cm, "n2", 8, 16000);

        let mut low = basic_spec("low");
        low.priority = Some(100);
        low.nodelist = Some("n2".into());
        let low_id = submit_and_wait(&cm, low);
        let res = scalar_alloc(1, 1000);
        cm.start_job(
            low_id,
            vec!["n2".into()],
            res.clone(),
            per_node_for(&["n2"], res),
        )
        .unwrap();
        settle(&cm, low_id, JobState::Running);

        let mut high = basic_spec("high");
        high.priority = Some(10_000);
        high.nodelist = Some("n1".into());
        let high_id = submit_and_wait(&cm, high);
        let high_job = cm.get_job(high_id).unwrap();
        let partitions = cm.get_partitions();

        crate::scheduler_loop::try_preempt(&cm, &partitions, &[&high_job]).await;
        assert_eq!(cm.get_job(low_id).unwrap().state, JobState::Running);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn preempt_triggers_when_qos_priority_differentiates_pending_job() {
        let dir = TempDir::new().unwrap();
        let mut config = test_config();
        config.partitions[0].preempt_mode = "cancel".into();
        let cm = test_cluster_with_config(&dir, config).await;
        register_node(&cm, "n1", 8, 16000);

        cm.qos_cache().insert(Qos {
            name: "high".into(),
            priority: 5000,
            ..Default::default()
        });

        // Same base priority on both jobs; only the QoS adjustment differentiates them.
        let mut low = basic_spec("low");
        low.priority = Some(1000);
        let low_id = submit_and_wait(&cm, low);
        let res = scalar_alloc(2, 4000);
        cm.start_job(
            low_id,
            vec!["n1".into()],
            res.clone(),
            per_node_for(&["n1"], res),
        )
        .unwrap();
        settle(&cm, low_id, JobState::Running);

        let mut high = basic_spec("high");
        high.priority = Some(1000);
        high.qos = Some("high".into());
        submit_and_wait(&cm, high);

        // pending_jobs() applies the QoS adjustment, unlike a synthetic Job.
        let pending = cm.pending_jobs();
        let pending_refs: Vec<&Job> = pending.iter().collect();
        let partitions = cm.get_partitions();
        crate::scheduler_loop::try_preempt(&cm, &partitions, &pending_refs).await;

        settle(&cm, low_id, JobState::Cancelled);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn preempt_uses_candidate_qos_preempt_mode_over_partition() {
        let dir = TempDir::new().unwrap();
        let mut config = test_config();
        // Partition says Cancel; the candidate's QoS overrides it to Suspend.
        config.partitions[0].preempt_mode = "cancel".into();
        let cm = test_cluster_with_config(&dir, config).await;
        register_node(&cm, "n1", 8, 16000);

        cm.qos_cache().insert(Qos {
            name: "suspend-me".into(),
            preempt_mode: spur_core::accounting::QosPreemptMode::Suspend,
            ..Default::default()
        });

        let mut low = basic_spec("low");
        low.priority = Some(100);
        low.qos = Some("suspend-me".into());
        let low_id = submit_and_wait(&cm, low);
        let res = scalar_alloc(2, 4000);
        cm.start_job(
            low_id,
            vec!["n1".into()],
            res.clone(),
            per_node_for(&["n1"], res),
        )
        .unwrap();
        settle(&cm, low_id, JobState::Running);

        let mut high = basic_spec("high");
        high.priority = Some(10_000);
        let high_id = submit_and_wait(&cm, high);
        let high_job = cm.get_job(high_id).unwrap();
        let partitions = cm.get_partitions();

        crate::scheduler_loop::try_preempt(&cm, &partitions, &[&high_job]).await;

        // Suspended, not Cancelled: proves the QoS override reached the real
        // preemption action, not just the pure job_preempt_mode() decision.
        settle(&cm, low_id, JobState::Suspended);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn purge_expired_holds_pending_reservation_jobs() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        let now = chrono::Utc::now();
        cm.apply_operation(&WalOperation::ReservationCreate {
            reservation: Reservation {
                name: "r1".into(),
                start_time: now - chrono::Duration::hours(2),
                end_time: now - chrono::Duration::minutes(1),
                nodes: vec!["n1".into()],
                accounts: Vec::new(),
                users: vec!["testuser".into()],
                flags: Default::default(),
                owner: String::new(),
            },
        });

        let mut spec = basic_spec("resv-pending");
        spec.reservation = Some("r1".into());
        let job_id = submit_and_wait(&cm, spec);

        cm.purge_expired_reservations();
        let job = cm.get_job(job_id).unwrap();
        assert_eq!(job.state, JobState::Pending);
        assert_eq!(job.pending_reason, PendingReason::ReservationDeleted);
        assert_eq!(job.priority, 0);
        assert!(cm.get_reservations().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_reservation_create_update_delete() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        let now = chrono::Utc::now();
        let res = Reservation {
            name: "r1".into(),
            start_time: now,
            end_time: now + chrono::Duration::hours(1),
            nodes: vec!["n1".into()],
            accounts: Vec::new(),
            users: vec!["alice".into()],
            flags: Default::default(),
            owner: String::new(),
        };
        cm.apply_operation(&WalOperation::ReservationCreate {
            reservation: res.clone(),
        });
        assert_eq!(cm.get_reservations().len(), 1);
        assert_eq!(cm.get_reservations()[0].name, "r1");

        cm.apply_operation(&WalOperation::ReservationUpdate {
            name: "r1".into(),
            duration_minutes: 120,
            add_nodes: Vec::new(),
            remove_nodes: Vec::new(),
            add_users: vec!["bob".into()],
            remove_users: Vec::new(),
            add_accounts: Vec::new(),
            remove_accounts: Vec::new(),
        });
        let updated = cm
            .get_reservations()
            .into_iter()
            .find(|r| r.name == "r1")
            .unwrap();
        assert!(updated.users.contains(&"bob".into()));
        assert_eq!(
            updated.end_time,
            updated.start_time + chrono::Duration::minutes(120)
        );

        cm.apply_operation(&WalOperation::ReservationDelete { name: "r1".into() });
        assert!(cm.get_reservations().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_reservation_create_idempotent() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        let now = chrono::Utc::now();
        let res = Reservation {
            name: "r1".into(),
            start_time: now,
            end_time: now + chrono::Duration::hours(1),
            nodes: vec!["n1".into()],
            accounts: Vec::new(),
            users: vec!["alice".into()],
            flags: Default::default(),
            owner: String::new(),
        };
        cm.apply_operation(&WalOperation::ReservationCreate {
            reservation: res.clone(),
        });
        cm.apply_operation(&WalOperation::ReservationCreate { reservation: res });
        assert_eq!(cm.get_reservations().len(), 1);
        assert_eq!(cm.get_reservations()[0].name, "r1");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_reservation_create_keeps_single_entry() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        let now = chrono::Utc::now();
        let base = Reservation {
            name: "r1".into(),
            start_time: now,
            end_time: now + chrono::Duration::hours(1),
            nodes: vec!["n1".into()],
            accounts: Vec::new(),
            users: vec!["alice".into()],
            flags: Default::default(),
            owner: String::new(),
        };
        let cm1 = cm.clone();
        let cm2 = cm.clone();
        let r1 = base.clone();
        let r2 = base;
        let (first, second) = tokio::join!(
            tokio::task::spawn_blocking(move || cm1.create_reservation(r1)),
            tokio::task::spawn_blocking(move || cm2.create_reservation(r2)),
        );
        let outcomes = [first.unwrap(), second.unwrap()];
        assert_eq!(
            outcomes.iter().filter(|r| r.is_ok()).count(),
            1,
            "exactly one concurrent create must succeed"
        );
        assert_eq!(cm.get_reservations().len(), 1);
        assert_eq!(cm.get_reservations()[0].name, "r1");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reservation_survives_wal_replay() {
        let dir = TempDir::new().unwrap();
        {
            let cm = test_cluster(&dir).await;
            register_node(&cm, "n1", 8, 16000);
            let now = chrono::Utc::now();
            cm.create_reservation(Reservation {
                name: "r1".into(),
                start_time: now,
                end_time: now + chrono::Duration::hours(1),
                nodes: vec!["n1".into()],
                accounts: Vec::new(),
                users: vec!["alice".into()],
                flags: Default::default(),
                owner: String::new(),
            })
            .unwrap();
            assert_eq!(cm.get_reservations().len(), 1);
        }

        let cm2 = test_cluster(&dir).await;
        wait_for("reservation replayed from WAL", || {
            cm2.get_reservations().iter().any(|r| r.name == "r1")
        });
        assert_eq!(cm2.get_reservations().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_hold_jobs_reservation_delete_does_not_permanently_block_job() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        let now = chrono::Utc::now();
        cm.create_reservation(Reservation {
            name: "r1".into(),
            start_time: now - chrono::Duration::minutes(5),
            end_time: now + chrono::Duration::hours(2),
            nodes: vec!["n1".into()],
            accounts: Vec::new(),
            users: vec!["testuser".into()],
            flags: spur_core::reservation::ReservationFlags {
                no_hold_jobs: true,
                ..Default::default()
            },
            owner: String::new(),
        })
        .unwrap();

        let mut spec = basic_spec("resv-pending");
        spec.reservation = Some("r1".into());
        let job_id = submit_and_wait(&cm, spec);

        cm.delete_reservation("r1", "root").unwrap();
        wait_for("reservation deleted", || cm.get_reservations().is_empty());

        let job = cm.get_job(job_id).unwrap();
        assert_eq!(job.state, JobState::Pending);
        assert_eq!(job.spec.reservation, None);
        assert_ne!(job.pending_reason, PendingReason::Held);
        assert_ne!(job.priority, 0);
        assert!(
            cm.pending_jobs().iter().any(|j| j.job_id == job_id),
            "job must remain schedulable after no_hold_jobs delete"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_reservation_rejects_non_owner() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        let now = chrono::Utc::now();
        cm.create_reservation(Reservation {
            name: "r1".into(),
            start_time: now - chrono::Duration::minutes(5),
            end_time: now + chrono::Duration::hours(2),
            nodes: vec!["n1".into()],
            accounts: Vec::new(),
            users: Vec::new(),
            flags: Default::default(),
            owner: "alice".into(),
        })
        .unwrap();

        let err = cm.delete_reservation("r1", "bob").unwrap_err();
        assert!(
            matches!(err, ReservationError::PermissionDenied(_)),
            "non-owner delete must be denied, got {err:?}"
        );
        assert_eq!(cm.get_reservations().len(), 1, "reservation must survive");

        cm.delete_reservation("r1", "alice").unwrap();
        assert!(
            cm.get_reservations().is_empty(),
            "owner delete must succeed"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_reservation_rejects_non_owner() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        let now = chrono::Utc::now();
        cm.create_reservation(Reservation {
            name: "r1".into(),
            start_time: now - chrono::Duration::minutes(5),
            end_time: now + chrono::Duration::hours(2),
            nodes: vec!["n1".into()],
            accounts: Vec::new(),
            users: Vec::new(),
            flags: Default::default(),
            owner: "alice".into(),
        })
        .unwrap();

        let err = cm
            .update_reservation("r1", 30, &[], &[], &[], &[], &[], &[], "bob")
            .unwrap_err();
        assert!(
            matches!(err, ReservationError::PermissionDenied(_)),
            "non-owner update must be denied, got {err:?}"
        );

        cm.update_reservation("r1", 30, &[], &[], &[], &[], &[], &[], "alice")
            .expect("owner update must succeed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_reservation_allows_root_and_unowned() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        let now = chrono::Utc::now();
        cm.create_reservation(Reservation {
            name: "owned".into(),
            start_time: now - chrono::Duration::minutes(5),
            end_time: now + chrono::Duration::hours(2),
            nodes: vec!["n1".into()],
            accounts: Vec::new(),
            users: Vec::new(),
            flags: Default::default(),
            owner: "alice".into(),
        })
        .unwrap();
        cm.create_reservation(Reservation {
            name: "legacy".into(),
            start_time: now - chrono::Duration::minutes(5),
            end_time: now + chrono::Duration::hours(2),
            nodes: vec!["n1".into()],
            accounts: Vec::new(),
            users: Vec::new(),
            flags: spur_core::reservation::ReservationFlags {
                overlap: true,
                ..Default::default()
            },
            owner: String::new(),
        })
        .unwrap();

        cm.delete_reservation("owned", "root")
            .expect("root may delete any reservation");
        cm.delete_reservation("legacy", "bob")
            .expect("unowned reservation stays manageable by anyone");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_hold_jobs_purge_does_not_permanently_block_job() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        let now = chrono::Utc::now();
        cm.apply_operation(&WalOperation::ReservationCreate {
            reservation: Reservation {
                name: "r1".into(),
                start_time: now - chrono::Duration::hours(2),
                end_time: now - chrono::Duration::minutes(1),
                nodes: vec!["n1".into()],
                accounts: Vec::new(),
                users: vec!["testuser".into()],
                flags: spur_core::reservation::ReservationFlags {
                    no_hold_jobs: true,
                    ..Default::default()
                },
                owner: String::new(),
            },
        });

        let mut spec = basic_spec("resv-pending");
        spec.reservation = Some("r1".into());
        let job_id = submit_and_wait(&cm, spec);

        cm.purge_expired_reservations();
        wait_for("reservation purged", || cm.get_reservations().is_empty());

        let job = cm.get_job(job_id).unwrap();
        assert_eq!(job.spec.reservation, None);
        assert!(
            cm.pending_jobs().iter().any(|j| j.job_id == job_id),
            "job must remain schedulable after no_hold_jobs purge"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reservation_fence_reason_requires_all_candidates_blocked() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        register_node(&cm, "n2", 8, 16000);
        let now = chrono::Utc::now();
        cm.create_reservation(Reservation {
            name: "r1".into(),
            start_time: now - chrono::Duration::minutes(5),
            end_time: now + chrono::Duration::hours(2),
            nodes: vec!["n1".into()],
            accounts: Vec::new(),
            users: vec!["alice".into()],
            flags: Default::default(),
            owner: String::new(),
        })
        .unwrap();

        let job_id = submit_and_wait(&cm, basic_spec("fence"));
        cm.tag_blocked_pending_reasons();
        let job = cm.get_job(job_id).unwrap();
        assert_ne!(
            job.pending_reason,
            PendingReason::ReqNodeNotAvail,
            "n2 is an unblocked candidate; must not fence the job"
        );
        assert_ne!(job.pending_reason, PendingReason::ReservedMaintenance);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn job_state_change_held_pending_apply_is_atomic() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        let job_id = submit_and_wait(&cm, basic_spec("hold-atomic"));
        cm.apply_operation(&WalOperation::job_state_change(
            job_id,
            JobState::Pending,
            JobState::Running,
        ));
        cm.apply_operation(&WalOperation::JobComplete {
            job_id,
            exit_code: -1,
            state: JobState::Preempted,
        });
        for _ in 0..5 {
            cm.apply_operation(&WalOperation::job_state_change(
                job_id,
                JobState::Preempted,
                JobState::Pending,
            ));
            cm.apply_operation(&WalOperation::job_state_change(
                job_id,
                JobState::Pending,
                JobState::Running,
            ));
            cm.apply_operation(&WalOperation::JobComplete {
                job_id,
                exit_code: -1,
                state: JobState::Preempted,
            });
        }
        assert_eq!(cm.get_job(job_id).unwrap().requeue_count, 5);

        cm.apply_operation(&WalOperation::job_state_change_held_pending(
            job_id,
            JobState::Preempted,
            PendingReason::JobHoldMaxRequeue,
        ));
        let job = cm.get_job(job_id).unwrap();
        assert_eq!(job.state, JobState::Pending);
        assert_eq!(job.priority, 0);
        assert_eq!(job.pending_reason, PendingReason::JobHoldMaxRequeue);
        assert!(
            !cm.pending_jobs().iter().any(|j| j.job_id == job_id),
            "hold must apply priority and reason in one WAL entry"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn requeue_preempted_job_holds_at_max_requeue() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        let job_id = submit_and_wait(&cm, basic_spec("preempt-me"));
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id,
            old_state: JobState::Pending,
            new_state: JobState::Running,
            pending_reason: None,
            pending_priority: None,
        });
        cm.apply_operation(&WalOperation::JobComplete {
            job_id,
            exit_code: -1,
            state: JobState::Preempted,
        });
        for _ in 0..5 {
            cm.apply_operation(&WalOperation::JobStateChange {
                job_id,
                old_state: JobState::Preempted,
                new_state: JobState::Pending,
                pending_reason: None,
                pending_priority: None,
            });
            cm.apply_operation(&WalOperation::JobStateChange {
                job_id,
                old_state: JobState::Pending,
                new_state: JobState::Running,
                pending_reason: None,
                pending_priority: None,
            });
            cm.apply_operation(&WalOperation::JobComplete {
                job_id,
                exit_code: -1,
                state: JobState::Preempted,
            });
        }
        assert_eq!(cm.get_job(job_id).unwrap().requeue_count, 5);

        cm.hold_job_at_max_requeue(job_id).unwrap();
        wait_for("job held at max requeue", || {
            cm.get_job(job_id).is_some_and(|j| {
                j.state == JobState::Pending && j.pending_reason == PendingReason::JobHoldMaxRequeue
            })
        });
        let job = cm.get_job(job_id).unwrap();
        assert_eq!(job.priority, 0);
        assert!(
            !cm.pending_jobs().iter().any(|j| j.job_id == job_id),
            "held job must not be schedulable"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reservation_delete_hold_survives_wal_replay() {
        let dir = TempDir::new().unwrap();
        {
            let cm = test_cluster(&dir).await;
            register_node(&cm, "n1", 8, 16000);
            let now = chrono::Utc::now();
            cm.create_reservation(Reservation {
                name: "r1".into(),
                start_time: now - chrono::Duration::minutes(5),
                end_time: now + chrono::Duration::hours(2),
                nodes: vec!["n1".into()],
                accounts: Vec::new(),
                users: vec!["testuser".into()],
                flags: Default::default(),
                owner: String::new(),
            })
            .unwrap();

            let mut spec = basic_spec("resv-hold");
            spec.reservation = Some("r1".into());
            let job_id = submit_and_wait(&cm, spec);
            cm.delete_reservation("r1", "root").unwrap();
            wait_for("job held after delete", || {
                cm.get_job(job_id).is_some_and(|j| {
                    j.pending_reason == PendingReason::ReservationDeleted && j.priority == 0
                })
            });
        }

        let cm2 = test_cluster(&dir).await;
        wait_for("held job replayed from WAL", || {
            cm2.jobs
                .read()
                .values()
                .any(|j| j.pending_reason == PendingReason::ReservationDeleted && j.priority == 0)
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reservation_delete_no_hold_survives_wal_replay() {
        let dir = TempDir::new().unwrap();
        let job_id;
        {
            let cm = test_cluster(&dir).await;
            register_node(&cm, "n1", 8, 16000);
            let now = chrono::Utc::now();
            cm.create_reservation(Reservation {
                name: "r1".into(),
                start_time: now - chrono::Duration::minutes(5),
                end_time: now + chrono::Duration::hours(2),
                nodes: vec!["n1".into()],
                accounts: Vec::new(),
                users: vec!["testuser".into()],
                flags: spur_core::reservation::ReservationFlags {
                    no_hold_jobs: true,
                    ..Default::default()
                },
                owner: String::new(),
            })
            .unwrap();

            let mut spec = basic_spec("resv-no-hold");
            spec.reservation = Some("r1".into());
            job_id = submit_and_wait(&cm, spec);
            cm.delete_reservation("r1", "root").unwrap();
            wait_for("reservation detached from job", || {
                cm.get_job(job_id)
                    .is_some_and(|j| j.spec.reservation.is_none() && j.priority > 0)
            });
        }

        let cm2 = test_cluster(&dir).await;
        wait_for("detached job replayed from WAL", || {
            cm2.get_job(job_id)
                .is_some_and(|j| j.spec.reservation.is_none() && j.priority > 0)
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn release_job_after_reservation_delete_unblocks_job() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        let now = chrono::Utc::now();
        cm.create_reservation(Reservation {
            name: "r1".into(),
            start_time: now - chrono::Duration::minutes(5),
            end_time: now + chrono::Duration::hours(2),
            nodes: vec!["n1".into()],
            accounts: Vec::new(),
            users: vec!["testuser".into()],
            flags: Default::default(),
            owner: String::new(),
        })
        .unwrap();

        let mut spec = basic_spec("release-me");
        spec.reservation = Some("r1".into());
        let job_id = submit_and_wait(&cm, spec);
        cm.delete_reservation("r1", "root").unwrap();
        wait_for("job held after delete", || {
            cm.get_job(job_id).is_some_and(|j| {
                j.pending_reason == PendingReason::ReservationDeleted && j.priority == 0
            })
        });
        assert!(
            !cm.pending_jobs().iter().any(|j| j.job_id == job_id),
            "held job must not be schedulable before release"
        );

        cm.release_job(job_id).unwrap();
        wait_for("job released after reservation delete", || {
            cm.get_job(job_id).is_some_and(|j| {
                j.spec.reservation.is_none()
                    && j.priority > 0
                    && j.pending_reason == PendingReason::Priority
            })
        });
        assert!(
            cm.pending_jobs().iter().any(|j| j.job_id == job_id),
            "released job must be schedulable"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tag_blocked_preserves_reservation_deleted_reason() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        let mut spec = basic_spec("resv-deleted");
        spec.reservation = Some("gone".into());
        let job_id = submit_and_wait(&cm, spec);
        {
            let mut jobs = cm.jobs.write();
            jobs.get_mut(&job_id).unwrap().pending_reason = PendingReason::ReservationDeleted;
            jobs.get_mut(&job_id).unwrap().priority = 0;
        }

        cm.tag_blocked_pending_reasons();
        assert_eq!(
            cm.get_job(job_id).unwrap().pending_reason,
            PendingReason::ReservationDeleted
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn preempt_requeue_never_holds_job_regardless_of_repeat_count() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "worker1", 8, 16000);
        let max = cm.config.controller.max_batch_requeue;

        let job_id = run_job_on(&cm, "chronic-preempt", "worker1");
        for _ in 0..(max + 3) {
            cm.preempt_job(job_id, PreemptMode::Requeue).unwrap();
            settle(&cm, job_id, JobState::Pending);
            {
                let mut jobs = cm.jobs.write();
                jobs.get_mut(&job_id).unwrap().spec.begin_time =
                    Some(Utc::now() - chrono::Duration::seconds(1));
            }
            let resources = scalar_alloc(2, 4000);
            cm.start_job(
                job_id,
                vec!["worker1".into()],
                resources.clone(),
                per_node_for(&["worker1"], resources),
            )
            .unwrap();
            settle(&cm, job_id, JobState::Running);
        }
        cm.preempt_job(job_id, PreemptMode::Requeue).unwrap();
        settle(&cm, job_id, JobState::Pending);

        let job = cm.get_job(job_id).unwrap();
        assert!(
            job.preempt_requeue_count > max,
            "job must have been preempted more times than max_batch_requeue"
        );
        assert_eq!(
            job.requeue_count, 0,
            "repeated preemption must not touch the failure-requeue counter"
        );
        assert_ne!(
            job.pending_reason,
            PendingReason::JobHoldMaxRequeue,
            "a chronically-preempted but otherwise healthy job must never be held"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn chronic_preemption_does_not_exhaust_failure_requeue_budget() {
        // A job preempted max_batch_requeue times must still get a full
        // failure-requeue budget: the two counters are independent.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "worker1", 8, 16000);
        let max = cm.config.controller.max_batch_requeue;

        let mut spec = basic_spec("chronic-then-timeout");
        spec.requeue = true;
        let job_id = submit_and_wait(&cm, spec);
        let resources = scalar_alloc(2, 4000);

        for _ in 0..max {
            cm.start_job(
                job_id,
                vec!["worker1".into()],
                resources.clone(),
                per_node_for(&["worker1"], resources.clone()),
            )
            .unwrap();
            settle(&cm, job_id, JobState::Running);
            cm.preempt_job(job_id, PreemptMode::Requeue).unwrap();
            settle(&cm, job_id, JobState::Pending);
            {
                let mut jobs = cm.jobs.write();
                jobs.get_mut(&job_id).unwrap().spec.begin_time =
                    Some(Utc::now() - chrono::Duration::seconds(1));
            }
        }
        assert_eq!(cm.get_job(job_id).unwrap().preempt_requeue_count, max);
        assert_eq!(cm.get_job(job_id).unwrap().requeue_count, 0);

        cm.start_job(
            job_id,
            vec!["worker1".into()],
            resources.clone(),
            per_node_for(&["worker1"], resources),
        )
        .unwrap();
        settle(&cm, job_id, JobState::Running);
        cm.complete_job(job_id, -1, JobState::Timeout).unwrap();

        settle(&cm, job_id, JobState::Pending);
        let job = cm.get_job(job_id).unwrap();
        assert_eq!(
            job.requeue_count, 1,
            "the first genuine failure must count, unaffected by prior preemptions"
        );
        assert_ne!(
            job.pending_reason,
            PendingReason::JobHoldMaxRequeue,
            "one timeout after chronic preemption must not exhaust the failure budget"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn maybe_requeue_holds_at_max_for_timeout() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);

        let mut spec = basic_spec("timeout-requeue");
        spec.requeue = true;
        let job_id = submit_and_wait(&cm, spec);
        let alloc = scalar_alloc(1, 1000);
        let nodes = vec!["n1".into()];
        let per_node = per_node_for(&["n1"], alloc.clone());

        for _ in 0..5 {
            cm.start_job(job_id, nodes.clone(), alloc.clone(), per_node.clone())
                .unwrap();
            settle(&cm, job_id, JobState::Running);
            cm.complete_job(job_id, -1, JobState::Timeout).unwrap();
            settle(&cm, job_id, JobState::Pending);
        }
        assert_eq!(cm.get_job(job_id).unwrap().requeue_count, 5);

        cm.start_job(job_id, nodes, alloc, per_node).unwrap();
        settle(&cm, job_id, JobState::Running);
        cm.complete_job(job_id, -1, JobState::Timeout).unwrap();
        wait_for("job held at max requeue after timeout", || {
            cm.get_job(job_id).is_some_and(|j| {
                j.state == JobState::Pending && j.pending_reason == PendingReason::JobHoldMaxRequeue
            })
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn release_job_resets_requeue_count_after_max_hold() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        let job_id = submit_and_wait(&cm, basic_spec("release-me"));
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id,
            old_state: JobState::Pending,
            new_state: JobState::Running,
            pending_reason: None,
            pending_priority: None,
        });
        cm.apply_operation(&WalOperation::JobComplete {
            job_id,
            exit_code: -1,
            state: JobState::Preempted,
        });
        for _ in 0..5 {
            cm.apply_operation(&WalOperation::JobStateChange {
                job_id,
                old_state: JobState::Preempted,
                new_state: JobState::Pending,
                pending_reason: None,
                pending_priority: None,
            });
            cm.apply_operation(&WalOperation::JobStateChange {
                job_id,
                old_state: JobState::Pending,
                new_state: JobState::Running,
                pending_reason: None,
                pending_priority: None,
            });
            cm.apply_operation(&WalOperation::JobComplete {
                job_id,
                exit_code: -1,
                state: JobState::Preempted,
            });
        }
        cm.hold_job_at_max_requeue(job_id).unwrap();
        wait_for("job held at max requeue", || {
            cm.get_job(job_id)
                .is_some_and(|j| j.pending_reason == PendingReason::JobHoldMaxRequeue)
        });
        assert_eq!(cm.get_job(job_id).unwrap().requeue_count, 5);

        cm.release_job(job_id).unwrap();
        wait_for("release resets requeue budget", || {
            cm.get_job(job_id).is_some_and(|j| {
                j.requeue_count == 0
                    && j.priority > 0
                    && j.pending_reason == PendingReason::Priority
            })
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_preempted_job_uses_cancelled_state() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        let job_id = submit_and_wait(&cm, basic_spec("cancel-preempted"));

        cm.apply_operation(&WalOperation::JobStateChange {
            job_id,
            old_state: JobState::Pending,
            new_state: JobState::Running,
            pending_reason: None,
            pending_priority: None,
        });
        cm.apply_operation(&WalOperation::JobComplete {
            job_id,
            exit_code: -1,
            state: JobState::Preempted,
        });
        assert_eq!(cm.get_job(job_id).unwrap().state, JobState::Preempted);

        cm.cancel_job(job_id, "testuser").unwrap();
        wait_for("preempted job cancelled", || {
            cm.get_job(job_id)
                .is_some_and(|j| j.state == JobState::Cancelled)
        });
        let job = cm.get_job(job_id).unwrap();
        assert_ne!(job.pending_reason, PendingReason::JobHoldMaxRequeue);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tag_blocked_sets_licenses_reason() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        let mut spec = basic_spec("lic");
        // Request a license with an empty cluster pool -> shortfall.
        spec.gres = vec!["license:flexlm:1".into()];
        let job_id = submit_and_wait(&cm, spec);

        cm.tag_blocked_pending_reasons();
        assert_eq!(
            cm.get_job(job_id).unwrap().pending_reason,
            PendingReason::Licenses
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tag_blocked_sets_qos_reason_from_cache() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        // Seed the cache with a QoS that caps wall time at 1 min, then submit a
        // job to that QoS asking for more — the specific QOS reason must surface
        // through resolve_qos -> qos_block_for (not the old limitless default).
        cm.qos_cache().insert(Qos {
            name: "short".into(),
            limits: spur_core::accounting::QosLimits {
                max_wall_minutes: Some(1),
                ..Default::default()
            },
            ..Default::default()
        });
        let mut spec = basic_spec("qos");
        spec.qos = Some("short".into());
        spec.time_limit = Some(chrono::Duration::hours(1));
        let job_id = submit_and_wait(&cm, spec);

        cm.tag_blocked_pending_reasons();
        assert_eq!(
            cm.get_job(job_id).unwrap().pending_reason,
            PendingReason::QosMaxWallDurationPerJobLimit
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tag_blocked_sets_qos_grp_cpu_reason() {
        // GrpCPU aggregates across all running jobs in the QOS: a running 4-cpu
        // job fills a grp_tres cpu=4 cap, so the next job in the same QOS blocks
        // with QOSGrpCpuLimit (the group reason, not the per-job/per-user one).
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        let mut grp = TresRecord::new();
        grp.set(TresType::Cpu, 4);
        cm.qos_cache().insert(Qos {
            name: "grp".into(),
            limits: spur_core::accounting::QosLimits {
                grp_tres: Some(grp),
                ..Default::default()
            },
            ..Default::default()
        });

        let mut s1 = basic_spec("g1");
        s1.qos = Some("grp".into());
        s1.num_tasks = 4;
        let j1 = submit_and_wait(&cm, s1);
        let res = scalar_alloc(4, 1000);
        cm.start_job(
            j1,
            vec!["n1".into()],
            res.clone(),
            per_node_for(&["n1"], res),
        )
        .unwrap();
        settle(&cm, j1, JobState::Running);

        let mut s2 = basic_spec("g2");
        s2.qos = Some("grp".into());
        s2.num_tasks = 1;
        let j2 = submit_and_wait(&cm, s2);

        cm.tag_blocked_pending_reasons();
        assert_eq!(
            cm.get_job(j2).unwrap().pending_reason,
            PendingReason::QosGrpCpuLimit
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn association_default_qos_reaches_real_enforcement() {
        // A default isn't just cosmetic: it's subject to the QOS's limits
        // exactly as if --qos had been passed explicitly.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        cm.qos_cache().insert(Qos {
            name: "highprio".into(),
            limits: spur_core::accounting::QosLimits {
                max_jobs_per_user: Some(1),
                ..Default::default()
            },
            ..Default::default()
        });
        cm.association_cache()
            .insert_default_qos("testuser", "research", "highprio");

        let mut s1 = basic_spec("h1");
        s1.account = Some("research".into());
        let j1 = submit_and_wait(&cm, s1);
        // The default was resolved and baked into the job at submission —
        // not merely applied ephemerally during scheduling.
        assert_eq!(
            cm.get_job(j1).unwrap().spec.qos.as_deref(),
            Some("highprio")
        );

        let res = scalar_alloc(1, 1000);
        cm.start_job(
            j1,
            vec!["n1".into()],
            res.clone(),
            per_node_for(&["n1"], res),
        )
        .unwrap();
        settle(&cm, j1, JobState::Running);

        let mut s2 = basic_spec("h2");
        s2.account = Some("research".into());
        let j2 = submit_and_wait(&cm, s2);
        assert_eq!(
            cm.get_job(j2).unwrap().spec.qos.as_deref(),
            Some("highprio")
        );

        cm.tag_blocked_pending_reasons();
        assert_eq!(
            cm.get_job(j2).unwrap().pending_reason,
            PendingReason::QoSMaxJobsPerUser
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cluster_default_qos_reaches_real_enforcement() {
        // A cluster fallback QOS must bind a no-qos job and its limits must
        // actually block a second job — end to end.
        let dir = TempDir::new().unwrap();
        let mut config = test_config();
        config.accounting.default_qos = "normal".into();
        let cm = test_cluster_with_config(&dir, config).await;
        register_node(&cm, "n1", 8, 16000);
        cm.qos_cache().insert(Qos {
            name: "normal".into(),
            limits: spur_core::accounting::QosLimits {
                max_jobs_per_user: Some(1),
                ..Default::default()
            },
            ..Default::default()
        });

        let j1 = submit_and_wait(&cm, basic_spec("d1"));
        assert_eq!(
            cm.get_job(j1).unwrap().spec.qos.as_deref(),
            Some("normal"),
            "no-qos job must be bound to the cluster fallback at submit"
        );
        let res = scalar_alloc(1, 1000);
        cm.start_job(
            j1,
            vec!["n1".into()],
            res.clone(),
            per_node_for(&["n1"], res),
        )
        .unwrap();
        settle(&cm, j1, JobState::Running);

        let j2 = submit_and_wait(&cm, basic_spec("d2"));
        cm.tag_blocked_pending_reasons();
        assert_eq!(
            cm.get_job(j2).unwrap().pending_reason,
            PendingReason::QoSMaxJobsPerUser,
            "the fallback QOS's limits must actually enforce"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tag_blocked_sets_account_max_jobs_reason() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        cm.association_cache().insert_limits(
            "testuser",
            "research",
            spur_core::accounting::AccountLimits {
                max_running_jobs: Some(1),
                ..Default::default()
            },
        );

        let mut s1 = basic_spec("a1");
        s1.account = Some("research".into());
        let j1 = submit_and_wait(&cm, s1);
        let res = scalar_alloc(1, 1000);
        cm.start_job(
            j1,
            vec!["n1".into()],
            res.clone(),
            per_node_for(&["n1"], res),
        )
        .unwrap();
        settle(&cm, j1, JobState::Running);

        let mut s2 = basic_spec("a2");
        s2.account = Some("research".into());
        let j2 = submit_and_wait(&cm, s2);

        cm.tag_blocked_pending_reasons();
        assert_eq!(
            cm.get_job(j2).unwrap().pending_reason,
            PendingReason::AssocMaxJobsLimit
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tag_blocked_sets_account_max_submit_jobs_reason() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        cm.association_cache().insert_limits(
            "testuser",
            "research",
            spur_core::accounting::AccountLimits {
                max_submit_jobs: Some(1),
                ..Default::default()
            },
        );

        let mut s1 = basic_spec("b1");
        s1.account = Some("research".into());
        let j1 = submit_and_wait(&cm, s1);

        let mut s2 = basic_spec("b2");
        s2.account = Some("research".into());
        let j2 = submit_and_wait(&cm, s2);

        cm.tag_blocked_pending_reasons();
        // j1 alone is within the limit (max_submit_jobs=1) and must not be
        // blocked by counting itself; only j2, which pushes the count over
        // the cap, should be blocked.
        assert_eq!(cm.get_job(j1).unwrap().pending_reason, PendingReason::None);
        assert_eq!(
            cm.get_job(j2).unwrap().pending_reason,
            PendingReason::AssocMaxSubmitJobLimit
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tag_blocked_sets_qos_max_submit_jobs_reason() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        cm.qos_cache().insert(Qos {
            name: "capped".into(),
            limits: spur_core::accounting::QosLimits {
                max_submit_jobs_per_user: Some(1),
                ..Default::default()
            },
            ..Default::default()
        });

        let mut s1 = basic_spec("c1");
        s1.qos = Some("capped".into());
        let j1 = submit_and_wait(&cm, s1);

        let mut s2 = basic_spec("c2");
        s2.qos = Some("capped".into());
        let j2 = submit_and_wait(&cm, s2);

        cm.tag_blocked_pending_reasons();
        // j1 alone is within the limit (max_submit_jobs_per_user=1) and must
        // not be blocked by counting itself; only j2, which pushes the count
        // over the cap, should be blocked.
        assert_eq!(cm.get_job(j1).unwrap().pending_reason, PendingReason::None);
        assert_eq!(
            cm.get_job(j2).unwrap().pending_reason,
            PendingReason::QosMaxSubmitJobPerUserLimit
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tag_blocked_sets_account_grp_cpu_reason_across_users() {
        // GrpTRES aggregates across every user in the account, not just the
        // requester: a different user's running job fills the cap so the next
        // job in the same account blocks with AssocGrpCpuLimit.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        let mut grp = TresRecord::new();
        grp.set(TresType::Cpu, 4);
        cm.association_cache().insert_limits(
            "alice",
            "research",
            spur_core::accounting::AccountLimits {
                grp_tres: Some(grp.clone()),
                ..Default::default()
            },
        );
        cm.association_cache().insert_limits(
            "bob",
            "research",
            spur_core::accounting::AccountLimits {
                grp_tres: Some(grp),
                ..Default::default()
            },
        );

        let mut s1 = basic_spec("c1");
        s1.user = "alice".into();
        s1.account = Some("research".into());
        s1.num_tasks = 4;
        let j1 = submit_and_wait(&cm, s1);
        let res = scalar_alloc(4, 1000);
        cm.start_job(
            j1,
            vec!["n1".into()],
            res.clone(),
            per_node_for(&["n1"], res),
        )
        .unwrap();
        settle(&cm, j1, JobState::Running);

        let mut s2 = basic_spec("c2");
        s2.user = "bob".into();
        s2.account = Some("research".into());
        s2.num_tasks = 1;
        let j2 = submit_and_wait(&cm, s2);

        cm.tag_blocked_pending_reasons();
        assert_eq!(
            cm.get_job(j2).unwrap().pending_reason,
            PendingReason::AssocGrpCpuLimit
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn account_limits_do_not_block_jobs_without_an_account() {
        // A job with no account can't be constrained by an association it
        // doesn't belong to.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        cm.association_cache().insert_limits(
            "testuser",
            "research",
            spur_core::accounting::AccountLimits {
                max_running_jobs: Some(0),
                ..Default::default()
            },
        );

        let spec = basic_spec("d1");
        let job_id = submit_and_wait(&cm, spec);

        cm.tag_blocked_pending_reasons();
        assert_ne!(
            cm.get_job(job_id).unwrap().pending_reason,
            PendingReason::AssocMaxJobsLimit
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn running_job_license_consumption_blocks_next_job() {
        // Concurrent license accounting: a running job holding all of a license
        // must make a second job requesting that license ineligible, even though
        // each request alone is within the configured total.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        cm.license_pool.write().insert("fluent".into(), 2);

        let mut s1 = basic_spec("j1");
        s1.gres = vec!["license:fluent:2".into()];
        let j1 = submit_and_wait(&cm, s1);
        let res = scalar_alloc(1, 1000);
        cm.start_job(
            j1,
            vec!["n1".into()],
            res.clone(),
            per_node_for(&["n1"], res),
        )
        .unwrap();
        settle(&cm, j1, JobState::Running);

        assert_eq!(
            cm.available_licenses().get("fluent").copied(),
            Some(0),
            "running job's licenses should count as in use (none available)"
        );

        let mut s2 = basic_spec("j2");
        s2.gres = vec!["license:fluent:1".into()];
        let j2 = submit_and_wait(&cm, s2);
        let pending: Vec<JobId> = cm.pending_jobs().iter().map(|j| j.job_id).collect();
        assert!(
            !pending.contains(&j2),
            "j2 must be blocked while the license pool is exhausted"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_job_frees_its_licenses_without_drifting_total() {
        // Derived accounting: a job releases its licenses the moment it leaves the
        // active set, and the configured total is never mutated (no drift).
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        cm.license_pool.write().insert("fluent".into(), 2);

        let mut s = basic_spec("j");
        s.gres = vec!["license:fluent:2".into()];
        let id = submit_and_wait(&cm, s);
        let res = scalar_alloc(1, 1000);
        cm.start_job(
            id,
            vec!["n1".into()],
            res.clone(),
            per_node_for(&["n1"], res),
        )
        .unwrap();
        settle(&cm, id, JobState::Running);
        assert_eq!(cm.available_licenses().get("fluent").copied(), Some(0));

        cm.cancel_job(id, "testuser").unwrap();
        settle(&cm, id, JobState::Cancelled);
        assert_eq!(
            cm.available_licenses().get("fluent").copied(),
            Some(2),
            "licenses must be freed when the job leaves the active set"
        );
        assert_eq!(
            *cm.license_pool.read().get("fluent").unwrap(),
            2,
            "configured total must never be mutated"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_jobs_does_not_overallocate_licenses_within_one_pass() {
        // Two pending jobs each request fluent:1 but the pool holds only 1.
        // A single pending_jobs() pass must not return both — otherwise the
        // scheduler would allocate both and over-subscribe the license.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        cm.license_pool.write().insert("fluent".into(), 1);

        let mut s1 = basic_spec("a");
        s1.gres = vec!["license:fluent:1".into()];
        let a = submit_and_wait(&cm, s1);
        let mut s2 = basic_spec("b");
        s2.gres = vec!["license:fluent:1".into()];
        let b = submit_and_wait(&cm, s2);

        let pending: Vec<JobId> = cm.pending_jobs().iter().map(|j| j.job_id).collect();
        let granted = [a, b].iter().filter(|id| pending.contains(id)).count();
        assert_eq!(
            granted, 1,
            "pending_jobs() returned {granted} fluent jobs but the pool holds only 1"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_jobs_does_not_oversubscribe_qos_grp_node_within_one_pass() {
        // Three pending 1-node jobs share a QOS capped at grp_tres node=2. Each
        // fits against the empty running total, but a single pass must admit at
        // most 2 or the QOSGrpNodeLimit is over-subscribed.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 64, 128000);
        let mut grp = TresRecord::new();
        grp.set(TresType::Node, 2);
        cm.qos_cache().insert(Qos {
            name: "burst".into(),
            limits: spur_core::accounting::QosLimits {
                grp_tres: Some(grp),
                ..Default::default()
            },
            ..Default::default()
        });

        let ids: Vec<JobId> = (0..3)
            .map(|i| {
                let mut s = basic_spec(&format!("b{i}"));
                s.qos = Some("burst".into());
                submit_and_wait(&cm, s)
            })
            .collect();

        let pending: Vec<JobId> = cm.pending_jobs().iter().map(|j| j.job_id).collect();
        let granted = ids.iter().filter(|id| pending.contains(id)).count();
        assert_eq!(
            granted, 2,
            "pending_jobs() returned {granted} burst jobs but grp_tres node=2 allows 2"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_jobs_qos_grp_reserves_highest_priority_first() {
        // A high-priority 2-node job and a low-priority 1-node job share a QOS
        // capped at grp_tres node=2. The pass must keep the high-priority job
        // (which alone fills the cap) and drop the low-priority one, not admit
        // the small job first and starve the large one.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 64, 128000);
        let mut grp = TresRecord::new();
        grp.set(TresType::Node, 2);
        cm.qos_cache().insert(Qos {
            name: "burst".into(),
            limits: spur_core::accounting::QosLimits {
                grp_tres: Some(grp),
                ..Default::default()
            },
            ..Default::default()
        });

        let mut big = basic_spec("big");
        big.qos = Some("burst".into());
        big.num_nodes = 2;
        big.priority = Some(1000);
        let big_id = submit_and_wait(&cm, big);

        let mut small = basic_spec("small");
        small.qos = Some("burst".into());
        small.num_nodes = 1;
        small.priority = Some(1);
        let small_id = submit_and_wait(&cm, small);

        let pending: Vec<JobId> = cm.pending_jobs().iter().map(|j| j.job_id).collect();
        assert!(
            pending.contains(&big_id),
            "the highest-priority job must win the group reservation"
        );
        assert!(
            !pending.contains(&small_id),
            "the low-priority job must not consume headroom the big job needs"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_jobs_does_not_oversubscribe_account_grp_node_within_one_pass() {
        // Same one-pass over-subscription guard, one layer up: an account
        // association capped at grp_tres node=2 must not admit 3 pending 1-node
        // jobs in a single pass.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 64, 128000);
        let mut grp = TresRecord::new();
        grp.set(TresType::Node, 2);
        cm.association_cache().insert_limits(
            "testuser",
            "research",
            spur_core::accounting::AccountLimits {
                grp_tres: Some(grp),
                ..Default::default()
            },
        );

        let ids: Vec<JobId> = (0..3)
            .map(|i| {
                let mut s = basic_spec(&format!("a{i}"));
                s.account = Some("research".into());
                submit_and_wait(&cm, s)
            })
            .collect();

        let pending: Vec<JobId> = cm.pending_jobs().iter().map(|j| j.job_id).collect();
        let granted = ids.iter().filter(|id| pending.contains(id)).count();
        assert_eq!(
            granted, 2,
            "pending_jobs() returned {granted} account jobs but grp_tres node=2 allows 2"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_jobs_does_not_oversubscribe_account_max_running_jobs_within_one_pass() {
        // Same guard as the QOS max_jobs_per_user test, one layer up: an account
        // association capped at max_running_jobs=2 must not admit 3 pending jobs
        // from one user in a single pass even though none is running yet.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 64, 128000);
        cm.association_cache().insert_limits(
            "testuser",
            "research",
            spur_core::accounting::AccountLimits {
                max_running_jobs: Some(2),
                ..Default::default()
            },
        );

        let ids: Vec<JobId> = (0..3)
            .map(|i| {
                let mut s = basic_spec(&format!("m{i}"));
                s.account = Some("research".into());
                submit_and_wait(&cm, s)
            })
            .collect();

        let pending: Vec<JobId> = cm.pending_jobs().iter().map(|j| j.job_id).collect();
        let granted = ids.iter().filter(|id| pending.contains(id)).count();
        assert_eq!(
            granted, 2,
            "pending_jobs() returned {granted} account jobs but max_running_jobs=2 allows 2"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_jobs_does_not_oversubscribe_qos_max_jobs_per_user_within_one_pass() {
        // The per-user running-job count cap is a same-pass accumulator too: three
        // pending jobs from one user under a QOS capped at max_jobs_per_user=2
        // must not all be admitted in a single pass even though none is running yet.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 64, 128000);
        cm.qos_cache().insert(Qos {
            name: "cnt".into(),
            limits: spur_core::accounting::QosLimits {
                max_jobs_per_user: Some(2),
                ..Default::default()
            },
            ..Default::default()
        });

        let ids: Vec<JobId> = (0..3)
            .map(|i| {
                let mut s = basic_spec(&format!("c{i}"));
                s.qos = Some("cnt".into());
                submit_and_wait(&cm, s)
            })
            .collect();

        let pending: Vec<JobId> = cm.pending_jobs().iter().map(|j| j.job_id).collect();
        let granted = ids.iter().filter(|id| pending.contains(id)).count();
        assert_eq!(
            granted, 2,
            "pending_jobs() returned {granted} jobs but max_jobs_per_user=2 allows 2"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_jobs_does_not_oversubscribe_qos_max_tres_per_user_within_one_pass() {
        // The per-user TRES cap (distinct from the group cap) also accumulates
        // within a pass: two 3-cpu jobs from one user under maxtresperuser cpu=4
        // must not both be admitted (3+3 > 4), even with an empty running set.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 64, 128000);
        let mut per_user = TresRecord::new();
        per_user.set(TresType::Cpu, 4);
        cm.qos_cache().insert(Qos {
            name: "usertres".into(),
            limits: spur_core::accounting::QosLimits {
                max_tres_per_user: Some(per_user),
                ..Default::default()
            },
            ..Default::default()
        });

        let ids: Vec<JobId> = (0..2)
            .map(|i| {
                let mut s = basic_spec(&format!("u{i}"));
                s.qos = Some("usertres".into());
                s.num_tasks = 3;
                submit_and_wait(&cm, s)
            })
            .collect();

        let pending: Vec<JobId> = cm.pending_jobs().iter().map(|j| j.job_id).collect();
        let granted = ids.iter().filter(|id| pending.contains(id)).count();
        assert_eq!(
            granted, 1,
            "pending_jobs() returned {granted} jobs but maxtresperuser cpu=4 fits only one 3-cpu job"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bb_request_over_pool_sets_resources_reason() {
        // A job asking for more BB capacity than the pool holds stays PENDING
        // with BurstBufferResources, and pending_jobs() drops it from scheduling.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        *cm.burst_buffer_total_gb.write() = 100;

        let mut spec = basic_spec("bb-too-big");
        spec.burst_buffer = Some("capacity=500".into());
        let job_id = submit_and_wait(&cm, spec);

        cm.tag_blocked_pending_reasons();
        assert_eq!(
            cm.get_job(job_id).unwrap().pending_reason,
            PendingReason::BurstBufferResources
        );
        let pending: Vec<JobId> = cm.pending_jobs().iter().map(|j| j.job_id).collect();
        assert!(
            !pending.contains(&job_id),
            "a job over the BB pool must be dropped from pending_jobs()"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bb_stage_in_holds_then_becomes_dispatchable() {
        // A BB job that fits the pool reserves capacity (None -> Staging), is
        // held with BurstBufferStageIn and excluded from dispatch, then becomes
        // dispatchable once stage-in completes (Staging -> Ready).
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        *cm.burst_buffer_total_gb.write() = 100;

        let mut spec = basic_spec("bb-stage");
        spec.burst_buffer = Some("capacity=40".into());
        let job_id = submit_and_wait(&cm, spec);

        cm.advance_bb_staging();
        assert_eq!(
            cm.get_job(job_id).unwrap().bb_stage_state,
            BbStageState::Staging
        );
        assert_eq!(cm.available_bb(), 60);

        cm.tag_blocked_pending_reasons();
        assert_eq!(
            cm.get_job(job_id).unwrap().pending_reason,
            PendingReason::BurstBufferStageIn
        );
        let pending: Vec<JobId> = cm.pending_jobs().iter().map(|j| j.job_id).collect();
        assert!(
            !pending.contains(&job_id),
            "a staging BB job must not be dispatched until stage-in completes"
        );

        assert!(cm.complete_bb_stage_in(job_id));
        assert_eq!(
            cm.get_job(job_id).unwrap().bb_stage_state,
            BbStageState::Ready
        );
        let pending: Vec<JobId> = cm.pending_jobs().iter().map(|j| j.job_id).collect();
        assert!(
            pending.contains(&job_id),
            "a Ready BB job must be dispatchable"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bb_staging_does_not_oversubscribe_pool() {
        // Two BB jobs each want 60GB but the pool holds 100. advance_bb_staging()
        // must reserve for only one; the other stays None and is reported as a
        // resource shortage.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        *cm.burst_buffer_total_gb.write() = 100;

        let mut s1 = basic_spec("bb-a");
        s1.burst_buffer = Some("capacity=60".into());
        let a = submit_and_wait(&cm, s1);
        let mut s2 = basic_spec("bb-b");
        s2.burst_buffer = Some("capacity=60".into());
        let b = submit_and_wait(&cm, s2);

        let staged = cm.advance_bb_staging();
        assert_eq!(staged.len(), 1, "only one 60GB job fits a 100GB pool");

        let states: Vec<(JobId, BbStageState)> = [a, b]
            .iter()
            .map(|id| (*id, cm.get_job(*id).unwrap().bb_stage_state))
            .collect();
        let staging = states
            .iter()
            .filter(|(_, s)| *s == BbStageState::Staging)
            .count();
        let none = states
            .iter()
            .filter(|(_, s)| *s == BbStageState::None)
            .count();
        assert_eq!((staging, none), (1, 1), "exactly one job stages");

        cm.tag_blocked_pending_reasons();
        let unstaged = states
            .iter()
            .find(|(_, s)| *s == BbStageState::None)
            .map(|(id, _)| *id)
            .unwrap();
        assert_eq!(
            cm.get_job(unstaged).unwrap().pending_reason,
            PendingReason::BurstBufferResources
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bb_capacity_freed_when_job_completes() {
        // A BB job releases its reserved capacity when it leaves the active set,
        // and the configured total is never mutated.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        *cm.burst_buffer_total_gb.write() = 100;

        let mut spec = basic_spec("bb-life");
        spec.burst_buffer = Some("capacity=40".into());
        let id = submit_and_wait(&cm, spec);

        cm.advance_bb_staging();
        assert!(cm.complete_bb_stage_in(id));
        assert_eq!(cm.available_bb(), 60);

        let res = scalar_alloc(1, 1000);
        cm.start_job(
            id,
            vec!["n1".into()],
            res.clone(),
            per_node_for(&["n1"], res),
        )
        .unwrap();
        settle(&cm, id, JobState::Running);
        assert_eq!(cm.available_bb(), 60, "running BB job still holds capacity");

        cm.cancel_job(id, "testuser").unwrap();
        settle(&cm, id, JobState::Cancelled);
        assert_eq!(
            cm.available_bb(),
            100,
            "capacity must be freed when the job leaves the active set"
        );
        assert_eq!(
            *cm.burst_buffer_total_gb.read(),
            100,
            "configured total must never be mutated"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tag_blocked_preserves_held_reason() {
        // A user-held job blocked by a reservation must stay Held, not get
        // reclassified to Reservation.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        let mut spec = basic_spec("held");
        spec.reservation = Some("does-not-exist".into());
        let job_id = submit_and_wait(&cm, spec);
        {
            let mut jobs = cm.jobs.write();
            jobs.get_mut(&job_id).unwrap().pending_reason = PendingReason::Held;
        }

        cm.tag_blocked_pending_reasons();
        assert_eq!(
            cm.get_job(job_id).unwrap().pending_reason,
            PendingReason::Held
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tag_blocked_dependency_takes_precedence_over_reservation() {
        // Blocked by both a dependency and an absent reservation -> Dependency
        // wins (pending_jobs() drops at the dependency filter, ahead of reservation).
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        // Parent running -> child's afterok dependency is Waiting (not satisfied).
        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(basic_spec("parent")),
        });
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id: 1,
            old_state: JobState::Pending,
            new_state: JobState::Running,
            pending_reason: None,
            pending_priority: None,
        });

        let mut child = basic_spec("child");
        child.dependency = vec!["afterok:1".into()];
        child.reservation = Some("does-not-exist".into());
        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 2,
            spec: Box::new(child),
        });

        cm.tag_blocked_pending_reasons();
        assert_eq!(
            cm.get_job(2).unwrap().pending_reason,
            PendingReason::Dependency
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn complete_terminal_job_errors() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        let job_id = submit_and_wait(&cm, basic_spec("j"));
        cm.cancel_job(job_id, "testuser").unwrap();
        settle(&cm, job_id, JobState::Cancelled);

        let result = cm.complete_job(job_id, 1, JobState::Failed);
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_running_job_releases_resources() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        register_node(&cm, "worker1", 8, 16000);
        let job_id = submit_and_wait(&cm, basic_spec("cancel-alloc"));

        let resources = scalar_alloc(2, 4000);
        cm.start_job(
            job_id,
            vec!["worker1".into()],
            resources.clone(),
            per_node_for(&["worker1"], resources),
        )
        .unwrap();
        settle(&cm, job_id, JobState::Running);

        let node = cm.get_node("worker1").unwrap();
        assert_eq!(node.alloc_resources.cpus, 2);

        cm.cancel_job(job_id, "testuser").unwrap();
        settle(&cm, job_id, JobState::Cancelled);

        let node = cm.get_node("worker1").unwrap();
        assert_eq!(
            node.alloc_resources.cpus, 0,
            "resources must be freed after cancel"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn double_cancel_returns_error() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        let job_id = submit_and_wait(&cm, basic_spec("double-cancel"));
        cm.cancel_job(job_id, "testuser").unwrap();
        settle(&cm, job_id, JobState::Cancelled);

        let result = cm.cancel_job(job_id, "testuser");
        assert!(
            result.is_err(),
            "cancelling an already-cancelled job must fail"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_job_wrong_user_rejected() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        let job_id = submit_and_wait(&cm, basic_spec("auth-cancel"));
        let result = cm.cancel_job(job_id, "other_user");
        assert!(
            result.is_err(),
            "non-owner must not cancel another user's job"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("cannot") && err_msg.contains("cancel"),
            "error should mention the denied action: {err_msg}"
        );

        // Job must still be alive.
        let job = cm.get_job(job_id).unwrap();
        assert!(!job.state.is_terminal());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_job_root_allowed() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        let job_id = submit_and_wait(&cm, basic_spec("root-cancel"));
        cm.cancel_job(job_id, "root").unwrap();
        settle(&cm, job_id, JobState::Cancelled);
        assert_eq!(cm.get_job(job_id).unwrap().state, JobState::Cancelled);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_job_empty_user_allowed() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        let job_id = submit_and_wait(&cm, basic_spec("internal-cancel"));
        cm.cancel_job(job_id, "").unwrap();
        settle(&cm, job_id, JobState::Cancelled);
        assert_eq!(cm.get_job(job_id).unwrap().state, JobState::Cancelled);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn suspend_job_wrong_user_rejected() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        let id = submit_and_wait(&cm, basic_spec("sus-auth"));
        let res = scalar_alloc(2, 4000);
        cm.start_job(
            id,
            vec!["n1".into()],
            res.clone(),
            per_node_for(&["n1"], res),
        )
        .unwrap();
        settle(&cm, id, JobState::Running);

        let result = cm.suspend_job(id, "other_user");
        assert!(
            result.is_err(),
            "non-owner must not suspend another user's job"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("cannot") && err_msg.contains("suspend"),
            "error should mention the denied action: {err_msg}"
        );
        assert_eq!(cm.get_job(id).unwrap().state, JobState::Running);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn suspend_job_root_allowed() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        let id = submit_and_wait(&cm, basic_spec("sus-root"));
        let res = scalar_alloc(2, 4000);
        cm.start_job(
            id,
            vec!["n1".into()],
            res.clone(),
            per_node_for(&["n1"], res),
        )
        .unwrap();
        settle(&cm, id, JobState::Running);

        cm.suspend_job(id, "root").unwrap();
        settle(&cm, id, JobState::Suspended);
        assert_eq!(cm.get_job(id).unwrap().state, JobState::Suspended);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resume_job_wrong_user_rejected() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        let id = submit_and_wait(&cm, basic_spec("res-auth"));
        let res = scalar_alloc(2, 4000);
        cm.start_job(
            id,
            vec!["n1".into()],
            res.clone(),
            per_node_for(&["n1"], res),
        )
        .unwrap();
        settle(&cm, id, JobState::Running);
        cm.suspend_job(id, "testuser").unwrap();
        settle(&cm, id, JobState::Suspended);

        let result = cm.resume_job(id, "other_user");
        assert!(
            result.is_err(),
            "non-owner must not resume another user's job"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("cannot") && err_msg.contains("resume"),
            "error should mention the denied action: {err_msg}"
        );
        assert_eq!(cm.get_job(id).unwrap().state, JobState::Suspended);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resume_job_root_allowed() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16000);
        let id = submit_and_wait(&cm, basic_spec("res-root"));
        let res = scalar_alloc(2, 4000);
        cm.start_job(
            id,
            vec!["n1".into()],
            res.clone(),
            per_node_for(&["n1"], res),
        )
        .unwrap();
        settle(&cm, id, JobState::Running);
        cm.suspend_job(id, "testuser").unwrap();
        settle(&cm, id, JobState::Suspended);

        cm.resume_job(id, "root").unwrap();
        settle(&cm, id, JobState::Running);
        assert_eq!(cm.get_job(id).unwrap().state, JobState::Running);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshot_and_restore() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        register_node(&cm, "n1", 4, 8000);
        submit_and_wait(&cm, basic_spec("snap-job"));

        let data = cm.snapshot_state().unwrap();
        assert!(!data.is_empty());

        // Create a fresh cluster and restore
        let dir2 = TempDir::new().unwrap();
        let cm2 = test_cluster(&dir2).await;
        cm2.restore_from_snapshot(&data).unwrap();

        assert!(cm2.get_job(1).is_some());
        assert!(cm2.get_node("n1").is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn k0s_state_survives_snapshot() {
        use spur_core::k0s::{K0sPhase, K0sRole};
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        register_node(&cm, "n1", 4, 8000);
        cm.assign_node_k0s("n1", K0sRole::Worker, "10.44.0.2", "10.42.2.0/24")
            .unwrap();
        cm.set_k0s_phase(K0sPhase::Ready, Some("head-node".into()), false)
            .unwrap();
        wait_for("k0s state applied", || {
            cm.k0s_state().phase == K0sPhase::Ready
                && cm.get_node("n1").and_then(|n| n.k0s_role).is_some()
        });

        // snapshot -> restore into a fresh cluster (log-compaction / HA follower path)
        let data = cm.snapshot_state().unwrap();
        let dir2 = TempDir::new().unwrap();
        let cm2 = test_cluster(&dir2).await;
        cm2.restore_from_snapshot(&data).unwrap();

        // Cluster-wide k0s state must be restored (it is runtime-authoritative).
        let st = cm2.k0s_state();
        assert_eq!(st.phase, K0sPhase::Ready);
        assert_eq!(st.control_plane_node.as_deref(), Some("head-node"));
        // Per-node k0s fields ride snap.nodes.
        let n1 = cm2.get_node("n1").unwrap();
        assert_eq!(n1.k0s_role, Some(K0sRole::Worker));
        assert_eq!(n1.k0s_mesh_ip.as_deref(), Some("10.44.0.2"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn provision_reassigns_a_readded_node() {
        // The Ready-phase self-heal: a spurd restart deregisters on SIGTERM (the node is REMOVED),
        // then re-registers as a fresh node with no k0s role. Re-running provisioning (which the
        // reconcile loop now does in Ready, not only Provisioning) must re-assign the un-roled node
        // its role + mesh IP + pod CIDR so it rejoins the mesh — without disturbing the others.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "node-a", 4, 8000);
        register_node(&cm, "node-b", 4, 8000);
        wait_for("both registered", || {
            cm.get_node("node-a").is_some() && cm.get_node("node-b").is_some()
        });
        let net = crate::cluster_k8s::ClusterNetworking {
            mesh_cidr: "10.44.0.0/16".into(),
            pod_cidr: "10.42.0.0/16".into(),
            service_cidr: "10.43.0.0/16".into(),
            cni_mtu: 1450,
            cni: "kuberouter".into(),
            control_plane_node: None,
        };
        crate::cluster_k8s::provision_assignments(&cm, &net, &cm.k0s_state()).unwrap();
        wait_for("both assigned", || {
            cm.get_node("node-a").and_then(|n| n.k0s_role).is_some()
                && cm.get_node("node-b").and_then(|n| n.k0s_role).is_some()
        });
        let b_role = cm.get_node("node-b").unwrap().k0s_role;
        let b_ip = cm.get_node("node-b").unwrap().k0s_mesh_ip.clone();
        let b_cidr = cm.get_node("node-b").unwrap().k0s_pod_cidr.clone();
        assert!(b_ip.is_some() && b_cidr.is_some());

        // spurd restart: deregister (remove) then re-register as a fresh, un-roled node.
        cm.remove_node("node-b", true, Some("test restart".into()))
            .unwrap();
        wait_for("node-b removed", || cm.get_node("node-b").is_none());
        register_node(&cm, "node-b", 4, 8000);
        wait_for("node-b re-registered without a role", || {
            cm.get_node("node-b")
                .map(|n| n.k0s_role.is_none())
                .unwrap_or(false)
        });

        // The Ready-phase reconcile re-runs provisioning and heals the un-roled node.
        crate::cluster_k8s::provision_assignments(&cm, &net, &cm.k0s_state()).unwrap();
        wait_for("node-b re-assigned", || {
            cm.get_node("node-b").and_then(|n| n.k0s_role).is_some()
        });
        let b = cm.get_node("node-b").unwrap();
        assert_eq!(b.k0s_role, b_role, "same role after re-add");
        assert_eq!(b.k0s_mesh_ip, b_ip, "same mesh IP reclaimed after re-add");
        assert_eq!(b.k0s_pod_cidr, b_cidr, "same pod CIDR after re-add");
        // The untouched node keeps its assignment (provisioning is idempotent).
        assert!(cm.get_node("node-a").and_then(|n| n.k0s_role).is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ready_phase_reconcile_assigns_unroled_node() {
        // The loop wiring: reconcile_phase must run provisioning in the Ready phase, not only
        // Provisioning. An un-roled node present while Ready (e.g. re-added after a spurd restart)
        // must be assigned by a Ready-phase reconcile tick. If Ready were a no-op it would stay
        // un-roled forever (out of the mesh) — the bug this fixes.
        use spur_core::k0s::K0sPhase;
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "node-a", 4, 8000);
        wait_for("registered", || cm.get_node("node-a").is_some());
        // Cluster is Ready with the control plane already recorded, but node-a has no k0s role.
        cm.set_k0s_phase(K0sPhase::Ready, Some("node-a".into()), false)
            .unwrap();
        wait_for("phase ready", || cm.k0s_state().phase == K0sPhase::Ready);
        assert!(cm.get_node("node-a").and_then(|n| n.k0s_role).is_none());

        let net = crate::cluster_k8s::ClusterNetworking {
            mesh_cidr: "10.44.0.0/16".into(),
            pod_cidr: "10.42.0.0/16".into(),
            service_cidr: "10.43.0.0/16".into(),
            cni_mtu: 1450,
            cni: "kuberouter".into(),
            control_plane_node: Some("node-a".into()),
        };
        let mut tokens = std::collections::HashMap::new();
        crate::cluster_k8s::reconcile_phase(&cm, &net, &cm.k0s_state(), &mut tokens).await;

        wait_for("un-roled node assigned by a Ready-phase tick", || {
            cm.get_node("node-a").and_then(|n| n.k0s_role).is_some()
        });
        assert!(cm.get_node("node-a").and_then(|n| n.k0s_mesh_ip).is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn provision_assigns_controller_and_worker_for_two_nodes() {
        use spur_core::k0s::K0sRole;
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "node-a", 4, 8000);
        register_node(&cm, "node-b", 4, 8000);
        wait_for("both nodes registered", || {
            cm.get_node("node-a").is_some() && cm.get_node("node-b").is_some()
        });

        let net = crate::cluster_k8s::ClusterNetworking {
            mesh_cidr: "10.44.0.0/16".into(),
            pod_cidr: "10.42.0.0/16".into(),
            service_cidr: "10.43.0.0/16".into(),
            cni_mtu: 1450,
            cni: "kuberouter".into(),
            control_plane_node: None,
        };
        crate::cluster_k8s::provision_assignments(&cm, &net, &cm.k0s_state()).unwrap();
        wait_for("both nodes assigned k0s roles", || {
            cm.get_node("node-a").and_then(|n| n.k0s_role).is_some()
                && cm.get_node("node-b").and_then(|n| n.k0s_role).is_some()
        });

        // Two nodes -> the deterministic control-plane (lexically-first, node-a) is a Controller
        // (NOT Single, which would never exercise the worker token-mint/join path); node-b is a
        // Worker. Each gets a distinct mesh IP + pod /24, and the control-plane choice is recorded.
        let a = cm.get_node("node-a").unwrap();
        let b = cm.get_node("node-b").unwrap();
        assert_eq!(a.k0s_role, Some(K0sRole::Controller));
        assert_eq!(b.k0s_role, Some(K0sRole::Worker));
        assert!(a.k0s_mesh_ip.is_some() && b.k0s_mesh_ip.is_some());
        assert_ne!(a.k0s_mesh_ip, b.k0s_mesh_ip);
        assert_ne!(a.k0s_pod_cidr, b.k0s_pod_cidr);
        assert_eq!(cm.k0s_state().control_plane_node.as_deref(), Some("node-a"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restore_from_snapshot_rejects_corrupt_data() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        assert!(cm.restore_from_snapshot(b"not valid json").is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hold_and_release_job() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        let id = submit_and_wait(&cm, basic_spec("holdme"));

        cm.hold_job(id).unwrap();
        wait_for("hold applied", || {
            cm.get_job(id).is_some_and(|j| j.priority == 0)
        });
        let job = cm.get_job(id).unwrap();
        assert_eq!(job.priority, 0);
        assert_eq!(job.pending_reason, PendingReason::Held);

        cm.release_job(id).unwrap();
        wait_for("release applied", || {
            cm.get_job(id).is_some_and(|j| j.priority > 0)
        });
        let job = cm.get_job(id).unwrap();
        assert_eq!(job.priority, 1000);
        assert_eq!(job.pending_reason, PendingReason::Priority);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_job_priority() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        let id = submit_and_wait(&cm, basic_spec("prio"));

        cm.update_job(id, None, Some(5000), None, None, None, None)
            .unwrap();
        wait_for("priority updated", || {
            cm.get_job(id).is_some_and(|j| j.priority == 5000)
        });
        assert_eq!(cm.get_job(id).unwrap().priority, 5000);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_job_qos_validates_against_cache() {
        // `scontrol update job qos=` must not be a second door to the bypass:
        // unknown and empty QOS are rejected, leaving the job's QOS unchanged.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        cm.qos_cache().insert(Qos {
            name: "highprio".into(),
            ..Default::default()
        });
        let id = submit_and_wait(&cm, basic_spec("q"));
        assert_eq!(cm.get_job(id).unwrap().spec.qos, None);

        // Unknown QOS rejected.
        let err = cm
            .update_job(id, None, None, None, None, None, Some("ghost".into()))
            .unwrap_err();
        assert!(err.to_string().contains("QOS 'ghost' does not exist"));
        assert_eq!(cm.get_job(id).unwrap().spec.qos, None);

        // Empty QOS (clear-to-limitless) rejected.
        let err = cm
            .update_job(id, None, None, None, None, None, Some(String::new()))
            .unwrap_err();
        assert!(err.to_string().contains("cannot clear a job's QOS"));
        assert_eq!(cm.get_job(id).unwrap().spec.qos, None);

        // A valid QOS is applied.
        cm.update_job(id, None, None, None, None, None, Some("highprio".into()))
            .unwrap();
        assert_eq!(
            cm.get_job(id).unwrap().spec.qos.as_deref(),
            Some("highprio")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_job_qos_rejects_unauthorized_association_qos() {
        // `scontrol update job qos=` must enforce the same per-association
        // authorization as submission (SPUR-101).
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        cm.qos_cache().insert(Qos {
            name: "highprio".into(),
            ..Default::default()
        });
        cm.qos_cache().insert(Qos {
            name: "other-teams-qos".into(),
            ..Default::default()
        });
        cm.association_cache()
            .insert_default_qos("testuser", "research", "highprio");
        let mut spec = basic_spec("q");
        spec.account = Some("research".into());
        let id = submit_and_wait(&cm, spec);
        // Submission already applied the association's own default QOS.
        assert_eq!(
            cm.get_job(id).unwrap().spec.qos.as_deref(),
            Some("highprio")
        );

        let err = cm
            .update_job(
                id,
                None,
                None,
                None,
                None,
                None,
                Some("other-teams-qos".into()),
            )
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("QOS 'other-teams-qos' is not permitted"));
        assert_eq!(
            cm.get_job(id).unwrap().spec.qos.as_deref(),
            Some("highprio"),
            "rejected update must leave the existing QOS untouched"
        );

        // The association's own default QOS is still accepted explicitly.
        cm.update_job(id, None, None, None, None, None, Some("highprio".into()))
            .unwrap();
        assert_eq!(
            cm.get_job(id).unwrap().spec.qos.as_deref(),
            Some("highprio")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_node_state() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 4, 8000);

        cm.update_node_state("n1", NodeState::Drain, Some("maint".into()))
            .unwrap();
        wait_for("node drain applied", || {
            cm.get_node("n1")
                .is_some_and(|n| n.state == NodeState::Drain)
        });
        let node = cm.get_node("n1").unwrap();
        assert_eq!(node.state, NodeState::Drain);
        assert_eq!(node.state_reason, Some("maint".into()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn check_node_health_marks_stale_down() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "stale", 4, 8000);

        // Set last_heartbeat far in the past
        if let Some(node) = cm.nodes.write().get_mut("stale") {
            node.last_heartbeat = Some(Utc::now() - chrono::Duration::seconds(200));
        }

        cm.check_node_health(90);
        wait_for("health check applied", || {
            cm.get_node("stale")
                .is_some_and(|n| n.state == NodeState::Down)
        });
        let node = cm.get_node("stale").unwrap();
        assert_eq!(node.state, NodeState::Down);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn admin_drained_node_stays_locked_through_timeout_and_reregister() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "locked", 4, 8000);

        // Give the node an allocation so Drain becomes Draining
        let id = submit_and_wait(&cm, basic_spec("hold-job"));
        let alloc = scalar_alloc(2, 4000);
        cm.start_job(
            id,
            vec!["locked".into()],
            alloc.clone(),
            per_node_for(&["locked"], alloc),
        )
        .unwrap();
        settle(&cm, id, JobState::Running);

        // Admin drains while job is running — becomes Draining (admin_locked)
        cm.update_node_state("locked", NodeState::Drain, Some("hw swap".into()))
            .unwrap();
        wait_for("draining applied", || {
            cm.get_node("locked")
                .is_some_and(|n| n.state == NodeState::Draining)
        });
        assert!(cm.get_node("locked").unwrap().admin_locked);

        // Heartbeat times out — Draining → Down, admin_locked preserved
        if let Some(node) = cm.nodes.write().get_mut("locked") {
            node.last_heartbeat = Some(Utc::now() - chrono::Duration::seconds(200));
        }
        cm.check_node_health(90);
        wait_for("health check applied", || {
            cm.get_node("locked")
                .is_some_and(|n| n.state == NodeState::Down)
        });
        let node = cm.get_node("locked").unwrap();
        assert_eq!(node.state, NodeState::Down);
        assert!(
            node.admin_locked,
            "admin lock must survive heartbeat timeout"
        );

        // Agent reconnects — re-registration must NOT recover to Idle
        cm.register_node(
            "locked".into(),
            ResourceSet {
                cpus: 4,
                memory_mb: 8000,
                ..Default::default()
            },
            "127.0.0.1".into(),
            6818,
            String::new(),
            "1.0".into(),
            NodeSource::NativeHost,
            HashMap::new(),
        )
        .unwrap();
        let node = cm.get_node("locked").unwrap();
        assert_eq!(
            node.state,
            NodeState::Down,
            "admin-locked node must not auto-recover"
        );
        assert!(node.admin_locked);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn requeue_resets_fields_via_apply() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 4, 8000);
        let id = submit_and_wait(&cm, basic_spec("requeue-me"));

        let alloc = scalar_alloc(2, 4000);
        cm.start_job(
            id,
            vec!["n1".into()],
            alloc.clone(),
            per_node_for(&["n1"], alloc),
        )
        .unwrap();
        settle(&cm, id, JobState::Running);

        // A reported path must not survive requeue (points at a node the job leaves).
        cm.set_job_output_paths(id, "/tmp/spur.out".into(), "/tmp/spur.out".into());
        assert!(cm.get_job(id).unwrap().actual_stdout_path.is_some());

        cm.apply_operation(&WalOperation::JobComplete {
            job_id: id,
            exit_code: -1,
            state: JobState::Timeout,
        });
        assert_eq!(cm.get_job(id).unwrap().state, JobState::Timeout);

        // Requeue: Timeout → Pending should reset allocation fields
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id: id,
            old_state: JobState::Timeout,
            new_state: JobState::Pending,
            pending_reason: None,
            pending_priority: None,
        });

        let job = cm.get_job(id).unwrap();
        assert_eq!(job.state, JobState::Pending);
        assert_eq!(job.requeue_count, 1);
        assert!(
            job.start_time.is_none(),
            "start_time should be cleared on requeue"
        );
        assert!(
            job.allocated_nodes.is_empty(),
            "allocated_nodes should be cleared"
        );
        assert!(
            job.allocated_resources.is_none(),
            "allocated_resources should be cleared"
        );
        assert_eq!(job.pending_reason, PendingReason::None);
        assert!(
            job.actual_stdout_path.is_none() && job.actual_stderr_path.is_none(),
            "stale reported output path should be cleared on requeue"
        );
    }

    // Empty ("" from a mixed-version agent) must not shadow the computed fallback.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_job_output_paths_ignores_empty() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 4, 8000);
        let id = submit_and_wait(&cm, basic_spec("empty-paths"));

        cm.set_job_output_paths(id, String::new(), String::new());
        let job = cm.get_job(id).unwrap();
        assert!(job.actual_stdout_path.is_none());
        assert!(job.actual_stderr_path.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn requeue_job_frees_node_resources() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 4, 8000);
        let id = submit_and_wait(&cm, basic_spec("dispatch-fail"));

        let alloc = scalar_alloc(2, 4000);
        cm.start_job(
            id,
            vec!["n1".into()],
            alloc.clone(),
            per_node_for(&["n1"], alloc),
        )
        .unwrap();
        settle(&cm, id, JobState::Running);

        let node = cm.get_node("n1").unwrap();
        assert_eq!(
            node.alloc_resources.cpus, 2,
            "CPUs should be allocated after start"
        );

        // Simulate all-dispatch-failed requeue (the fix under test)
        cm.requeue_job(id).unwrap();
        settle(&cm, id, JobState::Pending);

        let job = cm.get_job(id).unwrap();
        assert_eq!(job.state, JobState::Pending);
        assert_eq!(job.requeue_count, 1);
        assert!(job.start_time.is_none(), "start_time should be cleared");
        assert!(
            job.allocated_nodes.is_empty(),
            "allocated_nodes should be cleared"
        );
        assert!(
            job.allocated_resources.is_none(),
            "allocated_resources should be cleared"
        );

        let node = cm.get_node("n1").unwrap();
        assert_eq!(
            node.alloc_resources.cpus, 0,
            "node CPUs must be freed after requeue"
        );
        assert!(
            !node.alloc_resources.has_devices(),
            "node devices must be freed after requeue"
        );
        assert_eq!(node.state, NodeState::Idle, "node should return to Idle");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn evict_job_transitions_partial_dispatch_failure_to_nodefail() {
        // A multi-node job where the dispatch RPC only reaches some of the
        // assigned nodes must not be left running forever waiting on
        // completions from a node that never launched it.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 4, 8000);
        register_node(&cm, "n2", 4, 8000);

        let mut spec = basic_spec("partial-dispatch-fail");
        spec.num_nodes = 2;
        let id = submit_and_wait(&cm, spec);

        let alloc = scalar_alloc(2, 4000);
        cm.start_job(
            id,
            vec!["n1".into(), "n2".into()],
            scalar_alloc(4, 8000),
            per_node_for(&["n1", "n2"], alloc),
        )
        .unwrap();
        settle(&cm, id, JobState::Running);

        // n1's dispatch succeeded, n2's never reached the agent.
        cm.evict_job(id).unwrap();
        settle(&cm, id, JobState::NodeFail);

        let job = cm.get_job(id).unwrap();
        assert_eq!(job.state, JobState::NodeFail);
        assert_eq!(
            job.pending_reason,
            PendingReason::JobLaunchFailure,
            "partial-dispatch eviction must report the job never fully launched, \
             not a mid-run node failure"
        );
        assert!(
            job.node_completions.is_empty(),
            "node_completions must be cleared so a stray late report can't reopen the job"
        );

        for name in ["n1", "n2"] {
            let node = cm.get_node(name).unwrap();
            assert_eq!(
                node.alloc_resources.cpus, 0,
                "allocation on {name} must be freed on eviction"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn evict_job_does_not_double_deallocate_a_node_that_already_completed() {
        // Job A spans n1+n2 and shares n1 with unrelated Job B. n1 reports
        // A's completion (freeing A's slice, moving A to Completing) before
        // n2's dispatch failure is discovered and A is evicted. Evicting A
        // must not subtract A's already-freed n1 slice a second time — doing
        // so would incorrectly free capacity that actually still belongs to
        // B, letting the node be oversubscribed.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 8, 16_000);
        register_node(&cm, "n2", 4, 8000);

        let mut spec_a = basic_spec("job-a-shares-n1");
        spec_a.num_nodes = 2;
        let job_a = submit_and_wait(&cm, spec_a);
        cm.start_job(
            job_a,
            vec!["n1".into(), "n2".into()],
            scalar_alloc(4, 8000),
            per_node_for(&["n1", "n2"], scalar_alloc(2, 4000)),
        )
        .unwrap();
        settle(&cm, job_a, JobState::Running);

        let spec_b = basic_spec("job-b-shares-n1");
        let job_b = submit_and_wait(&cm, spec_b);
        cm.start_job(
            job_b,
            vec!["n1".into()],
            scalar_alloc(3, 3000),
            per_node_for(&["n1"], scalar_alloc(3, 3000)),
        )
        .unwrap();
        settle(&cm, job_b, JobState::Running);

        assert_eq!(
            cm.get_node("n1").unwrap().alloc_resources.cpus,
            5,
            "n1 should hold both A's (2) and B's (3) allocations"
        );

        cm.apply_operation(&WalOperation::JobNodeComplete {
            job_id: job_a,
            node_name: "n1".into(),
            exit_code: 0,
            signal: 0,
        });
        settle(&cm, job_a, JobState::Completing);
        assert_eq!(
            cm.get_node("n1").unwrap().alloc_resources.cpus,
            3,
            "A's n1 slice must already be freed by its own completion report, \
             leaving only B's 3 cpus"
        );

        cm.evict_job(job_a).unwrap();
        settle(&cm, job_a, JobState::NodeFail);

        assert_eq!(
            cm.get_node("n1").unwrap().alloc_resources.cpus,
            3,
            "evicting A must not re-subtract its already-freed n1 slice and \
             clobber B's still-running allocation"
        );
        assert_eq!(
            cm.get_node("n2").unwrap().alloc_resources.cpus,
            0,
            "n2's slice must still be freed by the eviction"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn register_node_gets_partition_via_propose() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "test-node", 4, 8000);

        let node = cm.get_node("test-node").unwrap();
        assert!(!node.partitions.is_empty());
        assert_eq!(node.partitions[0], "default");
    }

    // --- Pure evaluate_node_health tests (no Raft needed) ---

    fn make_health_node(
        name: &str,
        state: NodeState,
        admin_locked: bool,
        last_hb: Option<chrono::DateTime<Utc>>,
    ) -> Node {
        let mut node = Node::new(name.into(), ResourceSet::default());
        node.state = state;
        node.admin_locked = admin_locked;
        node.last_heartbeat = last_hb;
        node
    }

    #[test]
    fn health_stale_idle_marks_down() {
        let node = make_health_node(
            "n1",
            NodeState::Idle,
            false,
            Some(Utc::now() - chrono::Duration::seconds(200)),
        );
        let actions = super::evaluate_node_health(&[&node], Utc::now(), 90);
        assert_eq!(
            actions,
            vec![super::HealthAction::MarkDown {
                name: "n1".into(),
                old_state: NodeState::Idle,
                admin_locked: false,
            }]
        );
    }

    #[test]
    fn health_fresh_down_recovers() {
        let node = make_health_node(
            "n1",
            NodeState::Down,
            false,
            Some(Utc::now() - chrono::Duration::seconds(10)),
        );
        let actions = super::evaluate_node_health(&[&node], Utc::now(), 90);
        assert_eq!(
            actions,
            vec![super::HealthAction::Recover {
                name: "n1".into(),
                old_state: NodeState::Down,
            }]
        );
    }

    #[test]
    fn health_admin_locked_down_no_recovery() {
        let node = make_health_node(
            "n1",
            NodeState::Down,
            true,
            Some(Utc::now() - chrono::Duration::seconds(10)),
        );
        let actions = super::evaluate_node_health(&[&node], Utc::now(), 90);
        assert!(actions.is_empty());
    }

    #[test]
    fn health_drain_not_marked_down() {
        let node = make_health_node(
            "n1",
            NodeState::Drain,
            true,
            Some(Utc::now() - chrono::Duration::seconds(200)),
        );
        let actions = super::evaluate_node_health(&[&node], Utc::now(), 90);
        assert!(actions.is_empty());
    }

    #[test]
    fn health_idle_fresh_no_action() {
        let node = make_health_node(
            "n1",
            NodeState::Idle,
            false,
            Some(Utc::now() - chrono::Duration::seconds(10)),
        );
        let actions = super::evaluate_node_health(&[&node], Utc::now(), 90);
        assert!(actions.is_empty());
    }

    #[test]
    fn health_no_heartbeat_skipped() {
        let node = make_health_node("n1", NodeState::Idle, false, None);
        let actions = super::evaluate_node_health(&[&node], Utc::now(), 90);
        assert!(actions.is_empty());
    }

    #[test]
    fn health_mixed_actions() {
        let stale = make_health_node(
            "stale",
            NodeState::Idle,
            false,
            Some(Utc::now() - chrono::Duration::seconds(200)),
        );
        let recovering = make_health_node(
            "back",
            NodeState::Down,
            false,
            Some(Utc::now() - chrono::Duration::seconds(10)),
        );
        let stable = make_health_node(
            "ok",
            NodeState::Idle,
            false,
            Some(Utc::now() - chrono::Duration::seconds(10)),
        );
        let actions = super::evaluate_node_health(&[&stale, &recovering, &stable], Utc::now(), 90);
        assert_eq!(actions.len(), 2);
        assert_eq!(
            actions[0],
            super::HealthAction::MarkDown {
                name: "stale".into(),
                old_state: NodeState::Idle,
                admin_locked: false,
            }
        );
        assert_eq!(
            actions[1],
            super::HealthAction::Recover {
                name: "back".into(),
                old_state: NodeState::Down,
            }
        );
    }

    // --- Pure evaluate_registration tests ---

    #[test]
    fn registration_new_node() {
        let resources = ResourceSet {
            cpus: 4,
            memory_mb: 8000,
            ..Default::default()
        };
        assert_eq!(
            super::evaluate_registration(None, &resources),
            super::RegistrationAction::Register,
        );
    }

    #[test]
    fn registration_unchanged_skip() {
        let resources = ResourceSet {
            cpus: 4,
            memory_mb: 8000,
            ..Default::default()
        };
        let node = Node::new("n1".into(), resources.clone());
        assert_eq!(
            super::evaluate_registration(Some(&node), &resources),
            super::RegistrationAction::Skip,
        );
    }

    #[test]
    fn registration_resources_changed_update() {
        let old = ResourceSet {
            cpus: 4,
            memory_mb: 8000,
            ..Default::default()
        };
        let new = ResourceSet {
            cpus: 8,
            memory_mb: 16000,
            ..Default::default()
        };
        let node = Node::new("n1".into(), old);
        assert_eq!(
            super::evaluate_registration(Some(&node), &new),
            super::RegistrationAction::Update,
        );
    }

    // --- expand_job_specs tests ---

    #[test]
    fn expand_non_array_returns_single_spec() {
        let spec = basic_spec("simple");
        let result = super::expand_job_specs(spec, 1).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "simple");
        assert!(result[0].array_job_id.is_none());
        assert!(result[0].array_task_id.is_none());
        assert!(result[0].array_max_concurrent.is_none());
    }

    #[test]
    fn expand_array_with_throttle() {
        let mut spec = basic_spec("arr");
        spec.array_spec = Some("0-4%2".into());
        let result = super::expand_job_specs(spec, 10).unwrap();
        assert_eq!(result.len(), 5);
        for (i, s) in result.iter().enumerate() {
            assert_eq!(s.array_job_id, Some(10));
            assert_eq!(s.array_task_id, Some(i as u32));
            assert_eq!(s.array_max_concurrent, Some(2));
            assert!(s.array_spec.is_none());
            assert_eq!(s.name, "arr");
        }
    }

    #[test]
    fn expand_array_without_throttle() {
        let mut spec = basic_spec("arr");
        spec.array_spec = Some("0-4".into());
        let result = super::expand_job_specs(spec, 5).unwrap();
        assert_eq!(result.len(), 5);
        for s in &result {
            assert_eq!(s.array_job_id, Some(5));
            assert!(s.array_max_concurrent.is_none());
        }
    }

    #[test]
    fn expand_array_invalid_spec_errors() {
        let mut spec = basic_spec("bad");
        spec.array_spec = Some("10-5".into());
        assert!(super::expand_job_specs(spec, 1).is_err());
    }

    // --- apply_default_partition tests ---

    #[test]
    fn apply_default_partition_picks_default() {
        let mut spec = basic_spec("j");
        spec.partition = None;
        let partitions = vec![
            Partition {
                name: "other".into(),
                is_default: false,
                ..Default::default()
            },
            Partition {
                name: "gpu".into(),
                is_default: true,
                ..Default::default()
            },
        ];
        super::apply_default_partition(&mut spec, &partitions);
        assert_eq!(spec.partition.as_deref(), Some("gpu"));
    }

    #[test]
    fn apply_default_time_limit_uses_partition_default() {
        let mut spec = basic_spec("j");
        spec.partition = Some("gpu".into());
        spec.time_limit = None;
        let partitions = vec![Partition {
            name: "gpu".into(),
            default_time_minutes: Some(30),
            ..Default::default()
        }];
        super::apply_default_time_limit(&mut spec, &partitions);
        assert_eq!(spec.time_limit, Some(chrono::Duration::minutes(30)));
    }

    #[test]
    fn apply_default_time_limit_noop_when_set() {
        let mut spec = basic_spec("j");
        spec.time_limit = Some(chrono::Duration::minutes(5));
        let partitions = vec![Partition {
            name: "gpu".into(),
            default_time_minutes: Some(30),
            ..Default::default()
        }];
        super::apply_default_time_limit(&mut spec, &partitions);
        assert_eq!(spec.time_limit, Some(chrono::Duration::minutes(5)));
    }

    #[test]
    fn apply_default_time_limit_skips_when_partition_has_no_default() {
        let mut spec = basic_spec("j");
        spec.partition = Some("gpu".into());
        spec.time_limit = None;
        let partitions = vec![Partition {
            name: "gpu".into(),
            default_time_minutes: None,
            ..Default::default()
        }];
        super::apply_default_time_limit(&mut spec, &partitions);
        assert!(spec.time_limit.is_none());
    }

    #[test]
    fn apply_default_partition_falls_back_to_first() {
        let mut spec = basic_spec("j");
        spec.partition = None;
        let partitions = vec![Partition {
            name: "batch".into(),
            is_default: false,
            ..Default::default()
        }];
        super::apply_default_partition(&mut spec, &partitions);
        assert_eq!(spec.partition.as_deref(), Some("batch"));
    }

    #[test]
    fn apply_default_partition_noop_when_set() {
        let mut spec = basic_spec("j");
        spec.partition = Some("mypart".into());
        let partitions = vec![Partition {
            name: "default".into(),
            is_default: true,
            ..Default::default()
        }];
        super::apply_default_partition(&mut spec, &partitions);
        assert_eq!(spec.partition.as_deref(), Some("mypart"));
    }

    #[test]
    fn apply_default_partition_treats_empty_string_as_unset() {
        let mut spec = basic_spec("j");
        spec.partition = Some(String::new());
        let partitions = vec![Partition {
            name: "gpu".into(),
            is_default: true,
            ..Default::default()
        }];
        super::apply_default_partition(&mut spec, &partitions);
        assert_eq!(spec.partition.as_deref(), Some("gpu"));
    }

    // --- apply_default_qos tests ---

    fn qos_cache_with(names: &[&str]) -> QosCache {
        let cache = QosCache::new();
        for name in names {
            cache.insert(Qos {
                name: (*name).into(),
                ..Default::default()
            });
        }
        cache
    }

    // Inert config: no fallback, require_qos off (base resolution chain).
    fn acct_cfg() -> spur_core::config::AccountingConfig {
        spur_core::config::AccountingConfig::default()
    }

    fn acct_cfg_with(default_qos: &str, require_qos: bool) -> spur_core::config::AccountingConfig {
        spur_core::config::AccountingConfig {
            default_qos: default_qos.into(),
            require_qos,
            ..Default::default()
        }
    }

    #[test]
    fn apply_default_qos_explicit_valid_passes_through() {
        let assoc = AssociationCache::new();
        let qos = qos_cache_with(&["highprio"]);
        let mut spec = basic_spec("j");
        spec.qos = Some("highprio".into());

        super::apply_default_qos(&mut spec, &assoc, &qos, &acct_cfg()).unwrap();
        assert_eq!(spec.qos.as_deref(), Some("highprio"));
    }

    #[test]
    fn apply_default_qos_explicit_invalid_is_rejected() {
        let assoc = AssociationCache::new();
        let qos = qos_cache_with(&["normal"]);
        let mut spec = basic_spec("j");
        spec.qos = Some("doesnotexist".into());

        let err = super::apply_default_qos(&mut spec, &assoc, &qos, &acct_cfg()).unwrap_err();
        assert_eq!(
            err,
            SubmitError::invalid("QOS 'doesnotexist' does not exist")
        );
    }

    #[test]
    fn apply_default_qos_inherits_association_default_with_explicit_account() {
        let assoc = AssociationCache::new();
        assoc.insert_default_qos("testuser", "research", "highprio");
        let qos = qos_cache_with(&["highprio"]);
        let mut spec = basic_spec("j");
        spec.account = Some("research".into());

        super::apply_default_qos(&mut spec, &assoc, &qos, &acct_cfg()).unwrap();
        assert_eq!(spec.qos.as_deref(), Some("highprio"));
    }

    #[test]
    fn apply_default_account_inherits_users_default_when_unset() {
        let assoc = AssociationCache::new();
        assoc.insert_default_account("testuser", "research");
        let mut spec = basic_spec("j");
        assert!(spec.account.is_none());

        super::apply_default_account(&mut spec, &assoc);
        assert_eq!(spec.account.as_deref(), Some("research"));
    }

    #[test]
    fn apply_default_account_skips_when_cache_not_loaded() {
        let assoc = AssociationCache::new();
        let mut spec = basic_spec("j");
        super::apply_default_account(&mut spec, &assoc);
        assert!(spec.account.is_none());
    }

    #[test]
    fn apply_default_account_noop_when_explicit() {
        let assoc = AssociationCache::new();
        assoc.insert_default_account("testuser", "research");
        let mut spec = basic_spec("j");
        spec.account = Some("faculty".into());

        super::apply_default_account(&mut spec, &assoc);
        assert_eq!(spec.account.as_deref(), Some("faculty"));
    }

    #[test]
    fn apply_default_qos_inherits_via_users_default_account_when_no_dash_a() {
        let assoc = AssociationCache::new();
        assoc.insert_default_account("testuser", "research");
        assoc.insert_default_qos("testuser", "research", "highprio");
        let qos = qos_cache_with(&["highprio"]);
        let mut spec = basic_spec("j");
        // No --account given at all.
        assert!(spec.account.is_none());

        super::apply_default_qos(&mut spec, &assoc, &qos, &acct_cfg()).unwrap();
        assert_eq!(spec.qos.as_deref(), Some("highprio"));
    }

    #[test]
    fn apply_default_qos_no_association_default_leaves_qos_unset() {
        let assoc = AssociationCache::new();
        let qos = QosCache::new();
        let mut spec = basic_spec("j");
        spec.account = Some("research".into());

        super::apply_default_qos(&mut spec, &assoc, &qos, &acct_cfg()).unwrap();
        assert_eq!(spec.qos, None);
    }

    #[test]
    fn apply_default_qos_stale_association_default_degrades_silently() {
        let assoc = AssociationCache::new();
        // Association still points at a QOS that has since been deleted.
        assoc.insert_default_qos("testuser", "research", "deleted-qos");
        let qos = QosCache::new(); // empty: "deleted-qos" is not there
        let mut spec = basic_spec("j");
        spec.account = Some("research".into());

        super::apply_default_qos(&mut spec, &assoc, &qos, &acct_cfg()).unwrap();
        assert_eq!(spec.qos, None, "must not fail submission on stale data");
    }

    #[test]
    fn apply_default_qos_falls_back_to_cluster_default() {
        // No --qos and no association default → cluster fallback applies.
        let assoc = AssociationCache::new();
        let qos = qos_cache_with(&["normal"]);
        let mut spec = basic_spec("j");

        super::apply_default_qos(&mut spec, &assoc, &qos, &acct_cfg_with("normal", false)).unwrap();
        assert_eq!(spec.qos.as_deref(), Some("normal"));
    }

    #[test]
    fn apply_default_qos_association_default_beats_cluster_default() {
        let assoc = AssociationCache::new();
        assoc.insert_default_qos("testuser", "research", "highprio");
        let qos = qos_cache_with(&["highprio", "normal"]);
        let mut spec = basic_spec("j");
        spec.account = Some("research".into());

        super::apply_default_qos(&mut spec, &assoc, &qos, &acct_cfg_with("normal", false)).unwrap();
        assert_eq!(
            spec.qos.as_deref(),
            Some("highprio"),
            "association default takes precedence over the cluster fallback"
        );
    }

    #[test]
    fn apply_default_qos_nonexistent_cluster_default_is_rejected() {
        // A misconfigured fallback must hard-error, not silently leave it unenforced.
        let assoc = AssociationCache::new();
        let qos = qos_cache_with(&["normal"]);
        let mut spec = basic_spec("j");

        let err = super::apply_default_qos(&mut spec, &assoc, &qos, &acct_cfg_with("ghost", false))
            .unwrap_err();
        assert_eq!(
            err,
            SubmitError::invalid("configured default QOS 'ghost' does not exist")
        );
    }

    #[test]
    fn apply_default_qos_require_qos_rejects_when_none_resolves() {
        // require_qos with no fallback rejects a job that resolves to no QOS.
        let assoc = AssociationCache::new();
        let qos = QosCache::new();
        let mut spec = basic_spec("j");
        spec.account = Some("research".into());

        let err = super::apply_default_qos(&mut spec, &assoc, &qos, &acct_cfg_with("", true))
            .unwrap_err();
        assert_eq!(
            err,
            SubmitError::invalid(
                "no QOS specified and no default QOS is configured for this user/account"
            )
        );
    }

    #[test]
    fn apply_default_qos_require_qos_satisfied_by_cluster_default() {
        // With both set, the fallback satisfies require_qos — no rejection.
        let assoc = AssociationCache::new();
        let qos = qos_cache_with(&["normal"]);
        let mut spec = basic_spec("j");

        super::apply_default_qos(&mut spec, &assoc, &qos, &acct_cfg_with("normal", true)).unwrap();
        assert_eq!(spec.qos.as_deref(), Some("normal"));
    }

    #[test]
    fn apply_default_qos_require_qos_satisfied_by_explicit() {
        // An explicit valid QOS satisfies require_qos regardless of fallback.
        let assoc = AssociationCache::new();
        let qos = qos_cache_with(&["highprio"]);
        let mut spec = basic_spec("j");
        spec.qos = Some("highprio".into());

        super::apply_default_qos(&mut spec, &assoc, &qos, &acct_cfg_with("", true)).unwrap();
        assert_eq!(spec.qos.as_deref(), Some("highprio"));
    }

    #[test]
    fn apply_default_qos_explicit_unauthorized_qos_is_rejected() {
        // An explicit QOS that exists but isn't the association's own default
        // must be rejected, not silently accepted (SPUR-101).
        let assoc = AssociationCache::new();
        assoc.insert_default_qos("testuser", "research", "highprio");
        let qos = qos_cache_with(&["highprio", "other-teams-qos"]);
        let mut spec = basic_spec("j");
        spec.account = Some("research".into());
        spec.qos = Some("other-teams-qos".into());

        let err = super::apply_default_qos(&mut spec, &assoc, &qos, &acct_cfg()).unwrap_err();
        assert_eq!(
            err,
            SubmitError::invalid(
                "QOS 'other-teams-qos' is not permitted for user 'testuser' under account 'research'"
            )
        );
    }

    #[test]
    fn apply_default_qos_explicit_matches_association_default_is_allowed() {
        let assoc = AssociationCache::new();
        assoc.insert_default_qos("testuser", "research", "highprio");
        let qos = qos_cache_with(&["highprio"]);
        let mut spec = basic_spec("j");
        spec.account = Some("research".into());
        spec.qos = Some("highprio".into());

        super::apply_default_qos(&mut spec, &assoc, &qos, &acct_cfg()).unwrap();
        assert_eq!(spec.qos.as_deref(), Some("highprio"));
    }

    #[test]
    fn apply_default_qos_explicit_allowed_when_association_has_no_default_pinned() {
        // A loaded cache with a real membership but no default QOS on record
        // for this association has never been pinned to one — stay permissive.
        let assoc = AssociationCache::new();
        assoc.insert_association("testuser", "research");
        let qos = qos_cache_with(&["anything"]);
        let mut spec = basic_spec("j");
        spec.account = Some("research".into());
        spec.qos = Some("anything".into());

        super::apply_default_qos(&mut spec, &assoc, &qos, &acct_cfg()).unwrap();
        assert_eq!(spec.qos.as_deref(), Some("anything"));
    }

    #[test]
    fn apply_default_qos_explicit_skips_authz_when_no_account_resolves() {
        // Cache loaded but the user has no resolvable account (e.g. no
        // default account and none given) — nothing to authorize against.
        let assoc = AssociationCache::new();
        assoc.set_loaded_without_associations();
        let qos = qos_cache_with(&["anything"]);
        let mut spec = basic_spec("j");
        spec.qos = Some("anything".into());

        super::apply_default_qos(&mut spec, &assoc, &qos, &acct_cfg()).unwrap();
        assert_eq!(spec.qos.as_deref(), Some("anything"));
    }

    // ── array-parent dependency: cancel + display synthesis ──────

    /// Submit an array task job directly via the WAL (bypassing expansion) so
    /// tests can construct specific parent/task topologies.
    fn submit_array_task(cm: &ClusterManager, id: JobId, parent: JobId, task: u32) {
        let mut spec = basic_spec("arr");
        spec.array_job_id = Some(parent);
        spec.array_task_id = Some(task);
        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: id,
            spec: Box::new(spec),
        });
    }

    fn set_terminal(cm: &ClusterManager, id: JobId, state: JobState, exit_code: i32) {
        // Jobs may only reach Completed/Failed/etc. via Running; cancel is the
        // only legal direct transition out of Pending.
        if state != JobState::Cancelled {
            cm.apply_operation(&WalOperation::JobStateChange {
                job_id: id,
                old_state: JobState::Pending,
                new_state: JobState::Running,
                pending_reason: None,
                pending_priority: None,
            });
        }
        cm.apply_operation(&WalOperation::JobComplete {
            job_id: id,
            exit_code,
            state,
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_unsatisfiable_dep_cancels_failed_afterok() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        // Parent scalar job that fails.
        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(basic_spec("parent")),
        });
        set_terminal(&cm, 1, JobState::Failed, 1);

        // Child depends on afterok:1 — can never be satisfied.
        let mut child = basic_spec("child");
        child.dependency = vec!["afterok:1".into()];
        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 2,
            spec: Box::new(child),
        });

        let cancelled = cm.cancel_unsatisfiable_dependency_jobs();
        assert_eq!(cancelled, vec![2]);
        assert_eq!(cm.get_job(2).unwrap().state, JobState::Cancelled);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_unsatisfiable_dep_skips_running_job() {
        // A Running job with an unsatisfiable dep must not be cancelled
        // (Running -> Cancelled would destroy live work).
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(basic_spec("parent")),
        });
        set_terminal(&cm, 1, JobState::Failed, 1);

        let mut child = basic_spec("child");
        child.dependency = vec!["afterok:1".into()];
        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 2,
            spec: Box::new(child),
        });
        // Child is already Running by the time the cancel pass fires.
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id: 2,
            old_state: JobState::Pending,
            new_state: JobState::Running,
            pending_reason: None,
            pending_priority: None,
        });

        let cancelled = cm.cancel_unsatisfiable_dependency_jobs();
        assert!(cancelled.is_empty(), "running job must not be cancelled");
        assert_eq!(cm.get_job(2).unwrap().state, JobState::Running);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_unsatisfiable_dep_tags_waiting_jobs() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        // Parent still running; child waits, not cancelled.
        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(basic_spec("parent")),
        });
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id: 1,
            old_state: JobState::Pending,
            new_state: JobState::Running,
            pending_reason: None,
            pending_priority: None,
        });

        let mut child = basic_spec("child");
        child.dependency = vec!["afterok:1".into()];
        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 2,
            spec: Box::new(child),
        });

        let cancelled = cm.cancel_unsatisfiable_dependency_jobs();
        assert!(cancelled.is_empty());
        let child = cm.get_job(2).unwrap();
        assert_eq!(child.state, JobState::Pending);
        assert_eq!(child.pending_reason, PendingReason::Dependency);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_unsatisfiable_dep_array_parent_all_completed_releases() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        // Array parent id 10, tasks 11/12/13 all completed.
        submit_array_task(&cm, 11, 10, 0);
        submit_array_task(&cm, 12, 10, 1);
        submit_array_task(&cm, 13, 10, 2);
        for id in [11, 12, 13] {
            set_terminal(&cm, id, JobState::Completed, 0);
        }

        // Child depends on afterok:10 (the array parent) — should be satisfied,
        // so neither cancelled nor tagged.
        let mut child = basic_spec("child");
        child.dependency = vec!["afterok:10".into()];
        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 20,
            spec: Box::new(child),
        });

        let cancelled = cm.cancel_unsatisfiable_dependency_jobs();
        assert!(cancelled.is_empty());
        let child = cm.get_job(20).unwrap();
        assert_eq!(child.state, JobState::Pending);
        assert_ne!(child.pending_reason, PendingReason::Dependency);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_unsatisfiable_dep_array_parent_one_failed_cancels() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        submit_array_task(&cm, 11, 10, 0);
        submit_array_task(&cm, 12, 10, 1);
        set_terminal(&cm, 11, JobState::Completed, 0);
        set_terminal(&cm, 12, JobState::Failed, 1);

        let mut child = basic_spec("child");
        child.dependency = vec!["afterok:10".into()];
        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 20,
            spec: Box::new(child),
        });

        let cancelled = cm.cancel_unsatisfiable_dependency_jobs();
        assert_eq!(cancelled, vec![20]);
        assert_eq!(cm.get_job(20).unwrap().state, JobState::Cancelled);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_job_for_display_synthesizes_array_parent() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        // No stored job with id 10; tasks 11/12 carry array_job_id=10.
        submit_array_task(&cm, 11, 10, 0);
        submit_array_task(&cm, 12, 10, 1);

        // Unfinished → aggregate Pending, no exit_code.
        let synth = cm
            .get_job_for_display(10)
            .expect("array parent should synthesize");
        assert_eq!(synth.job_id, 10);
        assert_eq!(synth.state, JobState::Pending);
        assert_eq!(synth.spec.array_job_id, Some(10));
        assert_eq!(synth.spec.array_task_id, None);
        assert_eq!(synth.exit_code, None);

        // Complete both → aggregate Completed, exit_code 0.
        set_terminal(&cm, 11, JobState::Completed, 0);
        set_terminal(&cm, 12, JobState::Completed, 0);
        let synth = cm.get_job_for_display(10).unwrap();
        assert_eq!(synth.state, JobState::Completed);
        assert_eq!(synth.exit_code, Some(0));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_job_for_display_scalar_and_unknown() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 1,
            spec: Box::new(basic_spec("scalar")),
        });
        // Stored scalar job returned as-is.
        assert_eq!(cm.get_job_for_display(1).unwrap().job_id, 1);
        // Unknown id → None.
        assert!(cm.get_job_for_display(999).is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_jobs_by_id_synthesizes_array_parent() {
        // `scontrol show job <parent>` / squeue go through the get_jobs list
        // RPC, not get_job. A query for the array parent id must return the
        // synthesized aggregate, not an empty list.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        submit_array_task(&cm, 11, 10, 0);
        submit_array_task(&cm, 12, 10, 1);

        // Query the parent id explicitly.
        let got = cm.get_jobs(&[], None, None, None, None, &[10]);
        assert_eq!(got.len(), 1, "parent id should synthesize one record");
        assert_eq!(got[0].job_id, 10);
        assert_eq!(got[0].state, JobState::Pending);
        assert_eq!(got[0].spec.array_job_id, Some(10));

        // Querying a real task id still returns that task, not the parent.
        let got_task = cm.get_jobs(&[], None, None, None, None, &[11]);
        assert_eq!(got_task.len(), 1);
        assert_eq!(got_task[0].job_id, 11);

        // Unknown id → empty.
        assert!(cm.get_jobs(&[], None, None, None, None, &[999]).is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_jobs_filters_by_name() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        submit_and_wait(&cm, basic_spec("alpha"));
        submit_and_wait(&cm, basic_spec("beta"));
        submit_and_wait(&cm, basic_spec("alpha"));

        let all = cm.get_jobs(&[], None, None, None, None, &[]);
        assert_eq!(all.len(), 3);

        let alphas = cm.get_jobs(&[], None, None, None, Some("alpha"), &[]);
        assert_eq!(alphas.len(), 2);
        assert!(alphas.iter().all(|j| j.spec.name == "alpha"));

        let betas = cm.get_jobs(&[], None, None, None, Some("beta"), &[]);
        assert_eq!(betas.len(), 1);

        let multi = cm.get_jobs(&[], None, None, None, Some("alpha,beta"), &[]);
        assert_eq!(multi.len(), 3);

        let none = cm.get_jobs(&[], None, None, None, Some("nonexistent"), &[]);
        assert!(none.is_empty());

        let empty = cm.get_jobs(&[], None, None, None, Some(""), &[]);
        assert_eq!(empty.len(), 3);
    }

    // --- Partition matching tests ---

    #[test]
    fn partition_matches_node_by_hostlist() {
        let part = Partition {
            name: "gpu".into(),
            nodes: "node[1-3]".into(),
            ..Default::default()
        };
        let empty_labels = HashMap::new();
        assert!(super::partition_matches_node(&part, "node1", &empty_labels));
        assert!(super::partition_matches_node(&part, "node3", &empty_labels));
        assert!(!super::partition_matches_node(
            &part,
            "node4",
            &empty_labels
        ));
    }

    #[test]
    fn partition_matches_node_by_selector() {
        let mut selector = HashMap::new();
        selector.insert("pool".into(), "train".into());
        let part = Partition {
            name: "train".into(),
            selector,
            ..Default::default()
        };
        let mut labels = HashMap::new();
        labels.insert("pool".into(), "train".into());
        labels.insert("gpu".into(), "mi300x".into());
        assert!(super::partition_matches_node(
            &part,
            "arbitrary-host",
            &labels
        ));

        let wrong_labels = HashMap::from([("pool".into(), "infer".into())]);
        assert!(!super::partition_matches_node(
            &part,
            "arbitrary-host",
            &wrong_labels
        ));
    }

    #[test]
    fn partition_matches_node_union_of_both() {
        let mut selector = HashMap::new();
        selector.insert("pool".into(), "train".into());
        let part = Partition {
            name: "gpu".into(),
            nodes: "node1".into(),
            selector,
            ..Default::default()
        };
        // Matches by hostlist alone
        assert!(super::partition_matches_node(
            &part,
            "node1",
            &HashMap::new()
        ));
        // Matches by selector alone
        let labels = HashMap::from([("pool".into(), "train".into())]);
        assert!(super::partition_matches_node(&part, "other-host", &labels));
        // Matches neither
        assert!(!super::partition_matches_node(
            &part,
            "other-host",
            &HashMap::new()
        ));
    }

    #[test]
    fn node_config_matches_by_selector() {
        let nc = spur_core::config::NodeConfig {
            names: String::new(),
            selector: HashMap::from([("gpu".into(), "mi300x".into())]),
            cpus: 0,
            memory_mb: 0,
            gres: Vec::new(),
            features: Vec::new(),
            address: None,
            weight: 1,
        };
        let labels = HashMap::from([("gpu".into(), "mi300x".into())]);
        assert!(super::node_config_matches(&nc, "any-host", &labels));
        assert!(!super::node_config_matches(
            &nc,
            "any-host",
            &HashMap::new()
        ));
    }

    // --- Label update + partition re-routing ---

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_labels_reroutes_partition() {
        let dir = TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.partitions = vec![
            spur_core::config::PartitionConfig {
                name: "default".into(),
                default: true,
                state: "UP".into(),
                nodes: "ALL".into(),
                selector: HashMap::new(),
                max_time: None,
                default_time: None,
                max_nodes: None,
                min_nodes: 1,
                allow_accounts: Vec::new(),
                allow_groups: Vec::new(),
                deny_accounts: Vec::new(),
                priority_tier: 1,
                preempt_mode: String::new(),
            },
            spur_core::config::PartitionConfig {
                name: "train".into(),
                default: false,
                state: "UP".into(),
                nodes: String::new(),
                selector: HashMap::from([("pool".into(), "train".into())]),
                max_time: None,
                default_time: None,
                max_nodes: None,
                min_nodes: 1,
                allow_accounts: Vec::new(),
                allow_groups: Vec::new(),
                deny_accounts: Vec::new(),
                priority_tier: 1,
                preempt_mode: String::new(),
            },
        ];
        let cm = Arc::new(ClusterManager::new(cfg, dir.path()).unwrap());
        let handle = crate::raft::start_raft(1, &["[::1]:0".into()], dir.path(), cm.clone())
            .await
            .unwrap();
        handle
            .raft
            .wait(Some(std::time::Duration::from_secs(5)))
            .metrics(|m| m.current_leader == Some(1), "leader elected")
            .await
            .unwrap();
        cm.set_raft(handle.raft);

        register_node(&cm, "worker1", 4, 8000);
        let node = cm.get_node("worker1").unwrap();
        // Initially only in "default" (ALL matches everything)
        assert!(node.partitions.contains(&"default".into()));
        assert!(!node.partitions.contains(&"train".into()));

        // Add label that matches "train" partition selector
        cm.update_node_labels(
            "worker1",
            HashMap::from([("pool".into(), "train".into())]),
            &[],
        )
        .unwrap();
        wait_for("label applied", || {
            cm.get_node("worker1")
                .map(|n| !n.labels.is_empty())
                .unwrap_or(false)
        });

        let node = cm.get_node("worker1").unwrap();
        assert!(node.partitions.contains(&"train".into()));
        assert_eq!(node.labels.get("pool"), Some(&"train".into()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn register_node_with_labels_gets_selector_partition() {
        let dir = TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.partitions = vec![spur_core::config::PartitionConfig {
            name: "inference".into(),
            default: false,
            state: "UP".into(),
            nodes: String::new(),
            selector: HashMap::from([("role".into(), "infer".into())]),
            max_time: None,
            default_time: None,
            max_nodes: None,
            min_nodes: 1,
            allow_accounts: Vec::new(),
            allow_groups: Vec::new(),
            deny_accounts: Vec::new(),
            priority_tier: 1,
            preempt_mode: String::new(),
        }];
        let cm = Arc::new(ClusterManager::new(cfg, dir.path()).unwrap());
        let handle = crate::raft::start_raft(1, &["[::1]:0".into()], dir.path(), cm.clone())
            .await
            .unwrap();
        handle
            .raft
            .wait(Some(std::time::Duration::from_secs(5)))
            .metrics(|m| m.current_leader == Some(1), "leader elected")
            .await
            .unwrap();
        cm.set_raft(handle.raft);

        cm.register_node(
            "dyn-node".into(),
            ResourceSet {
                cpus: 8,
                memory_mb: 16000,
                ..Default::default()
            },
            "127.0.0.1".into(),
            6818,
            String::new(),
            String::new(),
            spur_core::node::NodeSource::NativeHost,
            HashMap::from([("role".into(), "infer".into())]),
        )
        .unwrap();
        wait_for("node registered", || cm.get_node("dyn-node").is_some());

        let node = cm.get_node("dyn-node").unwrap();
        assert!(node.partitions.contains(&"inference".into()));
    }

    #[test]
    fn partition_all_matches_any_node() {
        let part = Partition {
            name: "everything".into(),
            nodes: "ALL".into(),
            ..Default::default()
        };
        assert!(super::partition_matches_node(
            &part,
            "random-host-xyz",
            &HashMap::new()
        ));
        assert!(super::partition_matches_node(
            &part,
            "node1",
            &HashMap::new()
        ));
    }

    #[test]
    fn node_config_all_matches_any_node() {
        let nc = spur_core::config::NodeConfig {
            names: "ALL".into(),
            selector: HashMap::new(),
            cpus: 0,
            memory_mb: 0,
            gres: Vec::new(),
            features: vec!["common".into()],
            address: None,
            weight: 1,
        };
        assert!(super::node_config_matches(&nc, "any-host", &HashMap::new()));
        assert!(super::node_config_matches(
            &nc,
            "another",
            &HashMap::from([("x".into(), "y".into())])
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reregistration_syncs_labels() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        // First registration with labels
        cm.register_node(
            "worker1".into(),
            ResourceSet {
                cpus: 4,
                memory_mb: 8000,
                ..Default::default()
            },
            "127.0.0.1".into(),
            6818,
            String::new(),
            String::new(),
            spur_core::node::NodeSource::NativeHost,
            HashMap::from([("pool".into(), "train".into())]),
        )
        .unwrap();
        wait_for("node registered", || cm.get_node("worker1").is_some());
        assert_eq!(
            cm.get_node("worker1").unwrap().labels.get("pool"),
            Some(&"train".into())
        );

        // Re-register with same resources but different labels
        cm.register_node(
            "worker1".into(),
            ResourceSet {
                cpus: 4,
                memory_mb: 8000,
                ..Default::default()
            },
            "127.0.0.1".into(),
            6818,
            String::new(),
            String::new(),
            spur_core::node::NodeSource::NativeHost,
            HashMap::from([("pool".into(), "infer".into()), ("tier".into(), "1".into())]),
        )
        .unwrap();
        wait_for("labels synced", || {
            cm.get_node("worker1")
                .map(|n| n.labels.get("pool") == Some(&"infer".into()))
                .unwrap_or(false)
        });

        let node = cm.get_node("worker1").unwrap();
        assert_eq!(node.labels.get("pool"), Some(&"infer".into()));
        assert_eq!(node.labels.get("tier"), Some(&"1".into()));
    }

    #[test]
    fn label_update_applies_nodeconfig_features() {
        let dir = TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.nodes = vec![spur_core::config::NodeConfig {
            names: String::new(),
            selector: HashMap::from([("gpu".into(), "mi300x".into())]),
            cpus: 0,
            memory_mb: 0,
            gres: Vec::new(),
            features: vec!["mi300x".into(), "rocm6".into()],
            address: None,
            weight: 10,
        }];
        let cm = ClusterManager::new(cfg, dir.path()).unwrap();

        // Register a node directly via WAL apply
        cm.apply_operation(&WalOperation::NodeRegister {
            name: "gpu-node".into(),
            resources: ResourceSet {
                cpus: 8,
                memory_mb: 16000,
                ..Default::default()
            },
            address: "127.0.0.1".into(),
            port: 6818,
            wg_pubkey: String::new(),
            version: String::new(),
            labels: HashMap::new(),
        });

        let node = cm.get_node("gpu-node").unwrap();
        assert!(node.features.is_empty());

        // Apply label update that matches the NodeConfig selector
        cm.apply_operation(&WalOperation::NodeLabelsUpdate {
            name: "gpu-node".into(),
            set: HashMap::from([("gpu".into(), "mi300x".into())]),
            remove: Vec::new(),
        });

        let node = cm.get_node("gpu-node").unwrap();
        assert_eq!(node.features, vec!["mi300x", "rocm6"]);
        assert_eq!(node.weight, 10);
    }

    #[test]
    fn label_update_resets_features_when_no_match() {
        let dir = TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.nodes = vec![spur_core::config::NodeConfig {
            names: String::new(),
            selector: HashMap::from([("gpu".into(), "mi300x".into())]),
            cpus: 0,
            memory_mb: 0,
            gres: Vec::new(),
            features: vec!["mi300x".into(), "rocm6".into()],
            address: None,
            weight: 10,
        }];
        let cm = ClusterManager::new(cfg, dir.path()).unwrap();

        cm.apply_operation(&WalOperation::NodeRegister {
            name: "gpu-node".into(),
            resources: ResourceSet {
                cpus: 8,
                memory_mb: 16000,
                ..Default::default()
            },
            address: "127.0.0.1".into(),
            port: 6818,
            wg_pubkey: String::new(),
            version: String::new(),
            labels: HashMap::from([("gpu".into(), "mi300x".into())]),
        });

        let node = cm.get_node("gpu-node").unwrap();
        assert_eq!(node.features, vec!["mi300x", "rocm6"]);
        assert_eq!(node.weight, 10);

        cm.apply_operation(&WalOperation::NodeLabelsUpdate {
            name: "gpu-node".into(),
            set: HashMap::new(),
            remove: vec!["gpu".into()],
        });

        let node = cm.get_node("gpu-node").unwrap();
        assert!(node.features.is_empty());
        assert_eq!(node.weight, 1);
    }

    #[test]
    fn node_register_no_match_uses_defaults() {
        let dir = TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.nodes = vec![spur_core::config::NodeConfig {
            names: String::new(),
            selector: HashMap::from([("gpu".into(), "mi300x".into())]),
            cpus: 0,
            memory_mb: 0,
            gres: Vec::new(),
            features: vec!["mi300x".into(), "rocm6".into()],
            address: None,
            weight: 10,
        }];
        let cm = ClusterManager::new(cfg, dir.path()).unwrap();

        cm.apply_operation(&WalOperation::NodeRegister {
            name: "cpu-node".into(),
            resources: ResourceSet {
                cpus: 8,
                memory_mb: 16000,
                ..Default::default()
            },
            address: "127.0.0.1".into(),
            port: 6818,
            wg_pubkey: String::new(),
            version: String::new(),
            labels: HashMap::from([("gpu".into(), "mi250".into())]),
        });

        let node = cm.get_node("cpu-node").unwrap();
        assert!(node.features.is_empty());
        assert_eq!(node.weight, 1);
    }

    #[test]
    fn reconcile_resets_stale_features_on_restore() {
        let dir = TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.nodes = vec![spur_core::config::NodeConfig {
            names: String::new(),
            selector: HashMap::from([("gpu".into(), "mi300x".into())]),
            cpus: 0,
            memory_mb: 0,
            gres: Vec::new(),
            features: vec!["mi300x".into(), "rocm6".into()],
            address: None,
            weight: 10,
        }];
        let cm = ClusterManager::new(cfg, dir.path()).unwrap();

        // Snapshot node has stale policy but labels that no longer match the config.
        let mut stale = Node::new(
            "gpu-node".into(),
            ResourceSet {
                cpus: 8,
                memory_mb: 16000,
                ..Default::default()
            },
        );
        stale.features = vec!["mi300x".into(), "rocm6".into()];
        stale.weight = 10;
        stale.labels = HashMap::new();

        let snap = ClusterSnapshot {
            jobs: Vec::new(),
            nodes: vec![stale],
            reservations: Vec::new(),
            steps: Vec::new(),
            license_pool: HashMap::new(),
            tokens: Vec::new(),
            burst_buffer_total_gb: 0,
            k0s: spur_core::k0s::K0sClusterState::default(),
        };
        let bytes = serde_json::to_vec(&snap).unwrap();
        cm.restore_from_snapshot(&bytes).unwrap();

        let node = cm.get_node("gpu-node").unwrap();
        assert!(node.features.is_empty());
        assert_eq!(node.weight, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submit_job_triggers_scheduler_notify() {
        // Verify that submit_job() actually calls notify_one() in production code path.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        // Set up a listener before submitting
        let notify = cm.scheduler_notify.clone();
        let listener = tokio::spawn(async move {
            notify.notified().await;
        });

        // Give listener time to register
        tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;

        // Submit a job - this should trigger notify_one()
        let spec = basic_spec("test");
        let _ = submit_and_wait(&cm, spec);

        // Verify notification was received (with timeout to prevent hanging)
        let result = tokio::time::timeout(tokio::time::Duration::from_millis(100), listener).await;

        assert!(
            result.is_ok(),
            "submit_job should call notify_one() to wake scheduler"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submit_job_notifies_even_with_array_expansion() {
        // Array jobs expand into multiple tasks; verify notify is called during expansion.
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        // Set up a listener before submitting
        let notify = cm.scheduler_notify.clone();
        let listener = tokio::spawn(async move {
            notify.notified().await;
        });

        // Submit an array job (expands to multiple tasks). `submit_job` returns the
        // array parent id, which is not stored — only per-task ids exist in `jobs`.
        let mut spec = basic_spec("array");
        spec.array_spec = Some("0-2".into()); // Creates 3 tasks
        let parent_id = cm.submit_job(spec).unwrap();
        let first_task_id = parent_id + 1;
        wait_for(&format!("array task {first_task_id} applied"), || {
            cm.get_job(first_task_id).is_some()
        });

        // Verify notification was received (with timeout to prevent hanging)
        let result = tokio::time::timeout(tokio::time::Duration::from_secs(1), listener).await;
        assert!(
            result.is_ok(),
            "array job submission should trigger scheduler notification"
        );
    }

    // ---- Node deregistration tests ----

    fn start_job_on(cm: &ClusterManager, id: JobId, node: &str) {
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id: id,
            old_state: JobState::Pending,
            new_state: JobState::Running,
            pending_reason: None,
            pending_priority: None,
        });
        cm.apply_operation(&WalOperation::JobStart {
            job_id: id,
            nodes: vec![node.into()],
            resources: scalar_alloc(1, 1000),
            per_node_alloc: per_node_for(&[node], scalar_alloc(1, 1000)),
            srun_step_dispatch: false,
            run_attempt: 0,
        });
    }

    fn start_srun_job_on(cm: &ClusterManager, id: JobId, node: &str) {
        cm.apply_operation(&WalOperation::JobStateChange {
            job_id: id,
            old_state: JobState::Pending,
            new_state: JobState::Running,
            pending_reason: None,
            pending_priority: None,
        });
        cm.apply_operation(&WalOperation::JobStart {
            job_id: id,
            nodes: vec![node.into()],
            resources: scalar_alloc(1, 1000),
            per_node_alloc: per_node_for(&[node], scalar_alloc(1, 1000)),
            srun_step_dispatch: true,
            run_attempt: 0,
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn node_state_change_to_down_evicts_running_jobs() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        register_node(&cm, "n1", 4, 8000);
        let id = submit_and_wait(&cm, basic_spec("evict-me"));
        start_job_on(&cm, id, "n1");
        assert_eq!(cm.get_job(id).unwrap().state, JobState::Running);

        let resp = cm.apply_operation(&WalOperation::NodeStateChange {
            name: "n1".into(),
            old_state: NodeState::Allocated,
            new_state: NodeState::Down,
            reason: Some("heartbeat timeout".into()),
            admin_locked: false,
        });
        assert_eq!(resp.jobs_finalized.len(), 1);
        assert_eq!(resp.jobs_finalized[0].job_id, id);
        assert_eq!(resp.jobs_finalized[0].state, JobState::NodeFail);

        let job = cm.get_job(id).unwrap();
        assert_eq!(job.state, JobState::NodeFail);
        assert_eq!(job.exit_code, Some(-1));

        let node = cm.get_node("n1").unwrap();
        assert_eq!(node.state, NodeState::Down);
        assert_eq!(node.alloc_resources.cpus, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn node_state_change_to_down_no_jobs_is_clean() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        register_node(&cm, "n1", 4, 8000);

        let resp = cm.apply_operation(&WalOperation::NodeStateChange {
            name: "n1".into(),
            old_state: NodeState::Idle,
            new_state: NodeState::Down,
            reason: None,
            admin_locked: false,
        });
        assert!(resp.jobs_finalized.is_empty());
        assert_eq!(cm.get_node("n1").unwrap().state, NodeState::Down);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn node_remove_deletes_node() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        register_node(&cm, "n1", 4, 8000);
        assert!(cm.get_node("n1").is_some());

        cm.apply_operation(&WalOperation::NodeRemove {
            name: "n1".into(),
            reason: Some("decommission".into()),
        });
        assert!(cm.get_node("n1").is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn node_remove_evicts_and_deletes() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        register_node(&cm, "n1", 4, 8000);
        let id = submit_and_wait(&cm, basic_spec("j"));
        start_job_on(&cm, id, "n1");

        let resp = cm.apply_operation(&WalOperation::NodeRemove {
            name: "n1".into(),
            reason: None,
        });
        assert_eq!(resp.jobs_finalized.len(), 1);
        assert_eq!(resp.jobs_finalized[0].state, JobState::NodeFail);
        assert_eq!(cm.get_job(id).unwrap().state, JobState::NodeFail);
        assert!(cm.get_node("n1").is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drain_node_sets_draining_with_running_jobs() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        register_node(&cm, "n1", 4, 8000);
        let id = submit_and_wait(&cm, basic_spec("drain-job"));
        start_job_on(&cm, id, "n1");

        cm.drain_node("n1", Some("maintenance".into())).unwrap();
        wait_for("n1 draining", || {
            cm.get_node("n1")
                .is_some_and(|n| n.state == NodeState::Draining)
        });

        let node = cm.get_node("n1").unwrap();
        assert!(node.admin_locked);
        assert_eq!(node.state_reason.as_deref(), Some("maintenance"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drain_node_sets_drain_without_running_jobs() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        register_node(&cm, "n1", 4, 8000);

        cm.drain_node("n1", None).unwrap();
        wait_for("n1 drain", || {
            cm.get_node("n1")
                .is_some_and(|n| n.state == NodeState::Drain)
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remove_node_rejects_running_without_force() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        register_node(&cm, "n1", 4, 8000);
        let id = submit_and_wait(&cm, basic_spec("j"));
        start_job_on(&cm, id, "n1");

        let err = cm.remove_node("n1", false, None);
        assert!(err.is_err());
        assert!(cm.get_node("n1").is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remove_node_force_evicts_and_removes() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        register_node(&cm, "n1", 4, 8000);
        let id = submit_and_wait(&cm, basic_spec("j"));
        start_job_on(&cm, id, "n1");

        cm.remove_node("n1", true, Some("bad node".into())).unwrap();
        wait_for("n1 removed", || cm.get_node("n1").is_none());

        assert_eq!(cm.get_job(id).unwrap().state, JobState::NodeFail);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn multinode_eviction_frees_all_nodes() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        register_node(&cm, "n1", 4, 8000);
        register_node(&cm, "n2", 4, 8000);

        let id = submit_and_wait(&cm, basic_spec("multi"));

        let alloc = scalar_alloc(2, 2000);
        let per_node = per_node_for(&["n1", "n2"], scalar_alloc(1, 1000));
        cm.start_job(id, vec!["n1".into(), "n2".into()], alloc, per_node)
            .unwrap();
        settle(&cm, id, JobState::Running);

        assert_eq!(cm.get_node("n1").unwrap().alloc_resources.cpus, 1);
        assert_eq!(cm.get_node("n2").unwrap().alloc_resources.cpus, 1);

        let evicted = cm
            .remove_node("n1", true, Some("evict test".into()))
            .unwrap();
        wait_for("n1 removed", || cm.get_node("n1").is_none());

        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].job_id, id);
        assert_eq!(cm.get_job(id).unwrap().state, JobState::NodeFail);

        let n2 = cm.get_node("n2").unwrap();
        assert_eq!(
            n2.alloc_resources.cpus, 0,
            "peer node n2 must have allocations freed"
        );
        assert_eq!(n2.alloc_resources.memory_mb, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn draining_to_drain_on_last_job_complete() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        register_node(&cm, "n1", 4, 8000);
        let id = submit_and_wait(&cm, basic_spec("drain-job"));
        start_job_on(&cm, id, "n1");

        cm.drain_node("n1", None).unwrap();
        wait_for("n1 draining", || {
            cm.get_node("n1")
                .is_some_and(|n| n.state == NodeState::Draining)
        });

        cm.apply_operation(&WalOperation::JobNodeComplete {
            job_id: id,
            node_name: "n1".into(),
            exit_code: 0,
            signal: 0,
        });

        let node = cm.get_node("n1").unwrap();
        assert_eq!(node.state, NodeState::Drain);
    }

    // --- Direct evict_job unit tests ---

    fn make_running_job(job_id: JobId, nodes: &[&str], cpus_per_node: u32) -> Job {
        let mut spec = basic_spec("evict-test");
        spec.cpus_per_task = cpus_per_node;
        let mut job = Job::new(job_id, spec);
        job.state = JobState::Running;
        job.start_time = Some(Utc::now());
        let node_list: Vec<String> = nodes.iter().map(|n| (*n).to_string()).collect();
        let total_cpus = cpus_per_node * nodes.len() as u32;
        job.allocated_nodes = node_list;
        job.allocated_resources = Some(ResourceAllocations::with_scalar(total_cpus, 0));
        job.per_node_alloc = nodes
            .iter()
            .map(|n| {
                (
                    (*n).to_string(),
                    ResourceAllocations::with_scalar(cpus_per_node, 0),
                )
            })
            .collect();
        job
    }

    fn make_test_node(name: &str, total_cpus: u32, alloc_cpus: u32) -> Node {
        let mut node = Node::new(
            name.into(),
            ResourceSet {
                cpus: total_cpus,
                ..Default::default()
            },
        );
        node.state = if alloc_cpus > 0 {
            NodeState::Allocated
        } else {
            NodeState::Idle
        };
        node.alloc_resources = ResourceAllocations::with_scalar(alloc_cpus, 0);
        node
    }

    #[test]
    fn evict_job_returns_none_for_missing_job() {
        let mut jobs = HashMap::new();
        let mut nodes = HashMap::new();
        let result = ClusterManager::evict_job_locked(
            999,
            &mut jobs,
            &mut nodes,
            Utc::now(),
            PendingReason::NodeDown,
        );
        assert!(result.is_none());
    }

    #[test]
    fn evict_job_transitions_running_to_nodefail() {
        let mut jobs = HashMap::new();
        let mut nodes = HashMap::new();
        jobs.insert(1, make_running_job(1, &["n1"], 2));
        nodes.insert("n1".into(), make_test_node("n1", 4, 2));

        let fin = ClusterManager::evict_job_locked(
            1,
            &mut jobs,
            &mut nodes,
            Utc::now(),
            PendingReason::NodeDown,
        )
        .unwrap();
        assert_eq!(fin.job_id, 1);
        assert_eq!(fin.state, JobState::NodeFail);
        assert_eq!(fin.exit_code, -1);

        let job = &jobs[&1];
        assert_eq!(job.state, JobState::NodeFail);
        assert_eq!(job.exit_code, Some(-1));
        assert!(job.end_time.is_some());
        assert_eq!(job.pending_reason, PendingReason::NodeDown);
    }

    #[test]
    fn evict_job_frees_allocations_on_all_nodes() {
        let mut jobs = HashMap::new();
        let mut nodes = HashMap::new();
        jobs.insert(1, make_running_job(1, &["n1", "n2"], 2));
        nodes.insert("n1".into(), make_test_node("n1", 4, 2));
        nodes.insert("n2".into(), make_test_node("n2", 4, 2));

        ClusterManager::evict_job_locked(
            1,
            &mut jobs,
            &mut nodes,
            Utc::now(),
            PendingReason::NodeDown,
        );

        assert_eq!(nodes["n1"].alloc_resources.cpus, 0);
        assert_eq!(nodes["n2"].alloc_resources.cpus, 0);
    }

    #[test]
    fn evict_job_returns_none_for_terminal_job() {
        let mut jobs = HashMap::new();
        let mut nodes = HashMap::new();
        let mut job = make_running_job(1, &["n1"], 2);
        job.state = JobState::Completed;
        jobs.insert(1, job);
        nodes.insert("n1".into(), make_test_node("n1", 4, 2));

        let result = ClusterManager::evict_job_locked(
            1,
            &mut jobs,
            &mut nodes,
            Utc::now(),
            PendingReason::NodeDown,
        );
        assert!(result.is_none());
    }

    #[test]
    fn evict_job_finalizes_suspended_time() {
        let mut jobs = HashMap::new();
        let mut nodes = HashMap::new();
        let suspended_at = Utc::now() - chrono::Duration::seconds(30);
        let mut job = make_running_job(1, &["n1"], 2);
        job.state = JobState::Suspended;
        job.suspended_at = Some(suspended_at);
        job.suspended_secs = 10;
        jobs.insert(1, job);
        nodes.insert("n1".into(), make_test_node("n1", 4, 2));

        ClusterManager::evict_job_locked(
            1,
            &mut jobs,
            &mut nodes,
            Utc::now(),
            PendingReason::NodeDown,
        );

        let job = &jobs[&1];
        assert!(job.suspended_at.is_none());
        assert!(job.suspended_secs >= 40, "should accumulate ~30s more");
    }

    #[test]
    fn evict_job_transitions_completing_to_nodefail() {
        let mut jobs = HashMap::new();
        let mut nodes = HashMap::new();
        let mut job = make_running_job(1, &["n1"], 2);
        job.state = JobState::Completing;
        jobs.insert(1, job);
        nodes.insert("n1".into(), make_test_node("n1", 4, 2));

        let fin = ClusterManager::evict_job_locked(
            1,
            &mut jobs,
            &mut nodes,
            Utc::now(),
            PendingReason::NodeDown,
        )
        .unwrap();
        assert_eq!(fin.state, JobState::NodeFail);
        assert_eq!(jobs[&1].state, JobState::NodeFail);
        assert_eq!(nodes["n1"].alloc_resources.cpus, 0);
    }

    #[test]
    fn evict_job_transitions_draining_node_to_drain() {
        let mut jobs = HashMap::new();
        let mut nodes = HashMap::new();
        jobs.insert(1, make_running_job(1, &["n1"], 2));
        let mut node = make_test_node("n1", 4, 2);
        node.state = NodeState::Draining;
        nodes.insert("n1".into(), node);

        ClusterManager::evict_job_locked(
            1,
            &mut jobs,
            &mut nodes,
            Utc::now(),
            PendingReason::NodeDown,
        );

        assert_eq!(nodes["n1"].state, NodeState::Drain);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn finish_srun_job_completes_running_allocation() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 4, 8000);
        let id = submit_and_wait(&cm, srun_spec("srun-alloc"));
        start_srun_job_on(&cm, id, "n1");

        let returned = cm.finish_srun_job(id, 0, "testuser").unwrap();
        assert_eq!(returned.job_id, id);
        settle(&cm, id, JobState::Completed);
        assert_eq!(cm.get_job(id).unwrap().exit_code, Some(0));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn finish_srun_job_rejects_non_srun_job() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 4, 8000);
        let id = submit_and_wait(&cm, basic_spec("batch"));
        start_job_on(&cm, id, "n1");

        assert!(matches!(
            cm.finish_srun_job(id, 0, "testuser"),
            Err(SrunCompleteError::NotSrunJob(j)) if j == id
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn finish_srun_job_rejects_terminal_job() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 4, 8000);
        let id = submit_and_wait(&cm, srun_spec("srun-done"));
        start_srun_job_on(&cm, id, "n1");
        cm.finish_srun_job(id, 0, "testuser").unwrap();
        settle(&cm, id, JobState::Completed);

        assert!(matches!(
            cm.finish_srun_job(id, 0, "testuser"),
            Err(SrunCompleteError::AlreadyTerminal {
                job_id,
                state: JobState::Completed,
            }) if job_id == id
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn finish_srun_job_rejects_wrong_user() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 4, 8000);
        let id = submit_and_wait(&cm, srun_spec("srun-owner"));
        start_srun_job_on(&cm, id, "n1");

        assert!(matches!(
            cm.finish_srun_job(id, 0, "otheruser"),
            Err(SrunCompleteError::NotOwner { job_id, user }) if job_id == id && user == "otheruser"
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn finish_srun_job_rejects_batch_fallback_srun() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 4, 8000);
        let id = submit_and_wait(&cm, srun_spec("srun-batch-fallback"));
        start_job_on(&cm, id, "n1");

        assert!(matches!(
            cm.finish_srun_job(id, 0, "testuser"),
            Err(SrunCompleteError::NotStepDispatch(j)) if j == id
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn finish_srun_job_not_found() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        assert!(matches!(
            cm.finish_srun_job(999, 0, "testuser"),
            Err(SrunCompleteError::NotFound(999))
        ));
    }
}
