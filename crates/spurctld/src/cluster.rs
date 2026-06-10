// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use tracing::{debug, info, warn};

use spur_core::accounting::{Qos, TresRecord, TresType};
use spur_core::config::SlurmConfig;
use spur_core::job::{Job, JobId, JobSpec, JobState, NodeCompleteError, PendingReason};
use spur_core::node::{Node, NodeEvent, NodeSource, NodeState};
use spur_core::partition::Partition;
use spur_core::qos::{check_qos_limits, QosCheckResult};
use spur_core::reservation::Reservation;
use spur_core::resource::ResourceSet;
use spur_core::step::{JobStep, StepState, STEP_BATCH};
use spur_core::wal::WalOperation;
use spur_metrics::job::JobMetricsSnapshot;
use spur_metrics::node::NodeMetricsSnapshot;

use crate::accounting::AccountingNotifier;
use crate::fairshare_cache::FairshareCache;
use crate::raft::{ClientResponse, JobFinalized, SpurRaft, StateMachineApply};

/// Result of recording a per-node completion report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeCompleteResult {
    /// Node recorded; waiting for remaining nodes.
    Completing,
    /// All allocated nodes have reported; job is now terminal.
    AllDone { state: JobState, exit_code: i32 },
    /// Job was already in a terminal state (duplicate or race with cancel/timeout).
    AlreadyTerminal,
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
    license_pool: RwLock<HashMap<String, u64>>,
    hostname_aliases: RwLock<HashMap<String, String>>,
    raft: RwLock<Option<SpurRaft>>,
    accounting: RwLock<Option<AccountingNotifier>>,
    fairshare_cache: Arc<FairshareCache>,
}

impl ClusterManager {
    pub fn new(config: SlurmConfig, _state_dir: &Path) -> anyhow::Result<Self> {
        let partitions = config.build_partitions();
        let license_pool = config.licenses.clone();
        let fairshare_cache = Arc::new(FairshareCache::new());

        let cm = Self {
            config,
            jobs: RwLock::new(HashMap::new()),
            nodes: RwLock::new(HashMap::new()),
            partitions: RwLock::new(partitions),
            reservations: RwLock::new(Vec::new()),
            steps: RwLock::new(HashMap::new()),
            next_job_id: AtomicU32::new(1),
            license_pool: RwLock::new(license_pool),
            hostname_aliases: RwLock::new(HashMap::new()),
            raft: RwLock::new(None),
            accounting: RwLock::new(None),
            fairshare_cache,
        };

        info!("cluster manager initialized (state will be recovered via Raft)");

        Ok(cm)
    }

    /// Submit a new job. If it has an array spec, expand into individual tasks.
    pub fn submit_job(&self, mut spec: JobSpec) -> anyhow::Result<JobId> {
        apply_default_partition(&mut spec, &self.partitions.read());
        self.validate_partition(&spec)?;

        // Reject unknown/malformed dependency types up front so users get a
        // clear error instead of a silently-deadlocked job (e.g. `expand:N`).
        // This validates syntax only — the dependency *target* is intentionally
        // not checked for existence here (matching Slurm), so e.g. `after:9999`
        // against a nonexistent job is accepted and resolves as satisfiable.
        if !spec.dependency.is_empty() {
            spur_core::dependency::try_parse_dependencies(&spec.dependency)
                .map_err(|e| anyhow::anyhow!("invalid dependency: {}", e))?;
        }

        let job_id = self.next_job_id.fetch_add(1, Ordering::SeqCst);
        let specs = expand_job_specs(spec, job_id)?;

        for task_spec in specs {
            let task_id = if task_spec.array_job_id.is_some() {
                self.next_job_id.fetch_add(1, Ordering::SeqCst)
            } else {
                job_id
            };
            self.propose(WalOperation::JobSubmit {
                job_id: task_id,
                spec: Box::new(task_spec),
            })?;
        }

        info!(job_id, "job submitted");
        Ok(job_id)
    }

    /// Validate partition constraints: access control and node limits.
    fn validate_partition(&self, spec: &JobSpec) -> anyhow::Result<()> {
        let partition_name = match spec.partition.as_ref() {
            Some(p) if !p.is_empty() => p,
            _ => return Ok(()), // No partition specified — default, no restrictions
        };

        let partitions = self.partitions.read();
        let part = match partitions.iter().find(|p| p.name == *partition_name) {
            Some(p) => p,
            None => anyhow::bail!("partition '{}' not found", partition_name),
        };

        // Check partition state
        if part.state != spur_core::partition::PartitionState::Up {
            anyhow::bail!("partition '{}' is {}", partition_name, part.state.display());
        }

        // Check allow_accounts (if non-empty, user's account must be in the list)
        if !part.allow_accounts.is_empty() {
            let account = spec.account.as_deref().unwrap_or("");
            if !part.allow_accounts.iter().any(|a| a == account) {
                anyhow::bail!(
                    "account '{}' not allowed on partition '{}'",
                    account,
                    partition_name
                );
            }
        }

        // Check deny_accounts
        if let Some(ref account) = spec.account {
            if part.deny_accounts.iter().any(|a| a == account) {
                anyhow::bail!(
                    "account '{}' denied on partition '{}'",
                    account,
                    partition_name
                );
            }
        }

        // Check max_nodes
        if let Some(max) = part.max_nodes {
            if spec.num_nodes > max {
                anyhow::bail!(
                    "requested {} nodes exceeds partition '{}' max of {}",
                    spec.num_nodes,
                    partition_name,
                    max
                );
            }
        }

        // Check max_time
        if let (Some(max_mins), Some(ref tl)) = (part.max_time_minutes, &spec.time_limit) {
            let requested_mins = tl.num_minutes() as u32;
            if requested_mins > max_mins {
                anyhow::bail!(
                    "requested time {} min exceeds partition '{}' max of {} min",
                    requested_mins,
                    partition_name,
                    max_mins
                );
            }
        }

        Ok(())
    }

