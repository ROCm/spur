// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;

use crate::burst_buffer::BbStageState;
use crate::resource::ResourceAllocations;

/// Unique job identifier assigned by the controller.
pub type JobId = u32;

/// Job states matching Slurm's state model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum JobState {
    Pending,
    Running,
    Completing,
    Completed,
    Failed,
    Cancelled,
    Timeout,
    NodeFail,
    Preempted,
    Suspended,
    Deadline,
    OutOfMemory,
}

/// Sentinel bit spurd OR's into a completion `signal` for an OOM kill, so the
/// controller maps it to `JobState::OutOfMemory`; low bits keep the real signal.
pub const OOM_SIGNAL_FLAG: i32 = 0x1000;

impl JobState {
    /// Short code used in squeue output (matches Slurm).
    pub fn code(&self) -> &'static str {
        match self {
            Self::Pending => "PD",
            Self::Running => "R",
            Self::Completing => "CG",
            Self::Completed => "CD",
            Self::Failed => "F",
            Self::Cancelled => "CA",
            Self::Timeout => "TO",
            Self::NodeFail => "NF",
            Self::Preempted => "PR",
            Self::Suspended => "S",
            Self::Deadline => "DL",
            Self::OutOfMemory => "OOM",
        }
    }

    /// Full display name (matches Slurm).
    pub fn display(&self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Running => "RUNNING",
            Self::Completing => "COMPLETING",
            Self::Completed => "COMPLETED",
            Self::Failed => "FAILED",
            Self::Cancelled => "CANCELLED",
            Self::Timeout => "TIMEOUT",
            Self::NodeFail => "NODE_FAIL",
            Self::Preempted => "PREEMPTED",
            Self::Suspended => "SUSPENDED",
            Self::Deadline => "DEADLINE",
            Self::OutOfMemory => "OUT_OF_MEMORY",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::Failed
                | Self::Cancelled
                | Self::Timeout
                | Self::NodeFail
                | Self::Deadline
                | Self::OutOfMemory
        )
    }

    pub fn is_active(&self) -> bool {
        matches!(self, Self::Running | Self::Completing | Self::Suspended)
    }

    /// Per-node completion state implied by one exit code.
    pub fn completion_state_for_exit_code(exit_code: i32) -> Self {
        if exit_code == 0 {
            Self::Completed
        } else {
            Self::Failed
        }
    }

    /// Validate `ReportJobStatusRequest.state` for per-node completion reports.
    ///
    /// Only `Completed` and `Failed` are valid; state must match `exit_code`.
    pub fn validate_completion_report_state(
        reported: Self,
        exit_code: i32,
    ) -> Result<(), CompletionReportStateError> {
        match reported {
            Self::Completed | Self::Failed => {
                let expected = Self::completion_state_for_exit_code(exit_code);
                if reported == expected {
                    Ok(())
                } else {
                    Err(CompletionReportStateError::InvalidStateForExitCode {
                        reported,
                        exit_code,
                        expected,
                    })
                }
            }
            _ => Err(CompletionReportStateError::InvalidCompletionState { reported }),
        }
    }

    /// Every core variant, in proto discriminant order for iteration only.
    pub const ALL: [JobState; 12] = [
        Self::Pending,
        Self::Running,
        Self::Completing,
        Self::Completed,
        Self::Failed,
        Self::Cancelled,
        Self::Timeout,
        Self::NodeFail,
        Self::Preempted,
        Self::Suspended,
        Self::Deadline,
        Self::OutOfMemory,
    ];

    pub const COUNT: usize = Self::ALL.len();

    /// Convert a prost `JobState` enum to core.
    pub fn from_proto(p: spur_proto::proto::JobState) -> Self {
        match p {
            spur_proto::proto::JobState::JobPending => Self::Pending,
            spur_proto::proto::JobState::JobRunning => Self::Running,
            spur_proto::proto::JobState::JobCompleting => Self::Completing,
            spur_proto::proto::JobState::JobCompleted => Self::Completed,
            spur_proto::proto::JobState::JobFailed => Self::Failed,
            spur_proto::proto::JobState::JobCancelled => Self::Cancelled,
            spur_proto::proto::JobState::JobTimeout => Self::Timeout,
            spur_proto::proto::JobState::JobNodeFail => Self::NodeFail,
            spur_proto::proto::JobState::JobPreempted => Self::Preempted,
            spur_proto::proto::JobState::JobSuspended => Self::Suspended,
            spur_proto::proto::JobState::JobDeadline => Self::Deadline,
            spur_proto::proto::JobState::JobOutOfMemory => Self::OutOfMemory,
        }
    }

    /// Convert core state to prost `JobState`.
    pub fn to_proto(self) -> spur_proto::proto::JobState {
        match self {
            Self::Pending => spur_proto::proto::JobState::JobPending,
            Self::Running => spur_proto::proto::JobState::JobRunning,
            Self::Completing => spur_proto::proto::JobState::JobCompleting,
            Self::Completed => spur_proto::proto::JobState::JobCompleted,
            Self::Failed => spur_proto::proto::JobState::JobFailed,
            Self::Cancelled => spur_proto::proto::JobState::JobCancelled,
            Self::Timeout => spur_proto::proto::JobState::JobTimeout,
            Self::NodeFail => spur_proto::proto::JobState::JobNodeFail,
            Self::Preempted => spur_proto::proto::JobState::JobPreempted,
            Self::Suspended => spur_proto::proto::JobState::JobSuspended,
            Self::Deadline => spur_proto::proto::JobState::JobDeadline,
            Self::OutOfMemory => spur_proto::proto::JobState::JobOutOfMemory,
        }
    }

    /// Convert a proto wire discriminant to core.
    pub fn from_proto_i32(v: i32) -> Option<Self> {
        spur_proto::proto::JobState::try_from(v)
            .ok()
            .map(Self::from_proto)
    }

    /// Core state as proto wire discriminant.
    pub fn to_proto_i32(self) -> i32 {
        self.to_proto() as i32
    }

    /// Parse from a Slurm state code ("PD", "R") or full name ("PENDING", "RUNNING").
    pub fn from_code_or_name(s: &str) -> Option<Self> {
        let upper = s.to_uppercase();
        Self::ALL
            .iter()
            .find(|st| st.code() == upper || st.display() == upper)
            .copied()
    }
}

