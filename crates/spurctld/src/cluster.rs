use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use chrono::Utc;
use parking_lot::RwLock;
use tracing::{debug, info, warn};

use spur_core::accounting::{Qos, TresRecord, TresType};
use spur_core::config::SlurmConfig;
use spur_core::job::{Job, JobId, JobSpec, JobState, PendingReason};
use spur_core::node::{Node, NodeSource, NodeState};
use spur_core::partition::Partition;
use spur_core::qos::{check_qos_limits, QosCheckResult};
use spur_core::reservation::Reservation;
use spur_core::resource::ResourceSet;
use spur_core::step::{JobStep, StepState, STEP_BATCH};
use spur_core::wal::WalOperation;

use crate::raft::{SpurRaft, StateMachineApply};

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
}

impl ClusterManager {
    pub fn new(config: SlurmConfig, _state_dir: &Path) -> anyhow::Result<Self> {
        let partitions = config.build_partitions();
        let license_pool = config.licenses.clone();

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
        };

        info!("cluster manager initialized (state will be recovered via Raft)");

        Ok(cm)
    }

    /// Submit a new job. If it has an array spec, expand into individual tasks.
    pub fn submit_job(&self, mut spec: JobSpec) -> anyhow::Result<JobId> {
        // If no partition specified, assign the default partition.
        if spec.partition.is_none() {
            let partitions = self.partitions.read();
            if let Some(default_part) = partitions.iter().find(|p| p.is_default) {
                spec.partition = Some(default_part.name.clone());
            } else if let Some(first) = partitions.first() {
                spec.partition = Some(first.name.clone());
            }
        }

        // Validate partition constraints before accepting the job
        self.validate_partition(&spec)?;

        // Check for array job
        if let Some(ref array_spec) = spec.array_spec {
            return self.submit_array_job(spec.clone(), array_spec);
        }

        let job_id = self.next_job_id.fetch_add(1, Ordering::SeqCst);

        self.propose(WalOperation::JobSubmit {
            job_id,
            spec: spec.clone(),
        });

        info!(job_id, name = %spec.name, user = %spec.user, "job submitted");
        Ok(job_id)
    }

    /// Submit an array job — expands the spec into individual task jobs.
    fn submit_array_job(&self, spec: JobSpec, array_spec_str: &str) -> anyhow::Result<JobId> {
        use spur_core::array::parse_array_spec;

        let array = parse_array_spec(array_spec_str)
            .map_err(|e| anyhow::anyhow!("invalid array spec: {}", e))?;

        let array_job_id = self.next_job_id.fetch_add(1, Ordering::SeqCst);
        let mut jobs = self.jobs.write();

        info!(
            array_job_id,
            tasks = array.task_ids.len(),
            max_concurrent = array.max_concurrent,
            name = %spec.name,
            "array job submitted"
        );

        for &task_id in &array.task_ids {
            let task_job_id = self.next_job_id.fetch_add(1, Ordering::SeqCst);
            let mut task_spec = spec.clone();
            task_spec.array_spec = None; // Don't recurse

            self.propose(WalOperation::JobSubmit {
                job_id: task_job_id,
                spec: task_spec.clone(),
            });

            let mut job = Job::new(task_job_id, task_spec);
            job.array_job_id = Some(array_job_id);
            job.array_task_id = Some(task_id);
            if array.max_concurrent > 0 {
                job.array_max_concurrent = Some(array.max_concurrent);
            }

            jobs.insert(task_job_id, job);
        }

        // Return the array job ID (first task IDs are array_job_id + 1, +2, ...)
        Ok(array_job_id)
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

    /// Get jobs matching filters.
    pub fn get_jobs(
        &self,
        states: &[JobState],
        user: Option<&str>,
        partition: Option<&str>,
        account: Option<&str>,
        job_ids: &[JobId],
    ) -> Vec<Job> {
        let jobs = self.jobs.read();
        jobs.values()
            .filter(|j| {
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
                if !job_ids.is_empty() && !job_ids.contains(&j.job_id) {
                    return false;
                }
                true
            })
            .cloned()
            .collect()
    }

    /// Cancel a job.
    pub fn cancel_job(&self, job_id: JobId, _user: &str) -> anyhow::Result<()> {
        let old_state = {
            let jobs = self.jobs.read();
            let job = jobs
                .get(&job_id)
                .ok_or_else(|| anyhow::anyhow!("job {} not found", job_id))?;
            if job.state.is_terminal() {
                anyhow::bail!("job {} is already {:?}", job_id, job.state);
            }
            job.state
        };

        self.propose(WalOperation::JobStateChange {
            job_id,
            old_state,
            new_state: JobState::Cancelled,
        });

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
        });
        self.propose(WalOperation::JobStart {
            job_id,
            nodes: node_names.clone(),
            resources: resources.clone(),
        });

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

        debug!(job_id, "job started");
        Ok(())
    }

    /// Complete a job.
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
        self.propose(WalOperation::JobComplete {
            job_id,
            exit_code,
            state,
        });

        // Notifications (side effects, not replicated)
        let spec_for_notify = self.jobs.read().get(&job_id).map(|j| j.spec.clone());
        if let Some(spec) = spec_for_notify {
            let is_success = state == JobState::Completed;
            let is_failure = matches!(
                state,
                JobState::Failed | JobState::Timeout | JobState::NodeFail
            );
            if is_success && spec.mail_type.iter().any(|t| t == "END" || t == "ALL") {
                self.send_notification(job_id, "END", &spec);
            }
            if is_failure && spec.mail_type.iter().any(|t| t == "FAIL" || t == "ALL") {
                self.send_notification(job_id, "FAIL", &spec);
            }
        }

        let should_requeue = matches!(
            state,
            JobState::Timeout | JobState::Preempted | JobState::NodeFail
        );
        if should_requeue {
            self.maybe_requeue(job_id);
        }

        debug!(job_id, exit_code, "job completed");
        Ok(())
    }

    /// Requeue a job if spec.requeue is set and attempt limit not exceeded.
    fn maybe_requeue(&self, job_id: JobId) {
        const MAX_REQUEUE: u32 = 3;
        let (should_requeue, old_state) = {
            let jobs = self.jobs.read();
            let Some(job) = jobs.get(&job_id) else {
                return;
            };
            if !job.spec.requeue || job.requeue_count >= MAX_REQUEUE {
                return;
            }
            (true, job.state)
        };
        if !should_requeue {
            return;
        }

        self.propose(WalOperation::JobStateChange {
            job_id,
            old_state,
            new_state: JobState::Pending,
        });

        info!(job_id, from = %old_state, "job requeued");
    }

    /// Requeue a job back to Pending after a dispatch failure.
    /// Unlike `maybe_requeue`, this is unconditional and doesn't require
    /// the requeue flag on the spec. Used when the agent rejects a job
    /// (e.g., container image not found) so it can be retried after the
    /// user fixes the issue. (Issue #91)
    pub fn requeue_job(&self, job_id: JobId) {
        let old_state = {
            let jobs = self.jobs.read();
            let Some(job) = jobs.get(&job_id) else {
                return;
            };
            job.state
        };

        self.propose(WalOperation::JobStateChange {
            job_id,
            old_state,
            new_state: JobState::Pending,
        });

        info!(job_id, from = %old_state, "job requeued after dispatch failure");
    }

    /// Register a node agent.
    pub fn register_node(
        &self,
        name: String,
        resources: ResourceSet,
        address: String,
        port: u16,
        wg_pubkey: String,
        version: String,
        source: NodeSource,
    ) {
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

        // Issue #70: If node is already registered, update its connection
        // info and resources but PRESERVE its current state and allocations.
        // The K8s node watcher re-registers nodes on every Apply event, which
        // was resetting Allocated/Mixed nodes back to Idle.
        {
            let mut nodes = self.nodes.write();
            if let Some(existing) = nodes.get_mut(&effective_name) {
                // Update connection info and resources, keep state + allocations
                existing.total_resources = resources.clone();
                existing.address = Some(address.clone());
                existing.port = port;
                existing.source = source;
                if !wg_pubkey.is_empty() {
                    existing.wg_pubkey = Some(wg_pubkey);
                }
                existing.version = Some(version);
                existing.last_heartbeat = Some(Utc::now());
                debug!(node = %effective_name, state = ?existing.state, "node re-registered (state preserved)");
                return;
            }
        }

        self.propose(WalOperation::NodeRegister {
            name: effective_name.clone(),
            resources,
            address,
        });

        // Set ephemeral connection info not tracked by the WAL
        if let Some(node) = self.nodes.write().get_mut(&effective_name) {
            node.port = port;
            node.source = source;
            if !wg_pubkey.is_empty() {
                node.wg_pubkey = Some(wg_pubkey);
            }
            node.version = Some(version);
            node.agent_start_time = Some(Utc::now());
            node.last_heartbeat = Some(Utc::now());
        }

        info!(node = %effective_name, "node registered");
    }

    /// Update node heartbeat data.
    pub fn update_heartbeat(&self, name: &str, cpu_load: u32, free_memory_mb: u64) {
        // Resolve hostname alias (agent may use real hostname, node stored under config name)
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
        });
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
        });
        if let Some(job) = self.jobs.write().get_mut(&job_id) {
            job.pending_reason = PendingReason::Priority;
        }
        info!(job_id, "job released");
        Ok(())
    }

    /// Update job properties.
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
            });
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
            let effective = if state == NodeState::Drain
                && (node.alloc_resources.cpus > 0 || !node.alloc_resources.gpus.is_empty())
            {
                NodeState::Draining
            } else {
                state
            };
            (old, effective)
        };

        self.propose(WalOperation::NodeStateChange {
            name: name.to_string(),
            old_state,
            new_state: effective_state,
            reason,
        });
        info!(node = %name, old = ?old_state, new = ?effective_state, "node state updated");
        Ok(())
    }

    /// Check for stale nodes (no heartbeat within timeout) and mark them DOWN.
    pub fn check_node_health(&self, timeout_secs: u64) {
        let now = Utc::now();
        let threshold = chrono::Duration::seconds(timeout_secs as i64);

        // Collect nodes that need to be marked DOWN (avoid holding write lock across propose)
        let stale_nodes: Vec<(String, NodeState)> = {
            let nodes = self.nodes.read();
            nodes
                .values()
                .filter(|n| {
                    n.state != NodeState::Down
                        && n.state != NodeState::Drain
                        && n.last_heartbeat
                            .map(|hb| now - hb > threshold)
                            .unwrap_or(false)
                })
                .map(|n| (n.name.clone(), n.state))
                .collect()
        };

        for (name, old_state) in stale_nodes {
            warn!(node = %name, "node marked DOWN (heartbeat timeout)");
            self.propose(WalOperation::NodeStateChange {
                name,
                old_state,
                new_state: NodeState::Down,
                reason: Some("Not responding".into()),
            });
        }
    }

    /// Create a job step.
    pub fn create_step(&self, job_id: JobId, step_id: u32, step: JobStep) {
        self.steps.write().insert((job_id, step_id), step);
        debug!(job_id, step_id, "step created");
    }

    /// Complete a job step.
    pub fn complete_step(&self, job_id: JobId, step_id: u32, exit_code: i32) {
        if let Some(step) = self.steps.write().get_mut(&(job_id, step_id)) {
            step.state = if exit_code == 0 {
                StepState::Completed
            } else {
                StepState::Failed
            };
            step.exit_code = Some(exit_code);
            step.end_time = Some(Utc::now());
            debug!(job_id, step_id, exit_code, "step completed");
        }
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
            match check_dependencies(job, &get_job, &get_jobs_by_name_user) {
                DependencyResult::Satisfied => true,
                DependencyResult::Waiting => false,
                DependencyResult::Failed => {
                    // Dependency can never be satisfied — cancel the job
                    // (can't mutate here since we hold a read lock; mark for later)
                    false
                }
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
                    if let Some(aid) = j.array_job_id {
                        *counts.entry(aid).or_insert(0) += 1;
                    }
                }
            }
            counts
        };
        pending.retain(|job| {
            if let (Some(aid), Some(max)) = (job.array_job_id, job.array_max_concurrent) {
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
            // fair_share = 1.0 (neutral) until spurdbd integration
            job.priority = spur_sched::priority::effective_priority(
                job.priority,
                1.0,
                age_minutes,
                partition_tier,
            );
        }

        pending.sort_by(|a, b| b.priority.cmp(&a.priority));
        pending
    }

    /// Find jobs by name and user (for singleton dependency).
    pub fn get_jobs_by_name_user(&self, name: &str, user: &str) -> Vec<Job> {
        self.jobs
            .read()
            .values()
            .filter(|j| j.spec.name == name && j.spec.user == user)
            .cloned()
            .collect()
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
        use spur_sched::backfill::BackfillScheduler;
        use spur_sched::traits::Scheduler;

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

    /// Persist a mutation via Raft consensus. The apply callback
    /// (`StateMachineApply`) handles in-memory state on all nodes.
    fn propose(&self, op: WalOperation) {
        let raft = self
            .raft
            .read()
            .clone()
            .expect("raft must be set before propose is called");
        if let Err(e) = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async { raft.client_write(op).await })
        }) {
            warn!(error = %e, "raft propose failed");
        }
    }

    /// Apply a WalOperation to in-memory state.
    /// Called by Raft's `apply_to_state_machine` on commit.
    fn apply_operation(&self, op: &WalOperation) {
        let mut jobs = self.jobs.write();
        let mut nodes = self.nodes.write();
        let mut next_id = self.next_job_id.load(Ordering::Relaxed);
        let timestamp = Utc::now();

        match op {
            WalOperation::JobSubmit { job_id, spec } => {
                let mut job = Job::new(*job_id, spec.clone());
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
                    let _ = job.transition(*new_state);
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
                        return;
                    }
                }
            }
            WalOperation::JobComplete {
                job_id,
                exit_code,
                state,
            } => {
                let freed_nodes;
                let allocated_resources;
                if let Some(job) = jobs.get_mut(job_id) {
                    let _ = job.transition(*state);
                    job.exit_code = Some(*exit_code);
                    job.end_time = Some(timestamp);
                    freed_nodes = job.allocated_nodes.clone();
                    allocated_resources = job.allocated_resources.clone();
                } else {
                    return;
                }
                // Deallocate node resources
                if let Some(ref total) = allocated_resources {
                    let node_count = freed_nodes.len().max(1) as u32;
                    let per_node = ResourceSet {
                        cpus: total.cpus / node_count,
                        memory_mb: total.memory_mb / node_count as u64,
                        gpus: total.gpus.clone(),
                        generic: total
                            .generic
                            .iter()
                            .map(|(k, v)| (k.clone(), v / node_count as u64))
                            .collect(),
                    };
                    for name in &freed_nodes {
                        if let Some(node) = nodes.get_mut(name) {
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
                }
                // Complete all steps
                drop(jobs);
                drop(nodes);
                let mut steps = self.steps.write();
                for step in steps.values_mut() {
                    if step.job_id == *job_id && !step.state.is_terminal() {
                        step.state = if *exit_code == 0 {
                            StepState::Completed
                        } else {
                            StepState::Failed
                        };
                        step.exit_code = Some(*exit_code);
                        step.end_time = Some(timestamp);
                    }
                }
                // Return licenses
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
            } => {
                let mut node = Node::new(name.clone(), resources.clone());
                node.address = Some(address.clone());
                node.state = NodeState::Idle;

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
                return;
            }
            WalOperation::NodeStateChange {
                name,
                new_state,
                reason,
                ..
            } => {
                if let Some(node) = nodes.get_mut(name) {
                    node.state = *new_state;
                    node.state_reason = reason.clone();
                }
            }
            WalOperation::NodeHeartbeat {
                name,
                cpu_load,
                free_memory_mb,
            } => {
                if let Some(node) = nodes.get_mut(name) {
                    node.cpu_load = *cpu_load;
                    node.free_memory_mb = *free_memory_mb;
                }
            }
        }
        self.next_job_id.store(next_id, Ordering::Relaxed);
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
    fn apply_operation(&self, op: &WalOperation) {
        self.apply_operation(op);
    }

    fn snapshot_state(&self) -> Vec<u8> {
        let snap = ClusterSnapshot {
            jobs: self.jobs.read().values().cloned().collect(),
            nodes: self.nodes.read().values().cloned().collect(),
            reservations: self.reservations.read().clone(),
            steps: self.steps.read().values().cloned().collect(),
            license_pool: self.license_pool.read().clone(),
            hostname_aliases: self.hostname_aliases.read().clone(),
        };
        serde_json::to_vec(&snap).unwrap_or_default()
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use spur_core::job::JobSpec;
    use spur_core::resource::ResourceSet;
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
            licenses: HashMap::new(),
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
            spur_core::node::NodeSource::BareMetal,
        );
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
            cm.get_job(job_id).map_or(false, |j| j.state == expected)
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_job_submit() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        let spec = basic_spec("test-job");
        cm.apply_operation(&WalOperation::JobSubmit {
            job_id: 1,
            spec: spec.clone(),
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
            spec: basic_spec("j"),
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
            spec: basic_spec("j"),
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
            spec: basic_spec("j"),
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
            spec: basic_spec("j"),
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
    async fn apply_node_heartbeat() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        register_node(&cm, "n1", 4, 8000);
        cm.apply_operation(&WalOperation::NodeHeartbeat {
            name: "n1".into(),
            cpu_load: 75,
            free_memory_mb: 4000,
        });

        let node = cm.get_node("n1").unwrap();
        assert_eq!(node.cpu_load, 75);
        assert_eq!(node.free_memory_mb, 4000);
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
    async fn snapshot_and_restore() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;

        register_node(&cm, "n1", 4, 8000);
        submit_and_wait(&cm, basic_spec("snap-job"));

        let data = cm.snapshot_state();
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
            cm.get_job(id).map_or(false, |j| j.priority == 0)
        });
        let job = cm.get_job(id).unwrap();
        assert_eq!(job.priority, 0);
        assert_eq!(job.pending_reason, PendingReason::Held);

        cm.release_job(id).unwrap();
        wait_for("release applied", || {
            cm.get_job(id).map_or(false, |j| j.priority > 0)
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
            cm.get_job(id).map_or(false, |j| j.priority == 5000)
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
                .map_or(false, |n| n.state == NodeState::Drain)
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
                .map_or(false, |n| n.state == NodeState::Down)
        });
        let node = cm.get_node("stale").unwrap();
        assert_eq!(node.state, NodeState::Down);
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
    async fn register_node_gets_partition_via_propose() {
        let dir = TempDir::new().unwrap();
        let cm = test_cluster(&dir).await;
        register_node(&cm, "test-node", 4, 8000);

        let node = cm.get_node("test-node").unwrap();
        assert!(!node.partitions.is_empty());
        assert_eq!(node.partitions[0], "default");
    }
}