    /// Get a job by ID.
    pub fn get_job(&self, job_id: JobId) -> Option<Job> {
        self.jobs.read().get(&job_id).cloned()
    }

    /// Get a job by ID for display, synthesizing an aggregate record when the id
    /// is an array *parent* (whose own job record never exists — Spur stores
    /// only per-task jobs). This makes `spur show job <array_parent_id>` return
    /// the array's aggregate state instead of empty output, matching Slurm's
    /// `scontrol show job <array_job_id>`.
    ///
    /// The synthesized job borrows the first task's spec, reports the aggregate
    /// state (PENDING while unfinished), and uses the earliest task start / latest
    /// task end for timing. It is never stored or scheduled.
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
        // Present as the parent: clear the per-task id, keep array_job_id so the
        // proto carries the array linkage.
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
        // Worst exit code across tasks (mirrors derived_completion semantics).
        // Only meaningful once the array is terminal; leave None while running so
        // a still-pending aggregate doesn't read as "0 / success".
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

    /// Get jobs matching filters.
    pub fn get_jobs(
        &self,
        states: &[JobState],
        user: Option<&str>,
        partition: Option<&str>,
        account: Option<&str>,
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

        // For explicitly-requested ids that matched no stored job, synthesize an
        // array-parent aggregate (Spur stores only per-task jobs, so a query for
        // the parent id would otherwise return nothing). The read lock above is
        // released before get_job_for_display, which takes its own lock.
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
        if let Some(f) = resp.job_finalized {
            self.run_job_finalized_side_effects(f);
        }

        info!(job_id, "job deadline passed — transitioned to DEADLINE");
        Ok(())
    }

    /// Cancel a job.
    pub fn cancel_job(&self, job_id: JobId, _user: &str) -> anyhow::Result<()> {
        {
            let jobs = self.jobs.read();
            let job = jobs
                .get(&job_id)
                .ok_or_else(|| anyhow::anyhow!("job {} not found", job_id))?;
            if job.state.is_terminal() {
                anyhow::bail!("job {} is already {:?}", job_id, job.state);
            }
        }

        // Use JobComplete (not JobStateChange) so that resource deallocation
        // fires for any allocated nodes. For pending jobs, allocated_nodes is empty
        // so the deallocation loop is a no-op.
        let resp = self.propose(WalOperation::JobComplete {
            job_id,
            exit_code: -1,
            state: JobState::Cancelled,
        })?;
        if let Some(f) = resp.job_finalized {
            self.run_job_finalized_side_effects(f);
        }

        info!(job_id, "job cancelled");
        Ok(())
    }

    /// Start a job on specific nodes.
    pub fn start_job(
        &self,
        job_id: JobId,
        node_names: Vec<String>,
        resources: ResourceSet,
    ) -> anyhow::Result<()> {
        // Validate job exists and can transition
        let old_state;
        let spec_for_notify;
        {
            let jobs = self.jobs.read();
            let job = jobs
                .get(&job_id)
                .ok_or_else(|| anyhow::anyhow!("job {} not found", job_id))?;
            old_state = job.state;
            spec_for_notify = job.spec.clone();
            if job.state != JobState::Pending {
                anyhow::bail!("job {} cannot start from state {:?}", job_id, job.state);
            }
        }

        // propose() handles: state transition, resource allocation, license subtraction
        self.propose(WalOperation::JobStateChange {
            job_id,
            old_state,
            new_state: JobState::Running,
        })?;
        self.propose(WalOperation::JobStart {
            job_id,
            nodes: node_names.clone(),
            resources: resources.clone(),
        })?;

        // Batch step creation (not WAL-tracked, recreated on apply)
        let node_count = node_names.len().max(1) as u32;
        let per_node = ResourceSet {
            cpus: resources.cpus / node_count,
            memory_mb: resources.memory_mb / node_count as u64,
            gpus: resources.gpus.clone(),
            generic: resources
                .generic
                .iter()
                .map(|(k, v)| (k.clone(), v / node_count as u64))
                .collect(),
        };
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
        self.create_step(job_id, STEP_BATCH, batch_step);

        if spec_for_notify
            .mail_type
            .iter()
            .any(|t| t == "BEGIN" || t == "ALL")
        {
            self.send_notification(job_id, "BEGIN", &spec_for_notify);
        }

        if let Some(ref notifier) = *self.accounting.read() {
            notifier.notify_job_start(
                job_id,
                spec_for_notify.user.clone(),
                spec_for_notify.account.clone().unwrap_or_default(),
                spec_for_notify.partition.clone().unwrap_or_default(),
                &resources,
                Utc::now(),
            );
        }

        debug!(job_id, "job started");
        Ok(())
    }

    /// Record completion from one allocated node (multi-node COMPLETING flow).
    pub fn node_complete(
        &self,
        job_id: JobId,
        node_name: &str,
        exit_code: i32,
    ) -> Result<NodeCompleteResult, NodeCompleteError> {
        {
            let jobs = self.jobs.read();
            let job = jobs
                .get(&job_id)
                .ok_or(NodeCompleteError::JobNotFound { job_id })?;
            if job.state.is_terminal() {
                return Ok(NodeCompleteResult::AlreadyTerminal);
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
            })
            .map_err(|source| NodeCompleteError::RaftPropose { source })?;

        if let Some(f) = resp.job_finalized {
            self.run_job_finalized_side_effects(f);
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
        if let Some(f) = resp.job_finalized {
            self.run_job_finalized_side_effects(f);
        }

        debug!(job_id, exit_code, "job completed");
        Ok(())
    }

    fn run_job_finalized_side_effects(&self, finalized: JobFinalized) {
        self.run_epilog_slurmctld(finalized.job_id);
        self.notify_job_finished(finalized.job_id, finalized.state, finalized.exit_code);
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
            notifier.notify_job_end(job_id, state, exit_code, Utc::now());
        }

        let should_requeue = matches!(
            state,
            JobState::Timeout | JobState::Preempted | JobState::NodeFail
        );
        if should_requeue {
            if let Err(e) = self.maybe_requeue(job_id) {
                warn!(job_id, error = %e, "failed to requeue job");
            }
        }
    }