impl std::fmt::Display for JobState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.display())
    }
}

/// Reason a job is pending.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PendingReason {
    #[default]
    None,
    Priority,
    Resources,
    PartitionDown,
    PartitionNodeLimit,
    PartitionTimeLimit,
    Dependency,
    NodeDown,
    Held,
    QoSMaxJobsPerUser,
    ReqNodeNotAvail,
    BeginTime,
    DeadLine,
    Licenses,
    NonZeroExitCode,
    RaisedSignal,
    JobLaunchFailure,
    JobHeldAdmin,
    BadConstraints,
    PartitionInactive,
    PartitionConfig,
    DependencyNeverSatisfied,
    InvalidAccount,
    InvalidQOS,
    BootFail,
    OutOfMemory,

    // Slurm 25.11 reason-code parity additions; each maps to a WAIT_*/FAIL_*
    // reason, with the byte-exact Slurm string in display(). Only reasons with
    // a live emission path are landed here; the rest are added alongside their
    // enforcement logic (see #307) to avoid dead variants accreting in the
    // Raft-snapshot deserialization contract.
    Reservation,
    QosMaxCpuPerJobLimit,
    QosMaxWallDurationPerJobLimit,
    QosMaxMemoryPerJob,
    QosMaxCpuPerUserLimit,
    QosMaxSubmitJobPerUserLimit,
    QosMaxNodePerJobLimit,
    QosGrpCpuLimit,
    QosGrpMemLimit,
    QosGrpNodeLimit,
    BurstBufferResources,
    BurstBufferStageIn,
}

impl PendingReason {
    pub fn display(&self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Priority => "Priority",
            Self::Resources => "Resources",
            Self::PartitionDown => "PartitionDown",
            Self::PartitionNodeLimit => "PartNodeLimit",
            Self::PartitionTimeLimit => "PartTimeLimit",
            Self::Dependency => "Dependency",
            Self::NodeDown => "NodeDown",
            Self::Held => "JobHeldUser",
            Self::QoSMaxJobsPerUser => "QOSMaxJobsPerUserLimit",
            Self::ReqNodeNotAvail => "ReqNodeNotAvail",
            Self::BeginTime => "BeginTime",
            Self::DeadLine => "DeadLine",
            Self::Licenses => "Licenses",
            Self::NonZeroExitCode => "NonZeroExitCode",
            Self::RaisedSignal => "RaisedSignal",
            Self::JobLaunchFailure => "JobLaunchFailure",
            Self::JobHeldAdmin => "JobHeldAdmin",
            Self::BadConstraints => "BadConstraints",
            Self::PartitionInactive => "PartitionInactive",
            Self::PartitionConfig => "PartitionConfig",
            Self::DependencyNeverSatisfied => "DependencyNeverSatisfied",
            Self::InvalidAccount => "InvalidAccount",
            Self::InvalidQOS => "InvalidQOS",
            Self::BootFail => "BootFailure",
            Self::OutOfMemory => "OutOfMemory",
            Self::Reservation => "Reservation",
            Self::QosMaxCpuPerJobLimit => "QOSMaxCpuPerJobLimit",
            Self::QosMaxWallDurationPerJobLimit => "QOSMaxWallDurationPerJobLimit",
            Self::QosMaxMemoryPerJob => "QOSMaxMemoryPerJob",
            Self::QosMaxCpuPerUserLimit => "QOSMaxCpuPerUserLimit",
            Self::QosMaxSubmitJobPerUserLimit => "QOSMaxSubmitJobPerUserLimit",
            Self::QosMaxNodePerJobLimit => "QOSMaxNodePerJobLimit",
            Self::QosGrpCpuLimit => "QOSGrpCpuLimit",
            Self::QosGrpMemLimit => "QOSGrpMemLimit",
            Self::QosGrpNodeLimit => "QOSGrpNodeLimit",
            Self::BurstBufferResources => "BurstBufferResources",
            Self::BurstBufferStageIn => "BurstBufferStageIn",
        }
    }
}

impl std::fmt::Display for PendingReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.display())
    }
}

/// Job specification submitted by the user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSpec {
    pub name: String,
    pub partition: Option<String>,
    pub account: Option<String>,
    pub user: String,
    pub uid: u32,
    pub gid: u32,

    // Resources
    pub num_nodes: u32,
    pub num_tasks: u32,
    pub tasks_per_node: Option<u32>,
    pub cpus_per_task: u32,
    pub memory_per_node_mb: Option<u64>,
    pub memory_per_cpu_mb: Option<u64>,
    pub gres: Vec<String>,

    // Execution
    pub script: Option<String>,
    pub argv: Vec<String>,
    pub work_dir: String,
    pub stdout_path: Option<String>,
    pub stderr_path: Option<String>,
    pub environment: HashMap<String, String>,

    // Time
    pub time_limit: Option<chrono::Duration>,
    pub time_min: Option<chrono::Duration>,

    // Scheduling
    pub qos: Option<String>,
    pub priority: Option<u32>,
    pub reservation: Option<String>,
    pub dependency: Vec<String>,
    pub nodelist: Option<String>,
    pub exclude: Option<String>,
    /// Node feature constraint (comma-separated, all must match).
    pub constraint: Option<String>,

    // MPI
    pub mpi: Option<String>,
    pub distribution: Option<String>,

    // Heterogeneous jobs
    pub het_group: Option<u32>,

    // Array
    pub array_spec: Option<String>,
    #[serde(default)]
    pub array_job_id: Option<JobId>,
    #[serde(default)]
    pub array_task_id: Option<u32>,
    #[serde(default)]
    pub array_max_concurrent: Option<u32>,

    // Flags
    pub requeue: bool,
    pub exclusive: bool,
    pub hold: bool,
    pub interactive: bool,
    pub mail_type: Vec<String>,
    pub mail_user: Option<String>,
    pub comment: Option<String>,
    pub wckey: Option<String>,

    // Container
    pub container_image: Option<String>,
    pub container_mounts: Vec<String>,
    pub container_workdir: Option<String>,
    pub container_name: Option<String>,
    pub container_readonly: bool,
    pub container_mount_home: bool,
    pub container_env: HashMap<String, String>,
    pub container_entrypoint: Option<String>,
    pub container_remap_root: bool,

    // Burst buffer
    pub burst_buffer: Option<String>,

    // Deferred scheduling
    /// Earliest time the job is eligible to start.
    pub begin_time: Option<DateTime<Utc>>,
    /// If still pending after this time, cancel the job.
    pub deadline: Option<DateTime<Utc>>,

    // Scheduling strategy
    /// Spread job across least-loaded nodes.
    pub spread_job: bool,
    /// Topology-aware scheduling: "tree" (minimize switch hops) or
    /// "block" (keep within one rack). None = default (no topology preference).
    pub topology: Option<String>,

    // Kubernetes pod options
    /// Enable host networking for the pod (for RDMA/NCCL).
    pub host_network: bool,
    /// Run container in privileged mode.
    pub privileged: bool,
    /// Enable host IPC namespace sharing (for NCCL shared memory).
    pub host_ipc: bool,
    /// Shared memory size (e.g., "64Gi"). Mounted as emptyDir at /dev/shm.
    pub shm_size: Option<String>,
    /// Extra device plugin resources (e.g., {"rdma/devices": "1"}).
    pub extra_resources: std::collections::HashMap<String, String>,

    // Output mode
    /// How to open stdout/stderr files: "truncate" (default) or "append".
    pub open_mode: Option<String>,
}

impl Default for JobSpec {
    fn default() -> Self {
        Self {
            name: String::new(),
            partition: None,
            account: None,
            user: String::new(),
            uid: 0,
            gid: 0,
            num_nodes: 1,
            num_tasks: 1,
            tasks_per_node: None,
            cpus_per_task: 1,
            memory_per_node_mb: None,
            memory_per_cpu_mb: None,
            gres: Vec::new(),
            script: None,
            argv: Vec::new(),
            work_dir: String::from("/tmp"),
            stdout_path: None,
            stderr_path: None,
            environment: HashMap::new(),
            time_limit: None,
            time_min: None,
            qos: None,
            priority: None,
            reservation: None,
            dependency: Vec::new(),
            nodelist: None,
            exclude: None,
            constraint: None,
            mpi: None,
            distribution: None,
            het_group: None,
            array_spec: None,
            array_job_id: None,
            array_task_id: None,
            array_max_concurrent: None,
            requeue: false,
            exclusive: false,
            hold: false,
            interactive: false,
            mail_type: Vec::new(),
            mail_user: None,
            comment: None,
            wckey: None,
            container_image: None,
            container_mounts: Vec::new(),
            container_workdir: None,
            container_name: None,
            container_readonly: false,
            container_mount_home: false,
            container_env: HashMap::new(),
            container_entrypoint: None,
            container_remap_root: false,
            burst_buffer: None,
            begin_time: None,
            deadline: None,
            spread_job: false,
            topology: None,
            host_network: false,
            privileged: false,
            host_ipc: false,
            shm_size: None,
            extra_resources: std::collections::HashMap::new(),
            open_mode: None,
        }
    }
}

/// One node's completion outcome for a job: the raw process wait status,
/// split into exit code and terminating signal (0 = none).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct NodeCompletion {
    pub code: i32,
    pub signal: i32,
}

/// Internal job record held by the controller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub job_id: JobId,
    pub spec: JobSpec,
    pub state: JobState,
    pub pending_reason: PendingReason,
    pub priority: u32,

    pub submit_time: DateTime<Utc>,
    pub start_time: Option<DateTime<Utc>>,
    pub end_time: Option<DateTime<Utc>>,

    pub allocated_nodes: Vec<String>,
    pub allocated_resources: Option<ResourceAllocations>,
    /// Per-node allocation slices (for deallocation on job complete).
    #[serde(default)]
    pub per_node_alloc: HashMap<String, ResourceAllocations>,

    pub exit_code: Option<i32>,

    /// Terminating signal of the primary node's process (0 = none).
    #[serde(default)]
    pub exit_signal: i32,
    /// Slurm `DerivedExitCode`: running max over srun step exit codes (via
    /// `JobStepComplete`); 0 when the job ran no srun steps.
    #[serde(default)]
    pub derived_exit_code: i32,

    /// Number of times this job has been requeued.
    #[serde(default)]
    pub requeue_count: u32,

    // Heterogeneous job support
    /// Links het job components to the first component's job ID.
    #[serde(default)]
    pub het_job_id: Option<JobId>,
    /// Component index within a heterogeneous job group (0 = first).
    #[serde(default)]
    pub het_group: Option<u32>,

    /// Per-node exit codes reported while the job is in Completing.
    #[serde(default)]
    pub node_completions: HashMap<String, NodeCompletion>,

    /// Wall-clock instant the job entered Suspended (None unless currently suspended).
    #[serde(default)]
    pub suspended_at: Option<DateTime<Utc>>,
    /// Total seconds spent suspended across all suspend/resume cycles.
    #[serde(default)]
    pub suspended_secs: i64,

    /// Burst-buffer staging phase. `None` until the scheduler reserves BB
    /// capacity for this job; then `Staging` while stage-in runs and `Ready`
    /// once it completes. A BB job is held off dispatch until `Ready`.
    #[serde(default)]
    pub bb_stage_state: BbStageState,
}