    /// Requeue a job if spec.requeue is set and attempt limit not exceeded.
    fn maybe_requeue(&self, job_id: JobId) -> anyhow::Result<()> {
        const MAX_REQUEUE: u32 = 3;
        let (should_requeue, old_state) = {
            let jobs = self.jobs.read();
            let Some(job) = jobs.get(&job_id) else {
                return Ok(());
            };
            if !job.spec.requeue || job.requeue_count >= MAX_REQUEUE {
                return Ok(());
            }
            (true, job.state)
        };
        if !should_requeue {
            return Ok(());
        }

        self.propose(WalOperation::JobStateChange {
            job_id,
            old_state,
            new_state: JobState::Pending,
        })?;

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
        self.propose(WalOperation::JobStateChange {
            job_id,
            old_state: JobState::Failed,
            new_state: JobState::Pending,
        })?;

        info!(job_id, from = %old_state, "job requeued after dispatch failure");
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
    ) -> anyhow::Result<()> {
        // Normalize node name: if the agent's hostname doesn't match any config
        // entry, check if there's an unmatched config node it could be aliased to.
        // This handles single-node setups where config says "localhost" but the
        // agent registers with its real hostname.
        let effective_name = {
            let registered_nodes = self.nodes.read();
            let mut matches_config = false;
            for nc in &self.config.nodes {
                if let Ok(hosts) = spur_core::hostlist::expand(&nc.names) {
                    if hosts.contains(&name) {
                        matches_config = true;
                        break;
                    }
                }
            }
            if !matches_config {
                // Agent hostname doesn't match config — find an unmatched config node
                let mut candidate = None;
                for nc in &self.config.nodes {
                    if let Ok(hosts) = spur_core::hostlist::expand(&nc.names) {
                        for host in &hosts {
                            if !registered_nodes.contains_key(host) {
                                candidate = Some(host.clone());
                                break;
                            }
                        }
                        if candidate.is_some() {
                            break;
                        }
                    }
                }
                if let Some(config_name) = candidate {
                    info!(
                        agent_hostname = %name,
                        config_name = %config_name,
                        "node hostname doesn't match config — using config name"
                    );
                    // Store the alias so heartbeats from this hostname find the right node
                    drop(registered_nodes);
                    self.hostname_aliases
                        .write()
                        .insert(name.clone(), config_name.clone());
                    config_name
                } else {
                    name.clone()
                }
            } else {
                name.clone()
            }
        };

        let action = {
            let nodes = self.nodes.read();
            evaluate_registration(nodes.get(&effective_name), &resources)
        };

        match action {
            RegistrationAction::Skip => {
                debug!(node = %effective_name, "node unchanged, skipping");
            }
            RegistrationAction::Update => {
                self.propose(WalOperation::NodeUpdate {
                    name: effective_name.clone(),
                    resources,
                    address,
                    port,
                    wg_pubkey,
                    version,
                })?;
                if let Some(node) = self.nodes.write().get_mut(&effective_name) {
                    node.source = source;
                }
                info!(node = %effective_name, "node updated (resources changed)");
            }
            RegistrationAction::Register => {
                self.propose(WalOperation::NodeRegister {
                    name: effective_name.clone(),
                    resources,
                    address,
                    port,
                    wg_pubkey,
                    version,
                })?;
                if let Some(node) = self.nodes.write().get_mut(&effective_name) {
                    node.source = source;
                    node.agent_start_time = Some(Utc::now());
                }
                info!(node = %effective_name, "node registered");
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
        let effective_name = self
            .hostname_aliases
            .read()
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.to_string());
        let mut nodes = self.nodes.write();
        if let Some(node) = nodes.get_mut(&effective_name) {
            node.cpu_load = cpu_load;
            node.free_memory_mb = free_memory_mb;
            node.last_heartbeat = Some(Utc::now());
            true
        } else {
            false
        }
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
        })?;
        // Set held reason (not WAL-tracked)
        if let Some(job) = self.jobs.write().get_mut(&job_id) {
            job.pending_reason = PendingReason::Held;
        }
        info!(job_id, "job held");
        Ok(())
    }

    /// Release a held job.
    pub fn release_job(&self, job_id: JobId) -> anyhow::Result<()> {
        {
            let jobs = self.jobs.read();
            let job = jobs
                .get(&job_id)
                .ok_or_else(|| anyhow::anyhow!("job {} not found", job_id))?;
            if job.pending_reason != PendingReason::Held {
                anyhow::bail!("job {} is not held", job_id);
            }
        }

        self.propose(WalOperation::JobPriorityChange {
            job_id,
            old_priority: 0,
            new_priority: 1000,
        })?;
        if let Some(job) = self.jobs.write().get_mut(&job_id) {
            job.pending_reason = PendingReason::Priority;
        }
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
        {
            let jobs = self.jobs.read();
            if !jobs.contains_key(&job_id) {
                anyhow::bail!("job {} not found", job_id);
            }
        }

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
                && (node.alloc_resources.cpus > 0 || !node.alloc_resources.gpus.is_empty())
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

    /// Reconcile node liveness state with heartbeat data.
    /// Marks stale nodes Down and recovers nodes whose heartbeat has resumed.
    pub fn check_node_health(&self, timeout_secs: u64) {
        let actions = {
            let nodes = self.nodes.read();
            let refs: Vec<&Node> = nodes.values().collect();
            evaluate_node_health(&refs, Utc::now(), timeout_secs)
        };
        self.apply_health_actions(actions);
    }

    fn apply_health_actions(&self, actions: Vec<HealthAction>) {
        for action in actions {
            match action {
                HealthAction::MarkDown {
                    name,
                    old_state,
                    admin_locked,
                } => {
                    warn!(node = %name, "node marked DOWN (heartbeat timeout)");
                    if let Err(e) = self.propose(WalOperation::NodeStateChange {
                        name,
                        old_state,
                        new_state: NodeState::Down,
                        reason: Some("Not responding".into()),
                        admin_locked,
                    }) {
                        warn!(error = %e, "failed to propose node DOWN");
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
    }

    /// Create a job step.
    pub fn create_step(&self, job_id: JobId, step_id: u32, step: JobStep) {
        self.steps.write().insert((job_id, step_id), step);
        debug!(job_id, step_id, "step created");
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
    /// Recomputes effective priority using age and partition tier before sorting.
    pub fn pending_jobs(&self) -> Vec<Job> {
        let jobs = self.jobs.read();
        let mut pending: Vec<Job> = jobs
            .values()
            .filter(|j| j.state == JobState::Pending && j.pending_reason != PendingReason::Held)
            .cloned()
            .collect();

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

        // QoS enforcement: check per-user limits for jobs with a QoS
        pending.retain(|job| {
            if job.spec.qos.is_none() {
                return true; // No QoS — skip check
            }

            let user = &job.spec.user;

            let running_count = jobs
                .values()
                .filter(|j| j.state == JobState::Running && j.spec.user == *user)
                .count() as u32;

            let submitted_count = jobs
                .values()
                .filter(|j| {
                    (j.state == JobState::Pending || j.state == JobState::Running)
                        && j.spec.user == *user
                })
                .count() as u32;

            // Compute running TRES for this user (total CPUs from running jobs)
            let mut running_tres = TresRecord::new();
            let running_cpus: u64 = jobs
                .values()
                .filter(|j| j.state == JobState::Running && j.spec.user == *user)
                .map(|j| (j.spec.num_tasks * j.spec.cpus_per_task) as u64)
                .sum();
            running_tres.set(TresType::Cpu, running_cpus);

            // Use a default QoS (no limits) — real QoS definitions would come
            // from the accounting database; for now this wires the enforcement
            // path so it's ready when QoS configs are populated.
            let qos = Qos::default();

            match check_qos_limits(job, &qos, running_count, submitted_count, &running_tres) {
                QosCheckResult::Allowed => true,
                QosCheckResult::Blocked(_reason) => false,
            }
        });

        // License enforcement: check cluster-wide license pool
        {
            let pool = self.license_pool.read();
            pending.retain(|job| {
                let lic_req = extract_license_requirements(&job.spec);
                for (lic, count) in &lic_req {
                    let available = pool.get(lic).copied().unwrap_or(0);
                    if available < *count {
                        return false; // Not enough licenses
                    }
                }
                true
            });
        }

        // Reservation validation: reject jobs targeting expired/nonexistent reservations
        {
            let reservations = self.get_reservations();
            let now = Utc::now();
            pending.retain(|job| {
                if let Some(ref res_name) = job.spec.reservation {
                    if res_name.is_empty() {
                        return true;
                    }
                    match reservations.iter().find(|r| r.name == *res_name) {
                        Some(r) => {
                            if !r.is_active(now) {
                                return false; // Reservation not active yet or expired
                            }
                            r.allows_user(&job.spec.user, job.spec.account.as_deref())
                        }
                        None => false, // Reservation doesn't exist
                    }
                } else {
                    true
                }
            });
        }

        // Recompute effective priority with age + partition tier
        let now = Utc::now();
        let partitions = self.partitions.read();
        for job in &mut pending {
            let age_minutes = (now - job.submit_time).num_minutes().max(0);
            let partition_tier = job
                .spec
                .partition
                .as_ref()
                .and_then(|pname| partitions.iter().find(|p| p.name == *pname))
                .map(|p| p.priority_tier)
                .unwrap_or(1);
            let fair_share = self
                .fairshare_cache
                .get(&job.spec.user, job.spec.account.as_deref().unwrap_or(""));
            job.priority = spur_sched::priority::effective_priority(
                job.priority,
                fair_share,
                age_minutes,
                partition_tier,
            );
        }

        pending.sort_by_key(|j| std::cmp::Reverse(j.priority));
        pending
    }

    /// Cancel pending jobs whose dependencies can never be satisfied, and tag
    /// still-waiting dependency jobs with `PendingReason::Dependency`.
    ///
    /// Returns the ids of jobs that were cancelled. Run from the scheduler loop
    /// (leader only); takes the write lock that `pending_jobs()` cannot.
    ///
    /// This is what closes the silent-deadlock gap: previously a `Failed`
    /// dependency was merely filtered out of scheduling and the job sat PENDING
    /// forever. Now it is finalized as Cancelled, matching Slurm's
    /// `DependencyNeverSatisfied`.
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
                    || job.pending_reason == PendingReason::Held
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
                    // Don't clobber Held, or a DeadLine reason set by the
                    // deadline-enforcement path (the job is about to transition
                    // to JobState::Deadline) — matches update_pending_reasons().
                    if j.state == JobState::Pending
                        && j.pending_reason != PendingReason::Held
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
            match self.propose(WalOperation::JobComplete {
                job_id: id,
                exit_code: -1,
                state: JobState::Cancelled,
            }) {
                Ok(resp) => {
                    if let Some(f) = resp.job_finalized {
                        self.run_job_finalized_side_effects(f);
                    }
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

    /// Create a new reservation.
    pub fn create_reservation(&self, res: Reservation) -> anyhow::Result<()> {
        let mut reservations = self.reservations.write();
        if reservations.iter().any(|r| r.name == res.name) {
            anyhow::bail!("reservation '{}' already exists", res.name);
        }
        info!(name = %res.name, "reservation created");
        reservations.push(res);
        Ok(())
    }

    /// Update an existing reservation.
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
    ) -> anyhow::Result<()> {
        let mut reservations = self.reservations.write();
        let res = reservations
            .iter_mut()
            .find(|r| r.name == name)
            .ok_or_else(|| anyhow::anyhow!("reservation '{}' not found", name))?;

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

        info!(name, "reservation updated");
        Ok(())
    }

    /// Delete a reservation by name.
    pub fn delete_reservation(&self, name: &str) -> anyhow::Result<()> {
        let mut reservations = self.reservations.write();
        let len_before = reservations.len();
        reservations.retain(|r| r.name != name);
        if reservations.len() == len_before {
            anyhow::bail!("reservation '{}' not found", name);
        }
        info!(name, "reservation deleted");
        Ok(())
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
            if job_entry.pending_reason == PendingReason::Held {
                continue;
            }
            // Don't overwrite a DeadLine reason set by the deadline-enforcement
            // path — the job is about to transition to JobState::Deadline this
            // tick; clobbering with Resources/NodeDown would mislead any
            // observer that polls in between.
            if job_entry.pending_reason == PendingReason::DeadLine {
                continue;
            }

            // Determine the correct reason
            let partition_name = job.spec.partition.as_deref();

            // Check if any node is schedulable in the target partition
            let nodes_in_partition: Vec<&spur_core::node::Node> = cluster_state
                .nodes
                .iter()
                .filter(|n| {
                    if let Some(pname) = partition_name {
                        n.partitions.iter().any(|p| p == pname)
                    } else {
                        true
                    }
                })
                .collect();

            if nodes_in_partition.is_empty() {
                // No nodes in partition at all
                job_entry.pending_reason = PendingReason::Resources;
                continue;
            }

            let all_down = nodes_in_partition.iter().all(|n| !n.state.is_available());

            if all_down {
                job_entry.pending_reason = PendingReason::NodeDown;
                continue;
            }

            // Nodes exist but may be fully allocated — check if any node
            // can satisfy resource requirements with AVAILABLE resources.
            //
            // Issue #65 (reopen of #56): previous check used total_resources,
            // which always returned true for idle nodes even when their
            // available resources (total - alloc) were insufficient because
            // other jobs consumed them. Must use available = total - alloc.
            let required = spur_sched::backfill::job_resource_request(job);
            let has_capable_node = nodes_in_partition.iter().any(|n| {
                if !n.is_schedulable() {
                    return false;
                }
                // Skip nodes fully consumed by existing allocations
                if n.alloc_resources.cpus >= n.total_resources.cpus && n.total_resources.cpus > 0 {
                    return false;
                }
                // Exclusive job needs an idle node (no current allocations)
                if job.spec.exclusive
                    && (n.alloc_resources.cpus > 0 || !n.alloc_resources.gpus.is_empty())
                {
                    return false;
                }
                // Constraint feature check
                if let Some(ref constraint) = job.spec.constraint {
                    let required_features: Vec<&str> = constraint
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .collect();
                    if !required_features
                        .iter()
                        .all(|f| n.features.contains(&f.to_string()))
                    {
                        return false;
                    }
                }
                // Check AVAILABLE resources (total minus already allocated),
                // not just total capacity. This matches what the backfill
                // scheduler actually does when trying to place a job.
                let available = n.total_resources.subtract(&n.alloc_resources);
                available.can_satisfy(&required)
            });

            if !has_capable_node {
                // Resources insufficient or constraints prevent scheduling
                job_entry.pending_reason = PendingReason::Resources;
            } else {
                // Capable nodes exist but currently occupied — backfill will
                // schedule this job once they free up (or higher-priority jobs run)
                job_entry.pending_reason = PendingReason::Priority;
            }
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

    pub fn fairshare_cache(&self) -> &Arc<FairshareCache> {
        &self.fairshare_cache
    }

    /// Persist a mutation via Raft consensus. The apply callback
    /// (`StateMachineApply`) handles in-memory state on all nodes.
    fn complete_job_steps_and_licenses(
        &self,
        job_id: &JobId,
        exit_code: i32,
        timestamp: DateTime<Utc>,
    ) {
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

        let lic_req = if let Some(job) = self.jobs.read().get(job_id) {
            extract_license_requirements(&job.spec)
        } else {
            HashMap::new()
        };
        if !lic_req.is_empty() {
            let mut pool = self.license_pool.write();
            for (lic, count) in &lic_req {
                *pool.entry(lic.clone()).or_insert(0) += count;
            }
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
                job_id, new_state, ..
            } => {
                if let Some(job) = jobs.get_mut(job_id) {
                    if let Err(e) = job.transition(*new_state) {
                        warn!(job_id = *job_id, error = %e, "invalid state transition in WAL apply");
                    }
                    // Requeue: reset allocation fields when returning to Pending
                    if *new_state == JobState::Pending {
                        job.requeue_count += 1;
                        job.start_time = None;
                        job.exit_code = None;
                        job.allocated_nodes.clear();
                        job.allocated_resources = None;
                        job.pending_reason = PendingReason::None;
                    }
                }
            }
            WalOperation::JobStart {
                job_id,
                nodes: node_names,
                resources,
            } => {
                let spec = jobs.get(job_id).map(|j| j.spec.clone());
                if let Some(job) = jobs.get_mut(job_id) {
                    job.start_time = Some(timestamp);
                    job.allocated_nodes = node_names.clone();
                    job.allocated_resources = Some(resources.clone());
                    job.pending_reason = PendingReason::None;
                }
                let node_count = node_names.len().max(1) as u32;
                let per_node = ResourceSet {
                    cpus: resources.cpus / node_count,
                    memory_mb: resources.memory_mb / node_count as u64,
                    gpus: resources.gpus.clone(),
                    generic: resources
                        .generic
                        .iter()
                        .map(|(k, v)| (k.clone(), v / node_count as u64))
                        .collect(),
                };
                for name in node_names {
                    if let Some(node) = nodes.get_mut(name) {
                        node.alloc_resources = node.alloc_resources.add(&per_node);
                        node.update_state_from_alloc();
                    }
                }
                // Subtract licenses
                if let Some(ref spec) = spec {
                    let lic_req = extract_license_requirements(spec);
                    if !lic_req.is_empty() {
                        drop(jobs);
                        drop(nodes);
                        let mut pool = self.license_pool.write();
                        for (lic, count) in &lic_req {
                            if let Some(avail) = pool.get_mut(lic) {
                                *avail = avail.saturating_sub(*count);
                            }
                        }
                        self.next_job_id.store(next_id, Ordering::Relaxed);
                        return ClientResponse::default();
                    }
                }
            }
            WalOperation::JobNodeComplete {
                job_id,
                node_name,
                exit_code,
            } => {
                let finalized = {
                    let Some(job) = jobs.get_mut(job_id) else {
                        return ClientResponse::default();
                    };
                    if job.state.is_terminal() {
                        return ClientResponse::default();
                    }

                    let already_reported = job.node_completions.contains_key(node_name);
                    job.node_completions.insert(node_name.clone(), *exit_code);

                    if let Some(ref total) = job.allocated_resources {
                        if !already_reported {
                            deallocate_job_node(
                                &mut nodes,
                                total,
                                job.allocated_nodes.len(),
                                node_name,
                            );
                        }
                    }

                    if job.state == JobState::Running {
                        if let Err(e) = job.transition(JobState::Completing) {
                            warn!(job_id = *job_id, error = %e, "invalid transition to Completing");
                        }
                        job.end_time = Some(timestamp);
                    }

                    if job.all_nodes_completed() {
                        let (final_state, final_exit) =
                            Job::derived_completion(&job.node_completions);
                        match job.transition(final_state) {
                            Ok(()) => {
                                job.exit_code = Some(final_exit);
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
                    self.complete_job_steps_and_licenses(job_id, final_exit, timestamp);
                    self.next_job_id.store(next_id, Ordering::Relaxed);
                    return ClientResponse {
                        job_finalized: Some(JobFinalized {
                            job_id: *job_id,
                            state: final_state,
                            exit_code: final_exit,
                        }),
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
                    if job.state.is_terminal() {
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
                        response.job_finalized = Some(JobFinalized {
                            job_id: *job_id,
                            state: *state,
                            exit_code: *exit_code,
                        });
                    }
                    job.exit_code = Some(*exit_code);
                    job.end_time = Some(timestamp);
                    freed_nodes = job.allocated_nodes.clone();
                    allocated_resources = job.allocated_resources.clone();
                    already_deallocated = job.node_completions.keys().cloned().collect::<Vec<_>>();
                    job.node_completions.clear();
                } else {
                    return ClientResponse::default();
                }
                // Deallocate node resources not already freed during COMPLETING
                if let Some(ref total) = allocated_resources {
                    for name in &freed_nodes {
                        if already_deallocated.iter().any(|n| n == name) {
                            continue;
                        }
                        deallocate_job_node(&mut nodes, total, freed_nodes.len(), name);
                    }
                }
                drop(jobs);
                drop(nodes);
                self.complete_job_steps_and_licenses(job_id, *exit_code, timestamp);
            }
            WalOperation::JobPriorityChange {
                job_id,
                new_priority,
                ..
            } => {
                if let Some(job) = jobs.get_mut(job_id) {
                    job.priority = *new_priority;
                }
            }
            WalOperation::NodeRegister {
                name,
                resources,
                address,
                port,
                wg_pubkey,
                version,
            } => {
                let mut node = Node::new(name.clone(), resources.clone());
                node.address = Some(address.clone());
                node.port = *port;
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

                // Assign partitions from config
                drop(nodes);
                let partitions = self.partitions.read();
                for part in partitions.iter() {
                    if let Ok(hosts) = spur_core::hostlist::expand(&part.nodes) {
                        if hosts.contains(name) {
                            node.partitions.push(part.name.clone());
                        }
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

                for nc in &self.config.nodes {
                    if let Ok(hosts) = spur_core::hostlist::expand(&nc.names) {
                        if hosts.contains(name) {
                            node.features = nc.features.clone();
                            node.weight = nc.weight;
                            break;
                        }
                    }
                }

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
    hostname_aliases: HashMap<String, String>,
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
            hostname_aliases: self.hostname_aliases.read().clone(),
        };
        serde_json::to_vec(&snap).map_err(Into::into)
    }

    fn restore_from_snapshot(&self, data: &[u8]) {
        if let Ok(snap) = serde_json::from_slice::<ClusterSnapshot>(data) {
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

            *self.license_pool.write() = snap.license_pool;
            *self.hostname_aliases.write() = snap.hostname_aliases;

            self.next_job_id.store(next_id, Ordering::Relaxed);
            info!(
                jobs = jobs.len(),
                nodes = nodes.len(),
                "restored cluster state from Raft snapshot"
            );
        }
    }
}

/// Extract license requirements from a job's GRES list.
/// License GRES entries are formatted as "license:<name>:<count>" or "license:<name>".
fn per_node_resources(total: &ResourceSet, node_count: usize) -> ResourceSet {
    let node_count = node_count.max(1) as u32;
    ResourceSet {
        cpus: total.cpus / node_count,
        memory_mb: total.memory_mb / node_count as u64,
        gpus: total.gpus.clone(),
        generic: total
            .generic
            .iter()
            .map(|(k, v)| (k.clone(), v / node_count as u64))
            .collect(),
    }
}

fn deallocate_job_node(
    nodes: &mut HashMap<String, Node>,
    total: &ResourceSet,
    node_count: usize,
    node_name: &str,
) {
    let per_node = per_node_resources(total, node_count);
    if let Some(node) = nodes.get_mut(node_name) {
        node.alloc_resources = node.alloc_resources.subtract(&per_node);
        node.update_state_from_alloc();
        if node.state == NodeState::Draining
            && node.alloc_resources.cpus == 0
            && node.alloc_resources.gpus.is_empty()
        {
            node.state = NodeState::Drain;
        }
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
    if spec.partition.is_none() {
        if let Some(default_part) = partitions.iter().find(|p| p.is_default) {
            spec.partition = Some(default_part.name.clone());
        } else if let Some(first) = partitions.first() {
            spec.partition = Some(first.name.clone());
        }
    }
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
    use spur_core::resource::ResourceSet;
    use spur_metrics::job::JobMetricsSnapshot;
    use tempfile::TempDir;

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
                max_time: None,
                default_time: None,
                max_nodes: None,
                min_nodes: 1,
                allow_accounts: Vec::new(),
                allow_groups: Vec::new(),
                priority_tier: 1,
                preempt_mode: String::new(),
            }],
            nodes: Vec::new(),
            network: Default::default(),
            logging: Default::default(),
            kubernetes: Default::default(),
            notifications: Default::default(),
            power: Default::default(),
            federation: Default::default(),
            topology: None,
            isolation: Default::default(),
            licenses: HashMap::new(),
            update: Default::default(),
            metrics: Default::default(),
            hooks: Default::default(),
        }
    }

    async fn test_cluster(dir: &TempDir) -> Arc<ClusterManager> {
        let cm = Arc::new(ClusterManager::new(test_config(), dir.path()).unwrap());
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

        let resources = ResourceSet {
            cpus: 4,
            memory_mb: 8000,
            ..Default::default()
        };
        cm.apply_operation(&WalOperation::JobStart {
            job_id: 1,
            nodes: vec!["node1".into()],
            resources: resources.clone(),
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
        });
        cm.apply_operation(&WalOperation::JobStart {
            job_id: 1,
            nodes: vec!["node1".into()],
            resources: ResourceSet {
                cpus: 4,
                memory_mb: 8000,
                ..Default::default()
            },
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

        let resources = ResourceSet {
            cpus: 2,
            memory_mb: 4000,
            ..Default::default()
        };
        cm.start_job(job_id, vec!["worker1".into()], resources)
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
        });
        cm.apply_operation(&WalOperation::JobStart {
            job_id: 1,
            nodes: vec!["worker1".into()],
            resources: ResourceSet {
                cpus: 2,
                memory_mb: 4000,
                ..Default::default()
            },
        });

        cm.apply_operation(&WalOperation::JobNodeComplete {
            job_id: 1,
            node_name: "worker1".into(),
            exit_code: 0,
        });

        let job = cm.get_job(1).unwrap();
        assert_eq!(job.state, JobState::Completed);
        assert_eq!(job.exit_code, Some(0));
        assert!(job.node_completions.is_empty());
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
        });
        cm.apply_operation(&WalOperation::JobStart {
            job_id: 1,
            nodes: vec!["n1".into(), "n2".into(), "n3".into()],
            resources: ResourceSet {
                cpus: 6,
                memory_mb: 12000,
                ..Default::default()
            },
        });

        cm.apply_operation(&WalOperation::JobNodeComplete {
            job_id: 1,
            node_name: "n1".into(),
            exit_code: 0,
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
        });
        assert_eq!(cm.get_job(1).unwrap().state, JobState::Completing);

        cm.apply_operation(&WalOperation::JobNodeComplete {
            job_id: 1,
            node_name: "n3".into(),
            exit_code: 42,
        });

        let job = cm.get_job(1).unwrap();
        assert_eq!(job.state, JobState::Failed);
        assert_eq!(job.exit_code, Some(42));
        for name in ["n1", "n2", "n3"] {
            assert_eq!(cm.get_node(name).unwrap().alloc_resources.cpus, 0);
        }
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
        });
        cm.apply_operation(&WalOperation::JobStart {
            job_id: 1,
            nodes: vec!["n1".into(), "n2".into()],
            resources: ResourceSet {
                cpus: 4,
                memory_mb: 8000,
                ..Default::default()
            },
        });

        let r1 = cm.apply_operation(&WalOperation::JobNodeComplete {
            job_id: 1,
            node_name: "n1".into(),
            exit_code: 0,
        });
        assert!(r1.job_finalized.is_none());
        assert_eq!(cm.get_job(1).unwrap().state, JobState::Completing);

        let r2 = cm.apply_operation(&WalOperation::JobNodeComplete {
            job_id: 1,
            node_name: "n2".into(),
            exit_code: 0,
        });
        let f = r2.job_finalized.expect("last node should finalize");
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
        });
        cm.apply_operation(&WalOperation::JobStart {
            job_id: 1,
            nodes: vec!["worker1".into()],
            resources: ResourceSet {
                cpus: 2,
                memory_mb: 4000,
                ..Default::default()
            },
        });

        let resp = cm.apply_operation(&WalOperation::JobComplete {
            job_id: 1,
            exit_code: 0,
            state: JobState::Completed,
        });
        let f = resp.job_finalized.expect("JobComplete should finalize");
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
        });
        cm.apply_operation(&WalOperation::JobStart {
            job_id: 1,
            nodes: vec!["worker1".into()],
            resources: ResourceSet {
                cpus: 2,
                memory_mb: 4000,
                ..Default::default()
            },
        });

        let first = cm.apply_operation(&WalOperation::JobComplete {
            job_id: 1,
            exit_code: 0,
            state: JobState::Completed,
        });
        first
            .job_finalized
            .expect("first JobComplete should finalize");
        let node = cm.get_node("worker1").unwrap();
        assert_eq!(node.alloc_resources.cpus, 0);
        assert_eq!(node.alloc_resources.memory_mb, 0);

        let second = cm.apply_operation(&WalOperation::JobComplete {
            job_id: 1,
            exit_code: -1,
            state: JobState::Cancelled,
        });
        assert!(second.job_finalized.is_none());

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
        });
        cm.apply_operation(&WalOperation::JobStart {
            job_id: 1,
            nodes: vec!["n1".into(), "n2".into(), "n3".into()],
            resources: ResourceSet {
                cpus: 6,
                memory_mb: 12000,
                ..Default::default()
            },
        });
        cm.apply_operation(&WalOperation::JobNodeComplete {
            job_id: 1,
            node_name: "n1".into(),
            exit_code: 0,
        });

        let result = cm.node_complete(1, "n2", 0).unwrap();
        assert_eq!(result, NodeCompleteResult::Completing);
        assert_eq!(cm.get_job(1).unwrap().state, JobState::Completing);
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
        });
        cm.apply_operation(&WalOperation::JobStart {
            job_id: 1,
            nodes: vec!["n1".into(), "n2".into(), "n3".into()],
            resources: ResourceSet {
                cpus: 6,
                memory_mb: 12000,
                ..Default::default()
            },
        });

        cm.apply_operation(&WalOperation::JobNodeComplete {
            job_id: 1,
            node_name: "n1".into(),
            exit_code: 0,
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
        });
        cm.apply_operation(&WalOperation::JobStart {
            job_id: 1,
            nodes: vec!["n1".into(), "n2".into(), "n3".into()],
            resources: ResourceSet {
                cpus: 6,
                memory_mb: 12000,
                ..Default::default()
            },
        });
        cm.apply_operation(&WalOperation::JobNodeComplete {
            job_id: 1,
            node_name: "n1".into(),
            exit_code: 0,
        });

        cm.cancel_job(1, "testuser").unwrap();
        settle(&cm, 1, JobState::Cancelled);

        let result = cm.node_complete(1, "n2", 0).unwrap();
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

        let resources = ResourceSet {
            cpus: 4,
            memory_mb: 8192,
            ..Default::default()
        };
        cm.start_job(job_id, vec!["worker1".into()], resources)
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
        let expected = JobMetricsSnapshot::collect(cm.get_jobs(&[], None, None, None, &[]).iter());
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
        let resources = ResourceSet {
            cpus: 4,
            memory_mb: 8192,
            ..Default::default()
        };
        cm.start_job(job_id, vec!["worker1".into()], resources)
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
        let resources = ResourceSet {
            cpus: 1,
            memory_mb: 1000,
            ..Default::default()
        };
        cm.start_job(job_id, vec!["worker1".into()], resources)
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
    async fn complete_terminal_job_errors() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        let job_id = submit_and_wait(&cm, basic_spec("j"));
        cm.cancel_job(job_id, "u").unwrap();
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

        let resources = ResourceSet {
            cpus: 2,
            memory_mb: 4000,
            ..Default::default()
        };
        cm.start_job(job_id, vec!["worker1".into()], resources)
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
        cm2.restore_from_snapshot(&data);

        assert!(cm2.get_job(1).is_some());
        assert!(cm2.get_node("n1").is_some());
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
        cm.start_job(
            id,
            vec!["locked".into()],
            ResourceSet {
                cpus: 2,
                memory_mb: 4000,
                ..Default::default()
            },
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

        cm.start_job(
            id,
            vec!["n1".into()],
            ResourceSet {
                cpus: 2,
                memory_mb: 4000,
                ..Default::default()
            },
        )
        .unwrap();
        settle(&cm, id, JobState::Running);

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
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn requeue_job_frees_node_resources() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "n1", 4, 8000);
        let id = submit_and_wait(&cm, basic_spec("dispatch-fail"));

        cm.start_job(
            id,
            vec!["n1".into()],
            ResourceSet {
                cpus: 2,
                memory_mb: 4000,
                ..Default::default()
            },
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
            node.alloc_resources.gpus.is_empty(),
            "node GPUs must be freed after requeue"
        );
        assert_eq!(node.state, NodeState::Idle, "node should return to Idle");
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
        let got = cm.get_jobs(&[], None, None, None, &[10]);
        assert_eq!(got.len(), 1, "parent id should synthesize one record");
        assert_eq!(got[0].job_id, 10);
        assert_eq!(got[0].state, JobState::Pending);
        assert_eq!(got[0].spec.array_job_id, Some(10));

        // Querying a real task id still returns that task, not the parent.
        let got_task = cm.get_jobs(&[], None, None, None, &[11]);
        assert_eq!(got_task.len(), 1);
        assert_eq!(got_task[0].job_id, 11);

        // Unknown id → empty.
        assert!(cm.get_jobs(&[], None, None, None, &[999]).is_empty());
    }
}