impl Job {
    pub fn new(job_id: JobId, spec: JobSpec) -> Self {
        let priority = if spec.hold {
            0
        } else {
            spec.priority.unwrap_or(1000)
        };
        let state = JobState::Pending;
        let pending_reason = if spec.hold {
            PendingReason::Held
        } else {
            // Start with None — the scheduler loop's update_pending_reasons()
            // will set the actual reason (Priority, Resources, etc.) on the
            // first cycle. This avoids showing a misleading "Priority" reason
            // before the scheduler has evaluated the job. (Issue #90)
            PendingReason::None
        };
        Self {
            job_id,
            spec,
            state,
            pending_reason,
            priority,
            submit_time: Utc::now(),
            start_time: None,
            end_time: None,
            allocated_nodes: Vec::new(),
            allocated_resources: None,
            per_node_alloc: HashMap::new(),
            exit_code: None,
            exit_signal: 0,
            derived_exit_code: 0,
            requeue_count: 0,
            het_job_id: None,
            het_group: None,
            node_completions: HashMap::new(),
            suspended_at: None,
            suspended_secs: 0,
            bb_stage_state: BbStageState::None,
        }
    }

    /// Derive a job's `ExitCode` and state from per-node completions, matching
    /// Slurm: `ExitCode` is the primary node's raw wait status (exit_code,
    /// signal); state is `Failed` if the primary exited non-zero or was
    /// signaled, else `Completed`.
    ///
    /// If `primary_node` is absent, falls back to the worst completion (a
    /// signaled node, or the highest exit code) so a failure isn't masked.
    ///
    /// Returns `(state, exit_code, exit_signal)`. Note this does NOT compute the
    /// job's DerivedExitCode — that is the running max over srun steps maintained
    /// via `JobStepComplete`, not a node-based value.
    pub fn derived_completion(
        node_completions: &HashMap<String, NodeCompletion>,
        primary_node: &str,
    ) -> (JobState, i32, i32) {
        let primary = node_completions.get(primary_node).copied().or_else(|| {
            // No primary completion (shouldn't happen once all nodes report).
            // Pick the worst failure so the outcome is never masked: rank a
            // signaled node above a plain non-zero exit, then by exit code.
            node_completions
                .values()
                .filter(|c| c.code != 0 || c.signal != 0)
                .max_by_key(|c| (c.signal != 0, c.code, c.signal))
                .copied()
        });

        match primary {
            Some(c) => {
                let failed = c.code != 0 || c.signal != 0;
                let state = if failed {
                    JobState::Failed
                } else {
                    JobState::Completed
                };
                (state, c.code, c.signal)
            }
            None => (JobState::Completed, 0, 0),
        }
    }

    pub fn all_nodes_completed(&self) -> bool {
        !self.allocated_nodes.is_empty()
            && self.node_completions.len() == self.allocated_nodes.len()
    }

    /// Compute the run time.
    pub fn run_time(&self) -> Option<chrono::Duration> {
        let start = self.start_time?;
        let end = self.end_time.unwrap_or_else(Utc::now);
        let mut suspended = self.suspended_secs;
        if let Some(since) = self.suspended_at {
            suspended += (end - since).num_seconds().max(0);
        }
        Some(((end - start) - chrono::Duration::seconds(suspended)).max(chrono::Duration::zero()))
    }

    /// Wall-clock deadline for time-limit enforcement, pushed out by time spent
    /// suspended so a job regains its full budget after resume (Slurm parity).
    pub fn effective_deadline(
        &self,
        start: DateTime<Utc>,
        time_limit: chrono::Duration,
    ) -> DateTime<Utc> {
        let mut suspended = self.suspended_secs;
        if let Some(since) = self.suspended_at {
            suspended += (Utc::now() - since).num_seconds().max(0);
        }
        start + time_limit + chrono::Duration::seconds(suspended)
    }

    /// Resolve stdout path, substituting %j/%N patterns.
    pub fn resolved_stdout(&self) -> String {
        self.resolve_path(self.spec.stdout_path.as_deref().unwrap_or("spur-%j.out"))
    }

    /// Resolve stderr path.
    pub fn resolved_stderr(&self) -> String {
        self.resolve_path(self.spec.stderr_path.as_deref().unwrap_or("spur-%j.out"))
    }

    fn resolve_path(&self, pattern: &str) -> String {
        let mut result = pattern.to_string();
        result = result.replace("%j", &self.job_id.to_string());
        result = result.replace("%J", &self.job_id.to_string());
        result = result.replace("%x", &self.spec.name);
        if let Some(tid) = self.spec.array_task_id {
            result = result.replace("%a", &tid.to_string());
            result = result.replace(
                "%A",
                &self.spec.array_job_id.unwrap_or(self.job_id).to_string(),
            );
        }
        if let Some(node) = self.allocated_nodes.first() {
            result = result.replace("%N", node);
        }
        result = result.replace("%u", &self.spec.user);
        result
    }
}

/// State transitions.
#[derive(Debug, Error)]
pub enum JobTransitionError {
    #[error("invalid transition from {from} to {to}")]
    Invalid { from: JobState, to: JobState },
}

/// Errors from validating completion reports from agents/operators.
#[derive(Debug, Error)]
pub enum CompletionReportStateError {
    #[error("invalid completion state {reported}; only COMPLETED or FAILED are accepted for completion reports")]
    InvalidCompletionState { reported: JobState },

    #[error(
        "completion state {reported} does not match exit_code {exit_code}; expected {expected}"
    )]
    InvalidStateForExitCode {
        reported: JobState,
        exit_code: i32,
        expected: JobState,
    },
}

/// Errors from recording a per-node job completion report.
#[derive(Debug, Error)]
pub enum NodeCompleteError {
    #[error("job {job_id} not found")]
    JobNotFound { job_id: JobId },

    #[error("node {node} is not allocated to job {job_id}")]
    NodeNotAllocated { job_id: JobId, node: String },

    #[error("raft propose failed: {source}")]
    RaftPropose {
        #[source]
        source: anyhow::Error,
    },
}

impl NodeCompleteError {
    pub fn retryable(&self) -> bool {
        match self {
            Self::JobNotFound { .. } | Self::NodeNotAllocated { .. } => false,
            Self::RaftPropose { .. } => true,
        }
    }
}

/// Result of applying a transition through the tolerant apply path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionOutcome {
    /// The state changed from its previous value to the requested one.
    Applied,
    /// The requested state equalled the current state; nothing changed.
    /// Expected on WAL replay / HA follower catch-up where an entry may be
    /// re-applied after it has already taken effect.
    NoOp,
}

impl Job {
    /// Attempt a state transition, enforcing the state machine.
    pub fn transition(&mut self, to: JobState) -> Result<(), JobTransitionError> {
        let valid = match (self.state, to) {
            (JobState::Pending, JobState::Running) => true,
            (JobState::Pending, JobState::Cancelled) => true,
            (JobState::Pending, JobState::Deadline) => true,
            (JobState::Running, JobState::Completing) => true,
            (JobState::Running, JobState::Completed) => true,
            (JobState::Running, JobState::Failed) => true,
            (JobState::Running, JobState::Cancelled) => true,
            (JobState::Running, JobState::Timeout) => true,
            (JobState::Running, JobState::NodeFail) => true,
            (JobState::Running, JobState::Preempted) => true,
            (JobState::Running, JobState::Suspended) => true,
            (JobState::Running, JobState::OutOfMemory) => true,
            (JobState::Completing, JobState::Completed) => true,
            (JobState::Completing, JobState::Failed) => true,
            (JobState::Completing, JobState::Cancelled) => true,
            (JobState::Completing, JobState::NodeFail) => true,
            (JobState::Completing, JobState::OutOfMemory) => true,
            (JobState::Suspended, JobState::Running) => true,
            (JobState::Suspended, JobState::Cancelled) => true,
            // Completion routes through Completing like a running job (Slurm JOB_COMPLETING).
            (JobState::Suspended, JobState::Completing) => true,
            // A suspended job whose process dies out-of-band (OOM, external
            // kill, node loss) must still finalize rather than strand in
            // SUSPENDED. Mirrors Slurm finalizing a suspended job that exits.
            (JobState::Suspended, JobState::Completed) => true,
            (JobState::Suspended, JobState::Failed) => true,
            (JobState::Suspended, JobState::Timeout) => true,
            (JobState::Suspended, JobState::NodeFail) => true,
            (JobState::Suspended, JobState::OutOfMemory) => true,
            // Requeue transitions: terminal → Pending (for --requeue jobs)
            (JobState::Timeout, JobState::Pending) => true,
            (JobState::Preempted, JobState::Pending) => true,
            (JobState::NodeFail, JobState::Pending) => true,
            (JobState::Failed, JobState::Pending) => true,
            _ => false,
        };

        if valid {
            self.state = to;
            if to.is_terminal() && self.end_time.is_none() {
                self.end_time = Some(Utc::now());
            }
            // Requeue: clear end_time when going back to Pending
            if to == JobState::Pending {
                self.end_time = None;
            }
            Ok(())
        } else {
            Err(JobTransitionError::Invalid {
                from: self.state,
                to,
            })
        }
    }

    /// Transition tolerant of already-applied entries, for the WAL apply path.
    ///
    /// A request to move to the state the job is already in is a `NoOp` rather
    /// than an error: replaying a committed log or catching up an HA follower
    /// legitimately re-applies entries, and those must not surface as invalid
    /// transitions. Genuine illegal moves (e.g. Completed -> Running) still
    /// error. Live command paths use the strict `transition()` instead.
    pub fn apply_transition(
        &mut self,
        to: JobState,
    ) -> Result<TransitionOutcome, JobTransitionError> {
        if self.state == to {
            return Ok(TransitionOutcome::NoOp);
        }
        self.transition(to).map(|()| TransitionOutcome::Applied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_job() -> Job {
        Job::new(
            1,
            JobSpec {
                name: "test".into(),
                user: "alice".into(),
                ..Default::default()
            },
        )
    }

    #[test]
    fn suspended_time_excluded_from_run_time() {
        let mut job = make_job();
        job.start_time = Some(Utc::now() - chrono::Duration::seconds(100));
        job.end_time = Some(Utc::now());
        job.suspended_secs = 30;
        let rt = job.run_time().unwrap().num_seconds();
        assert!((68..=72).contains(&rt), "expected ~70s, got {rt}");
    }

    #[test]
    fn in_progress_suspension_excluded_from_run_time() {
        let mut job = make_job();
        job.start_time = Some(Utc::now() - chrono::Duration::seconds(100));
        job.end_time = None;
        job.suspended_at = Some(Utc::now() - chrono::Duration::seconds(40));
        let rt = job.run_time().unwrap().num_seconds();
        assert!((58..=62).contains(&rt), "expected ~60s, got {rt}");
    }

    #[test]
    fn effective_deadline_extends_by_suspended_time() {
        let mut job = make_job();
        let start = Utc::now();
        job.suspended_secs = 50;
        let dl = job.effective_deadline(start, chrono::Duration::seconds(100));
        assert_eq!((dl - start).num_seconds(), 150);
    }

    #[test]
    fn test_state_transitions() {
        let mut job = make_job();
        assert_eq!(job.state, JobState::Pending);

        job.transition(JobState::Running).unwrap();
        assert_eq!(job.state, JobState::Running);
        assert!(job.start_time.is_none()); // start_time set externally

        job.transition(JobState::Completed).unwrap();
        assert_eq!(job.state, JobState::Completed);
        assert!(job.end_time.is_some());
    }

    #[test]
    fn node_completion_defaults_and_construct() {
        let c = NodeCompletion { code: 7, signal: 0 };
        assert_eq!(c.code, 7);
        assert_eq!(c.signal, 0);
        let d = NodeCompletion::default();
        assert_eq!(d.code, 0);
        assert_eq!(d.signal, 0);
    }

    #[test]
    fn job_has_exit_signal_field_default_none() {
        let job = Job::new(1, JobSpec::default());
        assert_eq!(job.exit_signal, 0);
        assert_eq!(job.derived_exit_code, 0);
        assert!(job.node_completions.is_empty());
    }

    #[test]
    fn derived_completion_primary_exit() {
        let mut nc = HashMap::new();
        nc.insert("n0".to_string(), NodeCompletion { code: 2, signal: 0 });
        nc.insert("n1".to_string(), NodeCompletion { code: 7, signal: 0 });
        let (state, code, signal) = Job::derived_completion(&nc, "n0");
        assert_eq!(state, JobState::Failed);
        assert_eq!(code, 2);
        assert_eq!(signal, 0);
    }

    #[test]
    fn derived_completion_primary_signaled() {
        let mut nc = HashMap::new();
        nc.insert("n0".to_string(), NodeCompletion { code: 0, signal: 9 });
        let (state, code, signal) = Job::derived_completion(&nc, "n0");
        assert_eq!(state, JobState::Failed);
        assert_eq!(code, 0);
        assert_eq!(signal, 9);
    }

    #[test]
    fn derived_completion_clean_success() {
        let mut nc = HashMap::new();
        nc.insert("n0".to_string(), NodeCompletion { code: 0, signal: 0 });
        let (state, code, signal) = Job::derived_completion(&nc, "n0");
        assert_eq!(state, JobState::Completed);
        assert_eq!(code, 0);
        assert_eq!(signal, 0);
    }

    #[test]
    fn derived_completion_missing_primary_falls_back() {
        let mut nc = HashMap::new();
        nc.insert("nX".to_string(), NodeCompletion { code: 4, signal: 0 });
        let (state, code, _signal) = Job::derived_completion(&nc, "n0");
        assert_eq!(state, JobState::Failed);
        assert_eq!(code, 4);
    }

    #[test]
    fn derived_completion_missing_primary_prefers_signaled() {
        // Missing primary, a signaled node and a higher plain-exit node: the
        // signaled node wins so the signal isn't masked by the higher code.
        let mut nc = HashMap::new();
        nc.insert("a".to_string(), NodeCompletion { code: 9, signal: 0 });
        nc.insert(
            "b".to_string(),
            NodeCompletion {
                code: 0,
                signal: 11,
            },
        );
        let (state, code, signal) = Job::derived_completion(&nc, "missing");
        assert_eq!(state, JobState::Failed);
        assert_eq!(code, 0);
        assert_eq!(signal, 11);
    }

    #[test]
    fn derived_completion_empty_map_is_clean() {
        let nc = HashMap::new();
        let (state, code, signal) = Job::derived_completion(&nc, "n0");
        assert_eq!(state, JobState::Completed);
        assert_eq!((code, signal), (0, 0));
    }

    #[test]
    fn derived_completion_primary_mixed_code_and_signal() {
        // A single node that both exited non-zero AND was signaled: both propagate.
        let mut nc = HashMap::new();
        nc.insert(
            "n0".to_string(),
            NodeCompletion {
                code: 5,
                signal: 11,
            },
        );
        let (state, code, signal) = Job::derived_completion(&nc, "n0");
        assert_eq!(state, JobState::Failed);
        assert_eq!(code, 5);
        assert_eq!(signal, 11);
    }

    #[test]
    fn completion_state_for_exit_code_maps_expected_states() {
        assert_eq!(
            JobState::completion_state_for_exit_code(0),
            JobState::Completed
        );
        assert_eq!(
            JobState::completion_state_for_exit_code(42),
            JobState::Failed
        );
        assert_eq!(
            JobState::completion_state_for_exit_code(-1),
            JobState::Failed
        );
    }

    #[test]
    fn validate_completion_report_state_accepts_aligned_pairs() {
        assert!(JobState::validate_completion_report_state(JobState::Completed, 0).is_ok());
        assert!(JobState::validate_completion_report_state(JobState::Failed, 7).is_ok());
    }

    #[test]
    fn validate_completion_report_state_rejects_mismatch() {
        let err = JobState::validate_completion_report_state(JobState::Completed, 9).unwrap_err();
        assert!(matches!(
            err,
            CompletionReportStateError::InvalidStateForExitCode {
                reported: JobState::Completed,
                exit_code: 9,
                expected: JobState::Failed
            }
        ));
    }

    #[test]
    fn validate_completion_report_state_rejects_other_terminal_states() {
        let err = JobState::validate_completion_report_state(JobState::Cancelled, 0).unwrap_err();
        assert!(matches!(
            err,
            CompletionReportStateError::InvalidCompletionState {
                reported: JobState::Cancelled
            }
        ));
    }

    #[test]
    fn validate_completion_report_state_rejects_completing() {
        let err = JobState::validate_completion_report_state(JobState::Completing, 0).unwrap_err();
        assert!(matches!(
            err,
            CompletionReportStateError::InvalidCompletionState {
                reported: JobState::Completing
            }
        ));
    }

    #[test]
    fn validate_completion_report_state_rejects_running() {
        let err = JobState::validate_completion_report_state(JobState::Running, 0).unwrap_err();
        assert!(matches!(
            err,
            CompletionReportStateError::InvalidCompletionState {
                reported: JobState::Running
            }
        ));
    }

    #[test]
    fn node_complete_error_retryable() {
        assert!(!NodeCompleteError::JobNotFound { job_id: 1 }.retryable());
        assert!(!NodeCompleteError::NodeNotAllocated {
            job_id: 1,
            node: "n1".into(),
        }
        .retryable());
        assert!(NodeCompleteError::RaftPropose {
            source: anyhow::anyhow!("test"),
        }
        .retryable());
    }

    #[test]
    fn completing_to_cancelled() {
        let mut job = make_job();
        job.transition(JobState::Running).unwrap();
        job.transition(JobState::Completing).unwrap();
        job.transition(JobState::Cancelled).unwrap();
        assert_eq!(job.state, JobState::Cancelled);
        assert!(job.end_time.is_some());
    }

    #[test]
    fn test_invalid_transition() {
        let mut job = make_job();
        assert!(job.transition(JobState::Completed).is_err());
    }

    #[test]
    fn apply_transition_idempotent_terminal_is_noop() {
        // WAL replay / HA follower catch-up re-applies committed entries. A
        // completed job re-completing must be a silent NoOp, not an error.
        let mut job = make_job();
        job.transition(JobState::Running).unwrap();
        job.transition(JobState::Completed).unwrap();

        let outcome = job
            .apply_transition(JobState::Completed)
            .expect("re-applying the current terminal state must not error");
        assert_eq!(outcome, TransitionOutcome::NoOp);
        assert_eq!(job.state, JobState::Completed);
    }

    #[test]
    fn apply_transition_reports_applied_for_real_move() {
        let mut job = make_job();
        let outcome = job
            .apply_transition(JobState::Running)
            .expect("Pending -> Running is legal");
        assert_eq!(outcome, TransitionOutcome::Applied);
        assert_eq!(job.state, JobState::Running);
    }

    #[test]
    fn apply_transition_still_rejects_illegal_move() {
        // Idempotency tolerance must not weaken the state machine: a genuinely
        // illegal move (Completed -> Running) still errors on the apply path.
        let mut job = make_job();
        job.transition(JobState::Running).unwrap();
        job.transition(JobState::Completed).unwrap();
        assert!(job.apply_transition(JobState::Running).is_err());
        assert_eq!(job.state, JobState::Completed);
    }

    #[test]
    fn apply_transition_noop_on_non_terminal_repeat() {
        let mut job = make_job();
        job.transition(JobState::Running).unwrap();
        let outcome = job.apply_transition(JobState::Running).unwrap();
        assert_eq!(outcome, TransitionOutcome::NoOp);
        assert_eq!(job.state, JobState::Running);
    }

    #[test]
    fn suspended_routes_through_completing() {
        // A suspended job's completion can route through Completing (Slurm JOB_COMPLETING).
        let mut job = make_job();
        job.transition(JobState::Running).unwrap();
        job.transition(JobState::Suspended).unwrap();
        job.transition(JobState::Completing).unwrap();
        job.transition(JobState::Completed).unwrap();
        assert_eq!(job.state, JobState::Completed);
    }

    #[test]
    fn deadline_state_is_terminal_and_reachable_only_from_pending() {
        // Terminal flag is what tells the dep engine and the requeue logic
        // that this state can never come back to Pending.
        assert!(JobState::Deadline.is_terminal());
        assert!(!JobState::Deadline.is_active());
        assert_eq!(JobState::Deadline.code(), "DL");
        assert_eq!(JobState::Deadline.display(), "DEADLINE");

        // Pending -> Deadline is the only legal entry. Running/Suspended/
        // already-terminal jobs must NOT silently fall into DEADLINE — those
        // would mask the real reason the job ended.
        let mut p = make_job();
        assert_eq!(p.state, JobState::Pending);
        p.transition(JobState::Deadline).unwrap();
        assert_eq!(p.state, JobState::Deadline);
        assert!(p.end_time.is_some());

        let mut r = make_job();
        r.transition(JobState::Running).unwrap();
        assert!(r.transition(JobState::Deadline).is_err());

        let mut done = make_job();
        done.transition(JobState::Running).unwrap();
        done.transition(JobState::Completed).unwrap();
        assert!(done.transition(JobState::Deadline).is_err());
    }

    #[test]
    fn out_of_memory_state_terminal_and_reachable_from_active() {
        assert!(JobState::OutOfMemory.is_terminal());
        assert!(!JobState::OutOfMemory.is_active());
        assert_eq!(JobState::OutOfMemory.code(), "OOM");
        assert_eq!(JobState::OutOfMemory.display(), "OUT_OF_MEMORY");

        // Running -> OutOfMemory (direct), Completing -> OutOfMemory, and
        // Suspended -> OutOfMemory are all legal; Pending is not.
        let mut r = make_job();
        r.transition(JobState::Running).unwrap();
        r.transition(JobState::OutOfMemory).unwrap();
        assert_eq!(r.state, JobState::OutOfMemory);
        assert!(r.end_time.is_some());

        let mut c = make_job();
        c.transition(JobState::Running).unwrap();
        c.transition(JobState::Completing).unwrap();
        c.transition(JobState::OutOfMemory).unwrap();
        assert_eq!(c.state, JobState::OutOfMemory);

        let mut p = make_job();
        assert!(p.transition(JobState::OutOfMemory).is_err());
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn oom_signal_flag_is_outside_real_signal_range() {
        // The sentinel must not collide with any real terminating signal (1..=64)
        // and must be cleanly strippable to recover the underlying SIGKILL.
        assert!(OOM_SIGNAL_FLAG > 64);
        let encoded = OOM_SIGNAL_FLAG | 9;
        assert_ne!(encoded & OOM_SIGNAL_FLAG, 0);
        assert_eq!(encoded & !OOM_SIGNAL_FLAG, 9);
    }

    #[test]
    fn deadline_reason_displays_slurm_string() {
        // Slurm reports this exact string ("DeadLine", note the cap D and L).
        // squeue scrapers and Slurm-compat clients pattern-match on it.
        assert_eq!(PendingReason::DeadLine.display(), "DeadLine");
    }

    /// (variant, exact Slurm 25.11.6 string) for every parity addition.
    const REASON_VOCAB: &[(PendingReason, &str)] = &[
        (PendingReason::PartitionConfig, "PartitionConfig"),
        (PendingReason::PartitionInactive, "PartitionInactive"),
        (PendingReason::Reservation, "Reservation"),
        (PendingReason::QosMaxCpuPerJobLimit, "QOSMaxCpuPerJobLimit"),
        (
            PendingReason::QosMaxWallDurationPerJobLimit,
            "QOSMaxWallDurationPerJobLimit",
        ),
        (PendingReason::QosMaxMemoryPerJob, "QOSMaxMemoryPerJob"),
        (
            PendingReason::QosMaxCpuPerUserLimit,
            "QOSMaxCpuPerUserLimit",
        ),
        (
            PendingReason::QosMaxSubmitJobPerUserLimit,
            "QOSMaxSubmitJobPerUserLimit",
        ),
        (
            PendingReason::QosMaxNodePerJobLimit,
            "QOSMaxNodePerJobLimit",
        ),
        (PendingReason::QosGrpCpuLimit, "QOSGrpCpuLimit"),
        (PendingReason::QosGrpMemLimit, "QOSGrpMemLimit"),
        (PendingReason::QosGrpNodeLimit, "QOSGrpNodeLimit"),
        (PendingReason::BurstBufferResources, "BurstBufferResources"),
        (PendingReason::BurstBufferStageIn, "BurstBufferStageIn"),
    ];

    #[test]
    fn reason_vocab_display_matches_slurm_25_11() {
        for (reason, expected) in REASON_VOCAB {
            assert_eq!(reason.display(), *expected, "Display for {reason:?}");
            assert_eq!(format!("{reason}"), *expected, "fmt for {reason:?}");
        }
    }

    #[test]
    fn reason_vocab_serde_roundtrips() {
        for (reason, _) in REASON_VOCAB {
            let json = serde_json::to_string(reason).expect("serialize reason");
            let back: PendingReason = serde_json::from_str(&json).expect("deserialize reason");
            assert_eq!(&back, reason, "serde roundtrip for {reason:?}");
        }
    }

    #[test]
    fn pending_reason_exit_vocabulary_display() {
        assert_eq!(PendingReason::NonZeroExitCode.display(), "NonZeroExitCode");
        assert_eq!(PendingReason::RaisedSignal.display(), "RaisedSignal");
        assert_eq!(
            PendingReason::JobLaunchFailure.display(),
            "JobLaunchFailure"
        );
        assert_eq!(PendingReason::OutOfMemory.display(), "OutOfMemory");
        assert_eq!(PendingReason::BootFail.display(), "BootFailure");
    }

    #[test]
    fn test_path_resolution() {
        let mut job = make_job();
        job.job_id = 42;
        job.spec.name = "train".into();
        job.spec.user = "bob".into();

        assert_eq!(job.resolve_path("spur-%j.out"), "spur-42.out");
        assert_eq!(job.resolve_path("output-%x-%u.log"), "output-train-bob.log");
    }

    #[test]
    fn all_is_complete_and_ordered() {
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        assert_eq!(JobState::ALL.len(), JobState::COUNT);
        for state in &JobState::ALL {
            assert!(seen.insert(state), "duplicate variant in ALL: {state}");
        }
    }

    #[test]
    fn job_state_proto_discriminants_match_core() {
        use spur_proto::proto::JobState as P;

        const TABLE: &[(P, JobState)] = &[
            (P::JobPending, JobState::Pending),
            (P::JobRunning, JobState::Running),
            (P::JobCompleting, JobState::Completing),
            (P::JobCompleted, JobState::Completed),
            (P::JobFailed, JobState::Failed),
            (P::JobCancelled, JobState::Cancelled),
            (P::JobTimeout, JobState::Timeout),
            (P::JobNodeFail, JobState::NodeFail),
            (P::JobPreempted, JobState::Preempted),
            (P::JobSuspended, JobState::Suspended),
            (P::JobDeadline, JobState::Deadline),
            (P::JobOutOfMemory, JobState::OutOfMemory),
        ];

        assert_eq!(TABLE.len(), JobState::COUNT);
        for &(proto, core) in TABLE {
            let wire = proto as i32;
            assert_eq!(P::try_from(wire).ok(), Some(proto));
            assert_eq!(JobState::from_proto_i32(wire), Some(core));
            assert_eq!(
                JobState::ALL.iter().position(|&s| s == core),
                Some(wire as usize),
                "ALL position for {core:?}"
            );
        }
    }

    #[test]
    fn job_state_proto_try_from_unknown_wire_values() {
        use spur_proto::proto::JobState as P;

        for bad in [-1, JobState::COUNT as i32, 99, i32::MAX] {
            assert_eq!(JobState::from_proto_i32(bad), None);
            assert!(P::try_from(bad).is_err());
        }
    }

    #[test]
    fn job_state_core_proto_roundtrip() {
        for &core in &JobState::ALL {
            assert_eq!(JobState::from_proto_i32(core.to_proto_i32()), Some(core));
            assert_eq!(JobState::from_proto(core.to_proto()), core);
        }
    }

    #[test]
    fn job_state_from_code_or_name_roundtrip() {
        for &state in &JobState::ALL {
            assert_eq!(JobState::from_code_or_name(state.code()), Some(state));
            assert_eq!(JobState::from_code_or_name(state.display()), Some(state));
        }
    }
}
