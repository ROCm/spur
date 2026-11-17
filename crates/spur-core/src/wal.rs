// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

use crate::admission::AdmissionToken;
use crate::job::{JobId, JobSpec, JobState, PendingReason};
use crate::k0s::{K0sPhase, K0sRole};
use crate::node::{NodeSource, NodeState};
use crate::partition::{Partition, PartitionClears};
use crate::partition_limits::{MemoryAdjustment, PartitionLimitsUpdate};
use crate::reservation::Reservation;
use std::collections::HashMap;

use crate::resource::{ResourceAllocations, ResourceSet};

fn default_port() -> u16 {
    6818
}

/// Deserializer for `Option<Option<T>>` patch fields: absent is no change (via
/// `#[serde(default)]`), `null` clears, a value sets. Pair with
/// `skip_serializing_if = "Option::is_none"` so a clear is not read back as absent.
pub(crate) fn clear_or_set<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// All state-mutating operations that get logged to the Raft log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WalOperation {
    // Job operations
    JobSubmit {
        job_id: JobId,
        spec: Box<JobSpec>,
    },
    JobStateChange {
        job_id: JobId,
        old_state: JobState,
        new_state: JobState,
        /// When set with `new_state == Pending`, applied atomically instead of clearing to `None`.
        #[serde(default)]
        pending_reason: Option<PendingReason>,
        /// When set with `new_state == Pending`, sets priority in the same apply step (e.g. hold at 0).
        #[serde(default)]
        pending_priority: Option<u32>,
        /// When set with `new_state == Pending`, holds the job until this instant.
        /// Computed on the leader so every replica applies the same instant.
        #[serde(default)]
        begin_time: Option<chrono::DateTime<chrono::Utc>>,
        /// Slurm's `state_desc`, travelling with `pending_reason` so both fields
        /// land together on every replica.
        #[serde(default)]
        pending_reason_desc: Option<String>,
        /// When set, apply is a no-op unless the job still holds this run_attempt.
        /// A delayed transition for a superseded reservation must not fire.
        #[serde(default)]
        expected_run_attempt: Option<u32>,
        /// Stamped on the leader for a Pending -> Running activation, so
        /// `start_time` dates the confirmed run rather than its own reservation.
        #[serde(default)]
        at: Option<chrono::DateTime<chrono::Utc>>,
    },
    JobStart {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preemption_observation: Option<u64>,
        job_id: JobId,
        nodes: Vec<String>,
        resources: ResourceAllocations,
        /// Per-node allocation slices (device IDs are node-local).
        #[serde(default)]
        per_node_alloc: HashMap<String, ResourceAllocations>,
        /// Standalone srun: native step dispatch (false = K8s batch fallback).
        #[serde(default)]
        srun_step_dispatch: bool,
        /// Run epoch for this dispatch (0 for pre-upgrade entries).
        #[serde(default)]
        run_attempt: u32,
        /// This run started on spare capacity outside the QOS group node quota
        /// (idle-fill). Travels in the WAL so the stamp lands on followers; the
        /// `Job` field is what survives snapshot and failover.
        #[serde(default)]
        idle_fill: bool,
        /// Partition the run is charged to for partition caps. `None` in
        /// pre-upgrade entries.
        #[serde(default)]
        effective_partition: Option<String>,
        /// The request as that partition's memory policy adjusted it, when it
        /// changed anything.
        #[serde(default)]
        memory_adjustment: Option<MemoryAdjustment>,
        /// Stamped on the leader so a replay dates the run from the dispatch
        /// rather than from its own clock. `None` in pre-upgrade entries.
        #[serde(default)]
        at: Option<chrono::DateTime<chrono::Utc>>,
    },
    JobComplete {
        job_id: JobId,
        exit_code: i32,
        state: JobState,
        /// Allocated nodes that never reported, set only when the controller
        /// force-finishes a completing run.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        unreported_nodes: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cancel: Option<crate::end_reason::CancelProvenance>,
        /// The controller ended the run itself (a launch failure, the requeue
        /// cap), not the workload or its client. A preemption grace must not
        /// turn such a completion into the preemption outcome.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        controller_initiated: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<chrono::DateTime<chrono::Utc>>,
    },
    JobNodeComplete {
        job_id: JobId,
        node_name: String,
        exit_code: i32,
        signal: i32,
        /// The run this report speaks for, re-checked on apply so a report that
        /// lost to a requeue cannot discharge the next run's debt. 0 is legacy.
        #[serde(default)]
        run_attempt: u32,
        #[serde(default)]
        at: Option<chrono::DateTime<chrono::Utc>>,
    },
    /// The time-limit watchdog signalled a running job for exhausting its wall
    /// clock. Durable so the grace period survives a leadership change and so
    /// every replica finalizes the run as `Timeout` rather than reading the
    /// terminating signal as an ordinary failure. `at` is stamped on the leader
    /// so replicas share one instant instead of consulting their own clocks.
    JobTimeLimitSignaled {
        job_id: JobId,
        at: chrono::DateTime<chrono::Utc>,
    },
    /// A live run was signalled for cancellation. Records the verdict and moves
    /// the run to Completing, holding its allocation until the nodes report.
    JobCancelSignaled {
        job_id: JobId,
        at: chrono::DateTime<chrono::Utc>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cancel: Option<crate::end_reason::CancelProvenance>,
    },
    /// An srun job step finished. Records the step's exit code durably so the
    /// job's DerivedExitCode (running max over steps) survives restart/replay.
    JobStepComplete {
        job_id: JobId,
        step_id: u32,
        exit_code: i32,
    },
    /// Record a job step at creation so `run_step` survives controller restart.
    JobStepCreate {
        step: Box<crate::step::JobStep>,
    },
    JobPriorityChange {
        job_id: JobId,
        old_priority: u32,
        new_priority: u32,
        /// When set, applied on all replicas so pending reason survives replay.
        #[serde(default)]
        pending_reason: Option<PendingReason>,
        /// Overrides the reason's default display text, same as
        /// `JobStateChange`'s `pending_reason_desc`. `#[serde(default)]` so
        /// older WAL/snapshot entries without this field replay as `None`.
        #[serde(default)]
        pending_reason_desc: Option<String>,
        /// When true, clears automatic requeue counter (admin release after max requeue).
        #[serde(default)]
        reset_requeue_count: bool,
        /// When true, clears `spec.reservation` (admin release after reservation delete hold).
        #[serde(default)]
        clear_reservation: bool,
        /// Dispatch epoch being held/backed-off, so a held or backed-off job's
        /// `run_attempt` still advances past the poisoned attempt. `None`
        /// where no new epoch was presented this cycle.
        #[serde(default)]
        run_attempt: Option<u32>,
    },
    /// Replace a pending job's excluded nodes (`None` clears). Applied only while the
    /// job is Pending with no placement charged, so replay reaches the same answer.
    JobExcludeUpdate {
        job_id: JobId,
        exclude: Option<String>,
    },
    /// Back off a job that failed dispatch before ever leaving Pending, where
    /// `JobStateChange`'s transition-gated backoff can't apply. NoOp on replay
    /// if the job has since left Pending.
    JobDispatchBackoff {
        job_id: JobId,
        begin_time: chrono::DateTime<chrono::Utc>,
        /// Hold without charging `max_batch_requeue`, for a refusal the job did
        /// not cause. Defaults to `false`: every pre-upgrade entry charged.
        #[serde(default)]
        spare_requeue_budget: bool,
        /// Dispatch epoch (reservation attempt) this backoff is aborting. A
        /// mismatch against the job's current epoch means a newer reservation
        /// already superseded it, so apply is a NoOp; otherwise this becomes
        /// the job's new epoch so a retry never re-presents the poisoned one.
        /// 0 for pre-upgrade entries (disables both the gate and the advance).
        #[serde(default)]
        run_attempt: u32,
        /// Park at `JobHoldMaxRequeue` instead of an ordinary backoff, sharing
        /// this op's own Pending/`run_attempt` gate rather than a separate propose.
        #[serde(default)]
        at_max_requeue: bool,
    },
    /// Preempt a running job and requeue it in one atomic step: free its node
    /// allocation, end the prior run for accounting (as PREEMPTED), return it to
    /// Pending, and hold it ineligible until `begin_time` so the scheduler can't
    /// re-dispatch it into its own in-flight kill. A single committed entry
    /// leaves the job Pending-with-hold and nodes freed, so a leadership change
    /// or restart mid-sequence cannot strand it in PREEMPTED. `begin_time` is
    /// the leader-computed absolute instant (already max'd against any user
    /// `--begin`); followers apply it verbatim and re-apply is a NoOp.
    JobPreemptRequeue {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preemption_claim: Option<crate::preemption_usage::PreemptionClaim>,
        job_id: JobId,
        begin_time: chrono::DateTime<chrono::Utc>,
        /// Job that triggered this preemption. Absent in WAL entries written by
        /// older controllers; defaults to `None` on replay.
        #[serde(default)]
        preempted_by: Option<JobId>,
        /// QOS of the preempting job (`None` for plain priority-based preemption).
        #[serde(default)]
        preempt_qos: Option<String>,
        /// When the preempted run ended, distinct from `begin_time`'s future hold.
        #[serde(default)]
        at: Option<chrono::DateTime<chrono::Utc>>,
        /// Run the caller decided to preempt, not whichever is current when this
        /// is proposed. A NoOp on apply when set and the job is at another run.
        /// `None` (every pre-upgrade entry) applies to whichever run is current.
        #[serde(default)]
        run_attempt: Option<u32>,
        /// Hand the freed nodes to `preempted_by`; false for reclaim and pre-handoff entries.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        handoff: bool,
    },
    /// Preempt a running job with cancel. The job transitions Running →
    /// Preempted → Cancelled: Preempted is reported to accounting so the
    /// PREEMPTED counter increments; Cancelled is the terminal live state so
    /// dependents and arrays resolve. Single atomic entry so a leadership
    /// change cannot strand the job running with its allocations held.
    JobPreemptCancel {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preemption_claim: Option<crate::preemption_usage::PreemptionClaim>,
        job_id: JobId,
        /// Job that triggered this preemption. Absent in WAL entries written by
        /// older controllers; defaults to `None` on replay.
        #[serde(default)]
        preempted_by: Option<JobId>,
        /// QOS of the preempting job (`None` for plain priority-based preemption).
        #[serde(default)]
        preempt_qos: Option<String>,
        /// Leader-stamped instant, so every replica measures the completing
        /// deadline from one value. Absent pre-upgrade; falls back to apply time.
        #[serde(default)]
        at: Option<chrono::DateTime<chrono::Utc>>,
        /// Run the caller decided to preempt, not whichever is current when this
        /// is proposed. A NoOp on apply when set and the job is at another run.
        /// `None` (every pre-upgrade entry) applies to whichever run is current.
        #[serde(default)]
        run_attempt: Option<u32>,
    },
    /// Admin requeue (`scontrol requeue` / `requeuehold`): return a job to
    /// Pending with the same spec in one atomic step. A running/suspended job is
    /// finalized once (steps, allocations, accounting-end as REQUEUED) before
    /// re-pending; an already-terminal job re-pends with no re-finalization.
    /// `hold` parks it held; `begin_time` defers eligibility (leader-computed).
    /// NoOp on replay once Pending.
    JobUserRequeue {
        job_id: JobId,
        #[serde(default)]
        hold: bool,
        #[serde(default)]
        begin_time: Option<chrono::DateTime<chrono::Utc>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<crate::end_reason::Actor>,
        /// When the superseded run ended, distinct from `begin_time`'s future hold.
        #[serde(default)]
        at: Option<chrono::DateTime<chrono::Utc>>,
    },
    JobSuspend {
        job_id: JobId,
        /// Controller-stamped instant of suspension (for replay-deterministic accounting).
        at: chrono::DateTime<chrono::Utc>,
        /// Preemption provenance — set when suspension is triggered by preemption,
        /// absent (None) for admin-initiated suspends. Defaults to None on replay
        /// so old WAL entries remain backward compatible.
        #[serde(default)]
        preempted_by: Option<JobId>,
        #[serde(default)]
        preempt_qos: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<crate::end_reason::Actor>,
    },
    JobResume {
        job_id: JobId,
        /// Controller-stamped instant of resume.
        at: chrono::DateTime<chrono::Utc>,
    },
    /// Evict a single job to NodeFail: same effect as a node health-check
    /// failure (frees allocations, feeds the auto-requeue path), but scoped
    /// to one job instead of every job on a node. Used when a subset of a
    /// job's assigned nodes never received the launch dispatch.
    JobEvict {
        job_id: JobId,
        /// Human-readable bootstrap failure (shown via scontrol / logs).
        #[serde(default)]
        detail: Option<String>,
        #[serde(default)]
        at: Option<chrono::DateTime<chrono::Utc>>,
    },
    /// Record why a requeued job is back in Pending (survives controller restart).
    JobLaunchFailureDetail {
        job_id: JobId,
        detail: String,
    },

    // Node operations
    NodeRegister {
        name: String,
        #[serde(default)]
        hostname: String,
        resources: ResourceSet,
        address: String,
        #[serde(default = "default_port")]
        port: u16,
        #[serde(default)]
        wg_pubkey: String,
        #[serde(default)]
        version: String,
        #[serde(default)]
        labels: HashMap<String, String>,
        #[serde(default)]
        source: NodeSource,
        #[serde(default)]
        capabilities: Vec<String>,
    },
    NodeUpdate {
        name: String,
        #[serde(default)]
        hostname: String,
        resources: ResourceSet,
        address: String,
        port: u16,
        wg_pubkey: String,
        version: String,
        #[serde(default)]
        source: NodeSource,
        #[serde(default)]
        capabilities: Vec<String>,
    },
    NodeStateChange {
        name: String,
        old_state: NodeState,
        new_state: NodeState,
        reason: Option<String>,
        #[serde(default)]
        admin_locked: bool,
        #[serde(default)]
        reason_uid: Option<u32>,
        #[serde(default)]
        reason_time: Option<chrono::DateTime<chrono::Utc>>,
        /// When the change was made. Dates the eviction of any job the node was
        /// running, which `reason_time` does not (it is only set for some changes).
        #[serde(default)]
        at: Option<chrono::DateTime<chrono::Utc>>,
    },
    NodeLabelsUpdate {
        name: String,
        set: HashMap<String, String>,
        remove: Vec<String>,
    },

    // Node deregistration
    NodeRemove {
        name: String,
        reason: Option<String>,
        #[serde(default)]
        at: Option<chrono::DateTime<chrono::Utc>>,
    },

    // Admission token operations
    TokenCreate {
        token: AdmissionToken,
    },
    TokenRevoke {
        token_id: String,
    },

    PartitionCreate {
        partition: Partition,
    },
    PartitionUpdate {
        name: String,
        /// Fields present in the update; absent fields are left unchanged.
        nodes: Option<String>,
        selector: Option<HashMap<String, String>>,
        state: Option<String>,
        // These three limits and `preempt_exempt_time` below are always serialized,
        // so old entries hold `null` for "unchanged"; removing one goes through `clear`.
        #[serde(default)]
        max_time_minutes: Option<u32>,
        #[serde(default)]
        default_time_minutes: Option<u32>,
        #[serde(default)]
        max_nodes: Option<u32>,
        min_nodes: Option<u32>,
        allow_accounts: Option<Vec<String>>,
        allow_groups: Option<Vec<String>>,
        deny_accounts: Option<Vec<String>>,
        deny_qos: Option<Vec<String>>,
        allow_qos: Option<Vec<String>>,
        priority_tier: Option<u32>,
        preempt_mode: Option<String>,
        is_default: Option<bool>,
        #[serde(default)]
        preempt_exempt_time: Option<u32>,
        #[serde(
            default,
            deserialize_with = "clear_or_set",
            skip_serializing_if = "Option::is_none"
        )]
        grace_time: Option<Option<u32>>,
        /// Limits to remove; wins over a value for the same limit.
        #[serde(default, skip_serializing_if = "PartitionClears::is_empty")]
        clear: PartitionClears,
        /// Merged into the partition's limits when the entry applies.
        #[serde(default)]
        resource_limits: Option<PartitionLimitsUpdate>,
    },
    PartitionDelete {
        name: String,
    },

    ReservationCreate {
        reservation: Reservation,
    },
    ReservationUpdate {
        name: String,
        duration_minutes: u32,
        add_nodes: Vec<String>,
        remove_nodes: Vec<String>,
        add_users: Vec<String>,
        remove_users: Vec<String>,
        add_accounts: Vec<String>,
        remove_accounts: Vec<String>,
    },
    ReservationDelete {
        name: String,
    },

    // Native k0s cluster operations. Appended at the end to keep externally-tagged
    // WAL replay backward-compatible.
    NodeK0sAssign {
        name: String,
        role: K0sRole,
        mesh_ip: String,
        pod_cidr: String,
    },
    K0sSetPhase {
        phase: K0sPhase,
        #[serde(default)]
        control_plane_node: Option<String>,
        #[serde(default)]
        control_plane_nodes: Vec<String>,
        #[serde(default)]
        member_nodes: Vec<String>,
        #[serde(default)]
        reset_requested: bool,
    },
    NodeK0sClear {
        name: String,
    },

    /// Evict the named terminal jobs to bound controller memory, or, with an empty
    /// `job_ids`, confirm a preemption launch: older controllers apply that as a no-op.
    EvictTerminalJobs {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preemption_release: Option<crate::preemption_usage::PreemptionRelease>,
        /// Apply-side cutoff: jobs that ended or changed preemption counts after it stay.
        #[serde(default)]
        before: Option<chrono::DateTime<chrono::Utc>>,
        #[serde(default)]
        at: Option<chrono::DateTime<chrono::Utc>>,
        job_ids: Vec<JobId>,
    },
    NodeK0sSetError {
        name: String,
        error: Option<String>,
    },
    /// Grow a scoped k0s cluster's member set online (`spur k8s add-nodes`). Applied as a union into
    /// `K0sClusterState.member_nodes`, so replaying it is idempotent. Never emitted for a
    /// whole-inventory cluster (empty `member_nodes`), where every node is already a member.
    K0sMemberNodesAdd {
        nodes: Vec<String>,
    },
    /// Shrink a scoped k0s cluster's member set online (`spur k8s remove-nodes`). Applied as a
    /// set-subtraction from `K0sClusterState.member_nodes`, so replaying it is idempotent. Never
    /// emitted for a whole-inventory cluster (empty `member_nodes`).
    K0sMemberNodesRemove {
        nodes: Vec<String>,
    },
    /// A run exhausted its time limit: records the TIMEOUT verdict and moves it to
    /// Completing in one step, holding its nodes until they report.
    JobTimeLimitExpired {
        job_id: JobId,
        run_attempt: u32,
        at: chrono::DateTime<chrono::Utc>,
    },
    /// A running job was warned of preemption and is in its grace period until
    /// `pending.deadline`. Applied only to a Running job at `pending.run_attempt`
    /// that is not already pending, so replaying it is idempotent.
    JobPreemptSignaled {
        job_id: JobId,
        pending: crate::job::PreemptPending,
    },
}

impl WalOperation {
    pub fn job_complete(job_id: JobId, exit_code: i32, state: JobState) -> Self {
        Self::JobComplete {
            job_id,
            exit_code,
            state,
            unreported_nodes: Vec::new(),
            cancel: None,
            controller_initiated: false,
            at: None,
        }
    }

    /// A run the controller ends as `Failed` on its own account, before it
    /// requeues or holds the job.
    pub fn job_failed_by_controller(job_id: JobId) -> Self {
        Self::JobComplete {
            job_id,
            exit_code: -1,
            state: JobState::Failed,
            unreported_nodes: Vec::new(),
            cancel: None,
            controller_initiated: true,
            at: None,
        }
    }

    pub fn job_cancelled(job_id: JobId, cancel: crate::end_reason::CancelProvenance) -> Self {
        Self::JobComplete {
            job_id,
            exit_code: -1,
            state: JobState::Cancelled,
            unreported_nodes: Vec::new(),
            cancel: Some(cancel),
            controller_initiated: false,
            at: None,
        }
    }

    pub fn job_state_change(job_id: JobId, old_state: JobState, new_state: JobState) -> Self {
        Self::JobStateChange {
            job_id,
            old_state,
            new_state,
            pending_reason: None,
            pending_priority: None,
            begin_time: None,
            pending_reason_desc: None,
            expected_run_attempt: None,
            at: None,
        }
    }

    /// Pending -> Running, refused on apply if the job no longer holds
    /// `run_attempt` (a delayed activation for a superseded reservation).
    pub fn job_state_change_activating(job_id: JobId, run_attempt: u32) -> Self {
        Self::JobStateChange {
            job_id,
            old_state: JobState::Pending,
            new_state: JobState::Running,
            pending_reason: None,
            pending_priority: None,
            begin_time: None,
            pending_reason_desc: None,
            expected_run_attempt: Some(run_attempt),
            at: None,
        }
    }

    /// Pending transition that applies a scheduling hold atomically (priority 0 + reason).
    pub fn job_state_change_held_pending(
        job_id: JobId,
        old_state: JobState,
        reason: PendingReason,
    ) -> Self {
        Self::JobStateChange {
            job_id,
            old_state,
            new_state: JobState::Pending,
            pending_reason: Some(reason),
            pending_priority: Some(0),
            begin_time: None,
            pending_reason_desc: None,
            expected_run_attempt: None,
            at: None,
        }
    }

    /// As [`Self::job_state_change_held_pending`], with a `state_desc` that says
    /// more about the hold than its reason code can.
    pub fn job_state_change_held_pending_desc(
        job_id: JobId,
        old_state: JobState,
        reason: PendingReason,
        desc: impl Into<String>,
    ) -> Self {
        Self::JobStateChange {
            job_id,
            old_state,
            new_state: JobState::Pending,
            pending_reason: Some(reason),
            pending_priority: Some(0),
            begin_time: None,
            pending_reason_desc: Some(desc.into()),
            expected_run_attempt: None,
            at: None,
        }
    }

    /// Requeue to Pending with a backoff hold and a reason, applied in one step
    /// so the job is never briefly eligible with no hold.
    pub fn job_state_change_backoff_pending(
        job_id: JobId,
        old_state: JobState,
        reason: PendingReason,
        begin_time: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        Self::JobStateChange {
            job_id,
            old_state,
            new_state: JobState::Pending,
            pending_reason: Some(reason),
            pending_priority: None,
            begin_time: Some(begin_time),
            pending_reason_desc: None,
            expected_run_attempt: None,
            at: None,
        }
    }

    /// Record node allocation at job start (batch/sbatch and K8s srun fallback).
    pub fn job_start(
        job_id: JobId,
        nodes: Vec<String>,
        resources: ResourceAllocations,
        per_node_alloc: HashMap<String, ResourceAllocations>,
    ) -> Self {
        Self::JobStart {
            preemption_observation: None,
            job_id,
            nodes,
            resources,
            per_node_alloc,
            srun_step_dispatch: false,
            run_attempt: 0,
            idle_fill: false,
            effective_partition: None,
            memory_adjustment: None,
            at: None,
        }
    }

    /// The leader's stamp for operations dating something a user reads back.
    /// Replays must date from here, not their own clock. `None` pre-upgrade.
    pub fn occurred_at(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        match self {
            Self::JobSubmit { spec, .. } => spec.submitted_at,
            Self::EvictTerminalJobs { at, .. }
            | Self::JobStart { at, .. }
            | Self::JobStateChange { at, .. }
            | Self::JobComplete { at, .. }
            | Self::JobNodeComplete { at, .. }
            | Self::JobPreemptRequeue { at, .. }
            | Self::JobPreemptCancel { at, .. }
            | Self::JobUserRequeue { at, .. }
            | Self::JobEvict { at, .. }
            | Self::NodeStateChange { at, .. }
            | Self::NodeRemove { at, .. } => *at,
            Self::JobSuspend { at, .. }
            | Self::JobResume { at, .. }
            | Self::JobTimeLimitSignaled { at, .. }
            | Self::JobTimeLimitExpired { at, .. }
            | Self::JobCancelSignaled { at, .. } => Some(*at),
            Self::JobPreemptSignaled { pending, .. } => Some(pending.signaled_at),
            // Exhaustive on purpose: a new variant that dates something a user
            // reads back must be a compile error here, not a silent `None`.
            Self::JobStepComplete { .. }
            | Self::JobStepCreate { .. }
            | Self::JobPriorityChange { .. }
            | Self::JobExcludeUpdate { .. }
            | Self::JobDispatchBackoff { .. }
            | Self::JobLaunchFailureDetail { .. }
            | Self::NodeRegister { .. }
            | Self::NodeUpdate { .. }
            | Self::NodeLabelsUpdate { .. }
            | Self::TokenCreate { .. }
            | Self::TokenRevoke { .. }
            | Self::PartitionCreate { .. }
            | Self::PartitionUpdate { .. }
            | Self::PartitionDelete { .. }
            | Self::ReservationCreate { .. }
            | Self::ReservationUpdate { .. }
            | Self::ReservationDelete { .. }
            | Self::NodeK0sAssign { .. }
            | Self::K0sSetPhase { .. }
            | Self::NodeK0sClear { .. }
            | Self::NodeK0sSetError { .. }
            | Self::K0sMemberNodesAdd { .. }
            | Self::K0sMemberNodesRemove { .. } => None,
        }
    }

    /// Stamp on the proposing leader so replicas and replays read one instant.
    /// An entry that already carries one keeps it.
    pub fn stamp_occurred_at(&mut self, now: chrono::DateTime<chrono::Utc>) {
        let slot = match self {
            Self::JobSubmit { spec, .. } => &mut spec.submitted_at,
            Self::EvictTerminalJobs { at, .. }
            | Self::JobStart { at, .. }
            | Self::JobStateChange { at, .. }
            | Self::JobComplete { at, .. }
            | Self::JobNodeComplete { at, .. }
            | Self::JobPreemptRequeue { at, .. }
            | Self::JobPreemptCancel { at, .. }
            | Self::JobUserRequeue { at, .. }
            | Self::JobEvict { at, .. }
            | Self::NodeStateChange { at, .. }
            | Self::NodeRemove { at, .. } => at,
            // Exhaustive for the same reason as `occurred_at`: these carry no
            // stamp, and a new one that does must not silently go unstamped.
            Self::JobSuspend { .. }
            | Self::JobResume { .. }
            | Self::JobTimeLimitSignaled { .. }
            | Self::JobTimeLimitExpired { .. }
            | Self::JobPreemptSignaled { .. }
            | Self::JobCancelSignaled { .. }
            | Self::JobStepComplete { .. }
            | Self::JobStepCreate { .. }
            | Self::JobPriorityChange { .. }
            | Self::JobExcludeUpdate { .. }
            | Self::JobDispatchBackoff { .. }
            | Self::JobLaunchFailureDetail { .. }
            | Self::NodeRegister { .. }
            | Self::NodeUpdate { .. }
            | Self::NodeLabelsUpdate { .. }
            | Self::TokenCreate { .. }
            | Self::TokenRevoke { .. }
            | Self::PartitionCreate { .. }
            | Self::PartitionUpdate { .. }
            | Self::PartitionDelete { .. }
            | Self::ReservationCreate { .. }
            | Self::ReservationUpdate { .. }
            | Self::ReservationDelete { .. }
            | Self::NodeK0sAssign { .. }
            | Self::K0sSetPhase { .. }
            | Self::NodeK0sClear { .. }
            | Self::NodeK0sSetError { .. }
            | Self::K0sMemberNodesAdd { .. }
            | Self::K0sMemberNodesRemove { .. } => return,
        };
        slot.get_or_insert(now);
    }
}

#[cfg(test)]
mod job_state_change_wal_tests {
    use super::*;

    /// A reason's variant name is its wire form in the Raft log, and a controller
    /// on an older binary cannot read a name it does not know. Freeze the newest
    /// one so a rename has to be a deliberate, versioned decision.
    #[test]
    fn grp_wall_reason_has_a_frozen_wire_name() {
        let json = serde_json::to_string(&PendingReason::QosGrpWallLimit).unwrap();
        assert_eq!(json, "\"QosGrpWallLimit\"");
    }

    #[test]
    fn job_exclude_update_has_a_frozen_wire_form() {
        let op = WalOperation::JobExcludeUpdate {
            job_id: 7,
            exclude: Some("n[1-2]".into()),
        };
        let json = serde_json::to_string(&op).unwrap();
        assert_eq!(
            json,
            r#"{"JobExcludeUpdate":{"job_id":7,"exclude":"n[1-2]"}}"#
        );
        let back: WalOperation = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            back,
            WalOperation::JobExcludeUpdate { job_id: 7, exclude: Some(ref e) } if e == "n[1-2]"
        ));
    }

    #[test]
    fn job_state_change_held_pending_round_trips() {
        let op = WalOperation::job_state_change_held_pending(
            1,
            JobState::Preempted,
            PendingReason::JobHoldMaxRequeue,
        );
        let json = serde_json::to_string(&op).unwrap();
        let back: WalOperation = serde_json::from_str(&json).unwrap();
        match back {
            WalOperation::JobStateChange {
                job_id,
                old_state,
                new_state,
                pending_reason,
                pending_priority,
                begin_time,
                pending_reason_desc,
                ..
            } => {
                assert_eq!(job_id, 1);
                assert_eq!(old_state, JobState::Preempted);
                assert_eq!(new_state, JobState::Pending);
                assert_eq!(pending_reason, Some(PendingReason::JobHoldMaxRequeue));
                assert_eq!(pending_priority, Some(0));
                assert_eq!(begin_time, None, "a max-requeue hold has no begin_time");
                assert_eq!(pending_reason_desc, None);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn a_held_pending_hold_carries_its_description_to_every_replica() {
        // The reason and its description must travel in one entry: a follower
        // that applied only the reason would report a bare JobHeldUser and lose
        // why the job is parked.
        let op = WalOperation::job_state_change_held_pending_desc(
            9,
            JobState::Failed,
            PendingReason::Held,
            "launch failed requeued held",
        );
        let json = serde_json::to_string(&op).unwrap();
        let back: WalOperation = serde_json::from_str(&json).unwrap();
        match back {
            WalOperation::JobStateChange {
                new_state,
                pending_reason,
                pending_priority,
                pending_reason_desc,
                ..
            } => {
                assert_eq!(new_state, JobState::Pending);
                assert_eq!(pending_reason, Some(PendingReason::Held));
                assert_eq!(pending_priority, Some(0));
                assert_eq!(
                    pending_reason_desc.as_deref(),
                    Some("launch failed requeued held")
                );
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn a_state_change_written_before_the_description_field_still_replays() {
        // Pre-upgrade WAL entries have no pending_reason_desc key at all.
        let op =
            WalOperation::job_state_change_held_pending(3, JobState::Running, PendingReason::Held);
        let mut value = serde_json::to_value(&op).unwrap();
        value["JobStateChange"]
            .as_object_mut()
            .unwrap()
            .remove("pending_reason_desc")
            .expect("the field must be written, so removing it models an old entry");

        let back: WalOperation =
            serde_json::from_value(value).expect("old entries must still replay");
        match back {
            WalOperation::JobStateChange {
                pending_reason,
                pending_reason_desc,
                ..
            } => {
                assert_eq!(pending_reason, Some(PendingReason::Held));
                assert_eq!(pending_reason_desc, None);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn job_state_change_without_hold_fields_deserializes() {
        let op = WalOperation::job_state_change(1, JobState::Pending, JobState::Running);
        let json = serde_json::to_string(&op).unwrap();
        let back: WalOperation = serde_json::from_str(&json).unwrap();
        match back {
            WalOperation::JobStateChange {
                pending_reason,
                pending_priority,
                begin_time,
                ..
            } => {
                assert_eq!(pending_reason, None);
                assert_eq!(pending_priority, None);
                assert_eq!(begin_time, None);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn job_state_change_backoff_pending_round_trips() {
        let hold = chrono::Utc::now() + chrono::Duration::seconds(40);
        let op = WalOperation::job_state_change_backoff_pending(
            7,
            JobState::Failed,
            PendingReason::JobLaunchFailure,
            hold,
        );
        let json = serde_json::to_string(&op).unwrap();
        let back: WalOperation = serde_json::from_str(&json).unwrap();
        match back {
            WalOperation::JobStateChange {
                new_state,
                pending_reason,
                pending_priority,
                begin_time,
                ..
            } => {
                assert_eq!(new_state, JobState::Pending);
                assert_eq!(pending_reason, Some(PendingReason::JobLaunchFailure));
                assert_eq!(
                    pending_priority, None,
                    "a backoff defers the job, it must not zero its priority"
                );
                assert_eq!(
                    begin_time,
                    Some(hold),
                    "the leader-computed instant must survive the WAL verbatim"
                );
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn job_dispatch_backoff_round_trips() {
        let hold = chrono::Utc::now() + chrono::Duration::seconds(20);
        let op = WalOperation::JobDispatchBackoff {
            job_id: 8,
            begin_time: hold,
            spare_requeue_budget: true,
            run_attempt: 3,
            at_max_requeue: false,
        };
        let json = serde_json::to_string(&op).unwrap();
        let back: WalOperation = serde_json::from_str(&json).unwrap();
        match back {
            WalOperation::JobDispatchBackoff {
                job_id,
                begin_time,
                spare_requeue_budget,
                run_attempt,
                ..
            } => {
                assert_eq!(job_id, 8);
                assert_eq!(begin_time, hold);
                assert!(
                    spare_requeue_budget,
                    "the exemption decides a job's fate, so it must survive the WAL verbatim"
                );
                assert_eq!(
                    run_attempt, 3,
                    "the aborted attempt's identity must survive the WAL verbatim"
                );
            }
            _ => panic!("wrong variant"),
        }
    }

    // A pre-upgrade entry charged the job's requeue budget. Replaying it as
    // spared would silently extend a retry budget the old leader had spent.
    #[test]
    fn pre_upgrade_job_dispatch_backoff_replays_as_charging_the_requeue_budget() {
        let json = r#"{"JobDispatchBackoff":{"job_id":8,"begin_time":"2026-01-02T03:04:05Z"}}"#;
        let back: WalOperation = serde_json::from_str(json).unwrap();
        match back {
            WalOperation::JobDispatchBackoff {
                job_id,
                spare_requeue_budget,
                ..
            } => {
                assert_eq!(job_id, 8);
                assert!(
                    !spare_requeue_budget,
                    "an entry with no field must keep the meaning it was written with"
                );
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn pre_upgrade_job_state_change_without_begin_time_deserializes() {
        // A WAL written before the backoff field existed must still replay.
        let json = r#"{"JobStateChange":{"job_id":3,"old_state":"FAILED","new_state":"PENDING"}}"#;
        let back: WalOperation = serde_json::from_str(json).unwrap();
        match back {
            WalOperation::JobStateChange {
                job_id, begin_time, ..
            } => {
                assert_eq!(job_id, 3);
                assert_eq!(begin_time, None);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn pre_upgrade_job_dispatch_backoff_without_run_attempt_deserializes() {
        // A WAL written before run_attempt existed on this variant must still
        // replay, defaulting to 0 rather than crashing the controller.
        let json = r#"{"JobDispatchBackoff":{"job_id":8,"begin_time":"2026-01-01T00:00:00Z"}}"#;
        let back: WalOperation = serde_json::from_str(json).unwrap();
        match back {
            WalOperation::JobDispatchBackoff {
                job_id,
                run_attempt,
                ..
            } => {
                assert_eq!(job_id, 8);
                assert_eq!(run_attempt, 0);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn pre_upgrade_job_dispatch_backoff_without_at_max_requeue_deserializes() {
        let json = r#"{"JobDispatchBackoff":{"job_id":8,"begin_time":"2026-01-01T00:00:00Z"}}"#;
        let back: WalOperation = serde_json::from_str(json).unwrap();
        match back {
            WalOperation::JobDispatchBackoff {
                job_id,
                at_max_requeue,
                ..
            } => {
                assert_eq!(job_id, 8);
                assert!(
                    !at_max_requeue,
                    "an entry with no field must replay as an ordinary backoff"
                );
            }
            _ => panic!("wrong variant"),
        }
    }
}

#[cfg(test)]
mod job_priority_change_wal_tests {
    use super::*;

    #[test]
    fn job_priority_change_carries_a_description_alongside_its_reason() {
        // Mirrors JobStateChange's pending_reason_desc: a hold applied while a
        // job is still Pending (e.g. hold_job_for_launch_failure) has nowhere
        // else to carry a custom description, since there is no state
        // transition for it to ride along with.
        let op = WalOperation::JobPriorityChange {
            job_id: 4,
            old_priority: 500,
            new_priority: 0,
            pending_reason: Some(PendingReason::Held),
            pending_reason_desc: Some("launch failed requeued held".into()),
            reset_requeue_count: false,
            clear_reservation: false,
            run_attempt: Some(2),
        };
        let json = serde_json::to_string(&op).unwrap();
        let back: WalOperation = serde_json::from_str(&json).unwrap();
        match back {
            WalOperation::JobPriorityChange {
                job_id,
                new_priority,
                pending_reason,
                pending_reason_desc,
                run_attempt,
                ..
            } => {
                assert_eq!(job_id, 4);
                assert_eq!(new_priority, 0);
                assert_eq!(pending_reason, Some(PendingReason::Held));
                assert_eq!(
                    pending_reason_desc.as_deref(),
                    Some("launch failed requeued held")
                );
                assert_eq!(run_attempt, Some(2));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn pre_upgrade_job_priority_change_without_description_or_run_attempt_deserializes() {
        // A WAL written before pending_reason_desc/run_attempt existed on
        // this variant must still replay (e.g. hold_job entries from before
        // either field).
        let json = r#"{"JobPriorityChange":{"job_id":4,"old_priority":500,"new_priority":0,"pending_reason":"Held"}}"#;
        let back: WalOperation = serde_json::from_str(json).unwrap();
        match back {
            WalOperation::JobPriorityChange {
                job_id,
                pending_reason,
                pending_reason_desc,
                reset_requeue_count,
                clear_reservation,
                run_attempt,
                ..
            } => {
                assert_eq!(job_id, 4);
                assert_eq!(pending_reason, Some(PendingReason::Held));
                assert_eq!(pending_reason_desc, None);
                assert!(!reset_requeue_count);
                assert!(!clear_reservation);
                assert_eq!(
                    run_attempt, None,
                    "an entry without the field must stay inert, not advance run_attempt"
                );
            }
            _ => panic!("wrong variant"),
        }
    }
}

#[cfg(test)]
mod reservation_wal_tests {
    use super::*;
    use crate::reservation::{Reservation, ReservationFlags};
    use chrono::Utc;

    #[test]
    fn reservation_create_round_trips() {
        let now = Utc::now();
        let op = WalOperation::ReservationCreate {
            reservation: Reservation {
                name: "r1".into(),
                start_time: now,
                end_time: now + chrono::Duration::hours(1),
                nodes: vec!["n1".into()],
                accounts: Vec::new(),
                users: vec!["alice".into()],
                flags: ReservationFlags {
                    maint: true,
                    ..Default::default()
                },
                owner: String::new(),
            },
        };
        let json = serde_json::to_string(&op).unwrap();
        let back: WalOperation = serde_json::from_str(&json).unwrap();
        match back {
            WalOperation::ReservationCreate { reservation } => {
                assert_eq!(reservation.name, "r1");
                assert!(reservation.flags.maint);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn reservation_delete_round_trips() {
        let op = WalOperation::ReservationDelete { name: "r1".into() };
        let json = serde_json::to_string(&op).unwrap();
        let back: WalOperation = serde_json::from_str(&json).unwrap();
        match back {
            WalOperation::ReservationDelete { name } => assert_eq!(name, "r1"),
            _ => panic!("wrong variant"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Frozen pre-`pty` v0.5.1 Raft entry; must still deserialize or spurctld
    // crashes on upgrade replay. Never regenerate — a failure here means a new
    // field needs `#[serde(default)]`, not a fixture edit.
    #[test]
    fn job_submit_v0_5_1_payload_still_deserializes() {
        const JOB_SUBMIT_V0_5_1: &str = r#"{"JobSubmit":{"job_id":7,"spec":{"name":"fixture","partition":null,"account":null,"user":"alice","uid":1000,"gid":1000,"num_nodes":1,"num_tasks":1,"tasks_per_node":null,"cpus_per_task":1,"memory_per_node_mb":null,"memory_per_cpu_mb":null,"gres":[],"script":null,"argv":[],"script_args":[],"work_dir":"/home/alice","stdout_path":null,"stderr_path":null,"stdin_path":null,"environment":{},"time_limit":null,"time_min":null,"qos":null,"priority":null,"reservation":null,"dependency":[],"nodelist":null,"exclude":null,"constraint":null,"mpi":null,"distribution":null,"het_group":null,"array_spec":null,"array_job_id":null,"array_task_id":null,"array_max_concurrent":null,"requeue":false,"exclusive":false,"hold":false,"interactive":false,"mail_type":[],"mail_user":null,"comment":null,"wckey":null,"container_image":null,"container_mounts":[],"container_workdir":null,"container_name":null,"container_readonly":false,"container_mount_home":false,"container_env":{},"container_entrypoint":null,"container_remap_root":false,"burst_buffer":null,"begin_time":null,"deadline":null,"spread_job":false,"topology":null,"host_network":false,"privileged":false,"host_ipc":false,"shm_size":null,"extra_resources":{},"open_mode":null}}}"#;

        let op: WalOperation = serde_json::from_str(JOB_SUBMIT_V0_5_1).expect(
            "v0.5.1 JobSubmit must deserialize; a new JobSpec field needs #[serde(default)]",
        );
        match op {
            WalOperation::JobSubmit { job_id, spec } => {
                assert_eq!(job_id, 7);
                assert_eq!(spec.name, "fixture");
                assert_eq!(spec.work_dir, "/home/alice");
                assert!(!spec.pty);
                assert!(!spec.srun_job);
                assert!(spec.gpus.is_none());
                assert_eq!(spec.memory_per_gpu_mb, None);
            }
            _ => panic!("wrong variant"),
        }
    }

    // Frozen pre-idle-fill JobStart entry (no idle_fill field); must still
    // deserialize or spurctld crashes on upgrade replay, with idle_fill
    // defaulting to false. Never regenerate — a failure means a new field needs
    // #[serde(default)].
    #[test]
    fn job_start_pre_idle_fill_payload_still_deserializes() {
        const JOB_START_PRE_IDLE_FILL: &str = r#"{"JobStart":{"job_id":5,"nodes":["n1"],"resources":{"cpus":4,"memory_mb":1024,"devices":{}},"per_node_alloc":{"n1":{"cpus":4,"memory_mb":1024,"devices":{}}},"srun_step_dispatch":false,"run_attempt":1}}"#;
        let op: WalOperation = serde_json::from_str(JOB_START_PRE_IDLE_FILL)
            .expect("pre-idle-fill JobStart must deserialize; idle_fill needs #[serde(default)]");
        match op {
            WalOperation::JobStart {
                job_id,
                srun_step_dispatch,
                run_attempt,
                idle_fill,
                ..
            } => {
                assert_eq!(job_id, 5);
                assert!(!srun_step_dispatch);
                assert_eq!(run_attempt, 1);
                assert!(!idle_fill, "an entry without the field defaults to false");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn job_start_idle_fill_stamp_round_trips() {
        let op = WalOperation::JobStart {
            preemption_observation: None,
            job_id: 8,
            nodes: vec!["n1".into()],
            resources: ResourceAllocations::default(),
            per_node_alloc: HashMap::new(),
            srun_step_dispatch: false,
            run_attempt: 2,
            idle_fill: true,
            effective_partition: None,
            memory_adjustment: None,
            at: None,
        };
        let json = serde_json::to_string(&op).unwrap();
        let back: WalOperation = serde_json::from_str(&json).unwrap();
        match back {
            WalOperation::JobStart {
                preemption_observation: _,
                job_id,
                idle_fill,
                ..
            } => {
                assert_eq!(job_id, 8);
                assert!(idle_fill, "the stamp must survive the WAL round-trip");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn job_start_effective_partition_and_memory_adjustment_round_trip() {
        let adjustment = MemoryAdjustment {
            cpus_per_task: 4,
            memory_per_node_mb: None,
            memory_per_cpu_mb: Some(1536),
            memory_per_gpu_mb: None,
        };
        let op = WalOperation::JobStart {
            job_id: 9,
            nodes: vec!["n1".into()],
            resources: ResourceAllocations::default(),
            per_node_alloc: HashMap::new(),
            srun_step_dispatch: false,
            run_attempt: 1,
            idle_fill: false,
            effective_partition: Some("cpu".into()),
            memory_adjustment: Some(adjustment),
            preemption_observation: Some(7),
            at: None,
        };
        let back: WalOperation =
            serde_json::from_str(&serde_json::to_string(&op).unwrap()).unwrap();
        match back {
            WalOperation::JobStart {
                effective_partition,
                memory_adjustment,
                preemption_observation,
                ..
            } => {
                assert_eq!(effective_partition.as_deref(), Some("cpu"));
                assert_eq!(memory_adjustment, Some(adjustment));
                assert_eq!(preemption_observation, Some(7));
            }
            _ => panic!("wrong variant"),
        }
    }

    // Frozen PartitionUpdate entry from before partition resource limits; it
    // must still replay, leaving the limits untouched.
    #[test]
    fn partition_update_pre_resource_limits_payload_still_deserializes() {
        const PARTITION_UPDATE_PRE_LIMITS: &str = r#"{"PartitionUpdate":{"name":"cpu","nodes":null,"selector":null,"state":"UP","max_time_minutes":null,"default_time_minutes":null,"max_nodes":null,"min_nodes":null,"allow_accounts":null,"allow_groups":null,"deny_accounts":null,"deny_qos":null,"allow_qos":null,"priority_tier":null,"preempt_mode":null,"is_default":null}}"#;
        let op: WalOperation = serde_json::from_str(PARTITION_UPDATE_PRE_LIMITS)
            .expect("pre-limits PartitionUpdate must deserialize");
        match op {
            WalOperation::PartitionUpdate {
                name,
                resource_limits,
                ..
            } => {
                assert_eq!(name, "cpu");
                assert_eq!(resource_limits, None);
            }
            _ => panic!("wrong variant"),
        }
    }

    // Frozen JobStart entry from before partition limits. Never regenerate.
    #[test]
    fn job_start_pre_partition_limits_payload_still_deserializes() {
        const JOB_START_PRE_LIMITS: &str = r#"{"JobStart":{"job_id":5,"nodes":["n1"],"resources":{"cpus":4,"memory_mb":1024,"devices":{}},"per_node_alloc":{"n1":{"cpus":4,"memory_mb":1024,"devices":{}}},"srun_step_dispatch":false,"run_attempt":1,"idle_fill":false,"at":null}}"#;
        let op: WalOperation = serde_json::from_str(JOB_START_PRE_LIMITS)
            .expect("pre-limits JobStart must deserialize");
        match op {
            WalOperation::JobStart {
                job_id,
                effective_partition,
                memory_adjustment,
                ..
            } => {
                assert_eq!(job_id, 5);
                assert_eq!(effective_partition, None);
                assert_eq!(memory_adjustment, None);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn job_submit_mpi_pmix_round_trips() {
        use crate::job::JobSpec;

        let op = WalOperation::JobSubmit {
            job_id: 99,
            spec: Box::new(JobSpec {
                name: "mpi-job".into(),
                user: "alice".into(),
                mpi: Some("pmix".into()),
                ..Default::default()
            }),
        };
        let json = serde_json::to_string(&op).unwrap();
        let back: WalOperation = serde_json::from_str(&json).unwrap();
        match back {
            WalOperation::JobSubmit { job_id, spec } => {
                assert_eq!(job_id, 99);
                assert_eq!(spec.mpi.as_deref(), Some("pmix"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn job_node_complete_signal_round_trips() {
        let op = WalOperation::JobNodeComplete {
            job_id: 1,
            node_name: "n0".into(),
            exit_code: 0,
            signal: 9,
            run_attempt: 0,
            at: None,
        };
        let json = serde_json::to_string(&op).unwrap();
        let back: WalOperation = serde_json::from_str(&json).unwrap();
        // WalOperation has no PartialEq, so assert the fields rather than the value.
        match back {
            WalOperation::JobNodeComplete {
                job_id,
                node_name,
                exit_code,
                signal,
                ..
            } => {
                assert_eq!(job_id, 1);
                assert_eq!(node_name, "n0");
                assert_eq!(exit_code, 0);
                assert_eq!(signal, 9);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn job_start_frozen_payload_still_deserializes() {
        const START: &str = r#"{"JobStart":{"job_id":7,"nodes":["n1"],"resources":{"cpus":2,"memory_mb":1000,"devices":{}}}}"#;
        let op: WalOperation = serde_json::from_str(START)
            .expect("frozen JobStart must deserialize; a new field needs #[serde(default)]");
        match op {
            WalOperation::JobStart { job_id, at, .. } => {
                assert_eq!(job_id, 7);
                assert!(at.is_none(), "a pre-upgrade entry carries no start instant");
            }
            _ => panic!("wrong variant"),
        }
    }

    // Frozen on-wire shapes. Never regenerate: a failure here means the new
    // field needs a default, not a new fixture.
    #[test]
    fn frozen_pre_stamp_payloads_still_deserialize_undated() {
        const FROZEN: &[&str] = &[
            r#"{"JobComplete":{"job_id":7,"exit_code":0,"state":"COMPLETED"}}"#,
            r#"{"JobNodeComplete":{"job_id":7,"node_name":"n1","exit_code":0,"signal":0}}"#,
            r#"{"JobPreemptRequeue":{"job_id":7,"begin_time":"2026-01-01T00:00:00Z"}}"#,
            r#"{"JobPreemptCancel":{"job_id":7}}"#,
            r#"{"JobUserRequeue":{"job_id":7}}"#,
            r#"{"JobEvict":{"job_id":7}}"#,
            r#"{"NodeRemove":{"name":"n1","reason":null}}"#,
            r#"{"NodeStateChange":{"name":"n1","old_state":"IDLE","new_state":"DOWN","reason":null}}"#,
        ];
        for frozen in FROZEN {
            let op: WalOperation = serde_json::from_str(frozen).unwrap_or_else(|error| {
                panic!("frozen entry must deserialize ({frozen}): {error}")
            });
            assert!(
                op.occurred_at().is_none(),
                "a pre-upgrade entry carries no instant: {frozen}"
            );
        }
    }

    #[test]
    fn stamping_dates_an_entry_once_and_replay_reads_it_back() {
        let at = chrono::DateTime::parse_from_rfc3339("2026-03-04T05:06:07Z")
            .expect("fixture instant")
            .with_timezone(&chrono::Utc);
        let mut op = WalOperation::JobComplete {
            job_id: 7,
            exit_code: 0,
            state: JobState::Completed,
            unreported_nodes: Vec::new(),
            cancel: None,
            controller_initiated: false,
            at: None,
        };
        op.stamp_occurred_at(at);
        assert_eq!(op.occurred_at(), Some(at));

        let later = at + chrono::Duration::hours(3);
        op.stamp_occurred_at(later);
        assert_eq!(
            op.occurred_at(),
            Some(at),
            "an entry that already carries an instant keeps it"
        );

        let round_tripped: WalOperation =
            serde_json::from_str(&serde_json::to_string(&op).expect("serialize"))
                .expect("deserialize");
        assert_eq!(
            round_tripped.occurred_at(),
            Some(at),
            "the instant must survive the log verbatim"
        );
    }
}

#[cfg(test)]
mod deregistration_wal_tests {
    use super::*;

    #[test]
    fn node_remove_round_trips() {
        let op = WalOperation::NodeRemove {
            name: "gpu01".into(),
            reason: Some("decommission".into()),
            at: None,
        };
        let json = serde_json::to_string(&op).unwrap();
        let back: WalOperation = serde_json::from_str(&json).unwrap();
        match back {
            WalOperation::NodeRemove { name, reason, .. } => {
                assert_eq!(name, "gpu01");
                assert_eq!(reason.as_deref(), Some("decommission"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn k0s_wal_variants_round_trip() {
        let op = WalOperation::NodeK0sAssign {
            name: "gpu-node-1".into(),
            role: K0sRole::Worker,
            mesh_ip: "10.44.0.2".into(),
            pod_cidr: "10.42.2.0/24".into(),
        };
        let back: WalOperation =
            serde_json::from_str(&serde_json::to_string(&op).unwrap()).unwrap();
        match back {
            WalOperation::NodeK0sAssign {
                name,
                role,
                mesh_ip,
                pod_cidr,
            } => {
                assert_eq!(name, "gpu-node-1");
                assert_eq!(role, K0sRole::Worker);
                assert_eq!(mesh_ip, "10.44.0.2");
                assert_eq!(pod_cidr, "10.42.2.0/24");
            }
            _ => panic!("wrong variant"),
        }

        let op = WalOperation::NodeK0sClear {
            name: "gpu-node-1".into(),
        };
        let back: WalOperation =
            serde_json::from_str(&serde_json::to_string(&op).unwrap()).unwrap();
        match back {
            WalOperation::NodeK0sClear { name } => assert_eq!(name, "gpu-node-1"),
            _ => panic!("wrong variant"),
        }

        let op = WalOperation::NodeK0sSetError {
            name: "gpu-node-1".into(),
            error: Some("not active after 10m".into()),
        };
        let back: WalOperation =
            serde_json::from_str(&serde_json::to_string(&op).unwrap()).unwrap();
        match back {
            WalOperation::NodeK0sSetError { name, error } => {
                assert_eq!(name, "gpu-node-1");
                assert_eq!(error.as_deref(), Some("not active after 10m"));
            }
            _ => panic!("wrong variant"),
        }

        let op = WalOperation::K0sSetPhase {
            phase: K0sPhase::Ready,
            control_plane_node: Some("head-node".into()),
            control_plane_nodes: vec!["head-node".into(), "cp-2".into(), "cp-3".into()],
            member_nodes: vec![
                "head-node".into(),
                "cp-2".into(),
                "cp-3".into(),
                "w-4".into(),
            ],
            reset_requested: false,
        };
        let back: WalOperation =
            serde_json::from_str(&serde_json::to_string(&op).unwrap()).unwrap();
        match back {
            WalOperation::K0sSetPhase {
                phase,
                control_plane_node,
                control_plane_nodes,
                member_nodes,
                reset_requested,
            } => {
                assert_eq!(phase, K0sPhase::Ready);
                assert_eq!(control_plane_node.as_deref(), Some("head-node"));
                assert_eq!(control_plane_nodes, vec!["head-node", "cp-2", "cp-3"]);
                assert_eq!(member_nodes, vec!["head-node", "cp-2", "cp-3", "w-4"]);
                assert!(!reset_requested);
            }
            _ => panic!("wrong variant"),
        }

        let op = WalOperation::K0sMemberNodesAdd {
            nodes: vec!["gpu-09".into(), "gpu-10".into()],
        };
        let back: WalOperation =
            serde_json::from_str(&serde_json::to_string(&op).unwrap()).unwrap();
        match back {
            WalOperation::K0sMemberNodesAdd { nodes } => {
                assert_eq!(nodes, vec!["gpu-09", "gpu-10"]);
            }
            _ => panic!("wrong variant"),
        }

        let op = WalOperation::K0sMemberNodesRemove {
            nodes: vec!["gpu-09".into()],
        };
        let back: WalOperation =
            serde_json::from_str(&serde_json::to_string(&op).unwrap()).unwrap();
        match back {
            WalOperation::K0sMemberNodesRemove { nodes } => {
                assert_eq!(nodes, vec!["gpu-09"]);
            }
            _ => panic!("wrong variant"),
        }
    }

    // Frozen pre-optional-preempt_mode PartitionCreate entry; `preempt_mode` was a
    // bare `PreemptMode` whose default serialized as "Off". Never regenerate.
    #[test]
    fn partition_create_pre_optional_preempt_mode_still_deserializes() {
        const PARTITION_CREATE_PRE_OPTIONAL: &str = r#"{"PartitionCreate":{"partition":{"name":"gpu","state":"Up","is_default":true,"nodes":"ALL","selector":{},"max_time_minutes":1440,"default_time_minutes":10,"max_nodes":null,"min_nodes":1,"allow_root":true,"exclusive_user":false,"allow_accounts":[],"allow_groups":[],"allow_qos":[],"deny_accounts":[],"deny_qos":[],"preempt_mode":"Off","priority_tier":1,"preempt_exempt_time":null}}}"#;

        let op: WalOperation = serde_json::from_str(PARTITION_CREATE_PRE_OPTIONAL)
            .expect("legacy PartitionCreate must deserialize; a new field needs #[serde(default)]");
        match op {
            WalOperation::PartitionCreate { partition } => {
                assert_eq!(partition.name, "gpu");
                // Legacy "Off" decodes to an explicit Off, which resolves the
                // same as unset — an upgraded cluster keeps its behaviour.
                assert_eq!(
                    partition.preempt_mode,
                    Some(crate::partition::PreemptMode::Off)
                );
            }
            _ => panic!("wrong variant"),
        }
    }

    // Frozen pre-grace_time PartitionUpdate entry; it must still replay or the
    // controller cannot start after an upgrade. Never regenerate.
    #[test]
    fn partition_update_without_grace_time_replays() {
        const PARTITION_UPDATE_PRE_GRACE_TIME: &str = r#"{"PartitionUpdate":{"name":"gpu","nodes":null,"selector":null,"state":"DRAIN","max_time_minutes":null,"default_time_minutes":null,"max_nodes":null,"min_nodes":null,"allow_accounts":null,"allow_groups":null,"deny_accounts":null,"deny_qos":null,"allow_qos":null,"priority_tier":null,"preempt_mode":null,"is_default":null,"preempt_exempt_time":120}}"#;
        let op: WalOperation = serde_json::from_str(PARTITION_UPDATE_PRE_GRACE_TIME)
            .expect("legacy PartitionUpdate must deserialize; a new field needs #[serde(default)]");
        match op {
            WalOperation::PartitionUpdate {
                state,
                preempt_exempt_time,
                grace_time,
                ..
            } => {
                assert_eq!(state.as_deref(), Some("DRAIN"));
                assert_eq!(preempt_exempt_time, Some(120));
                assert_eq!(grace_time, None, "absent means leave unchanged");
            }
            _ => panic!("wrong variant"),
        }
    }

    /// `[max_time, default_time, max_nodes, preempt_exempt_time]`.
    type Limits = [Option<u32>; 4];

    fn partition_update(
        limits: Limits,
        clear: PartitionClears,
        grace_time: Option<Option<u32>>,
    ) -> WalOperation {
        let [max_time_minutes, default_time_minutes, max_nodes, preempt_exempt_time] = limits;
        WalOperation::PartitionUpdate {
            name: "gpu".into(),
            nodes: None,
            selector: None,
            state: None,
            max_time_minutes,
            default_time_minutes,
            max_nodes,
            min_nodes: None,
            allow_accounts: None,
            allow_groups: None,
            deny_accounts: None,
            deny_qos: None,
            allow_qos: None,
            priority_tier: None,
            preempt_mode: None,
            is_default: None,
            preempt_exempt_time,
            grace_time,
            clear,
            resource_limits: None,
        }
    }

    fn partition_update_with_grace(grace_time: Option<Option<u32>>) -> WalOperation {
        partition_update([None; 4], PartitionClears::default(), grace_time)
    }

    // The log stores JSON, where a bare `Option<Option<_>>` cannot tell a clear
    // from "no change"; losing it would resurrect a cleared grace on replay.
    #[test]
    fn partition_update_grace_time_set_clear_and_unchanged_survive_the_log() {
        for grace_time in [None, Some(None), Some(Some(0)), Some(Some(600))] {
            let json = serde_json::to_string(&partition_update_with_grace(grace_time)).unwrap();
            match serde_json::from_str(&json).unwrap() {
                WalOperation::PartitionUpdate {
                    grace_time: replayed,
                    ..
                } => assert_eq!(replayed, grace_time, "{json}"),
                _ => panic!("wrong variant"),
            }
        }
    }

    // Frozen wire forms of `grace_time` on a PartitionUpdate: they are what is
    // already in deployed logs, so a change to how they read changes what a
    // replay does. Never regenerate.
    #[test]
    fn partition_update_grace_time_wire_format_is_frozen() {
        const CLEAR: &str = r#"{"PartitionUpdate":{"name":"gpu","nodes":null,"selector":null,"state":null,"max_time_minutes":null,"default_time_minutes":null,"max_nodes":null,"min_nodes":null,"allow_accounts":null,"allow_groups":null,"deny_accounts":null,"deny_qos":null,"allow_qos":null,"priority_tier":null,"preempt_mode":null,"is_default":null,"preempt_exempt_time":null,"grace_time":null}}"#;
        const SET: &str = r#"{"PartitionUpdate":{"name":"gpu","nodes":null,"selector":null,"state":null,"max_time_minutes":null,"default_time_minutes":null,"max_nodes":null,"min_nodes":null,"allow_accounts":null,"allow_groups":null,"deny_accounts":null,"deny_qos":null,"allow_qos":null,"priority_tier":null,"preempt_mode":null,"is_default":null,"preempt_exempt_time":null,"grace_time":600}}"#;
        const ABSENT: &str = r#"{"PartitionUpdate":{"name":"gpu","nodes":null,"selector":null,"state":null,"max_time_minutes":null,"default_time_minutes":null,"max_nodes":null,"min_nodes":null,"allow_accounts":null,"allow_groups":null,"deny_accounts":null,"deny_qos":null,"allow_qos":null,"priority_tier":null,"preempt_mode":null,"is_default":null,"preempt_exempt_time":null}}"#;

        let replayed = |json: &str| match serde_json::from_str(json).expect("must deserialize") {
            WalOperation::PartitionUpdate { grace_time, .. } => grace_time,
            _ => panic!("wrong variant"),
        };

        assert_eq!(replayed(CLEAR), Some(None), "null clears the grace time");
        assert_eq!(replayed(SET), Some(Some(600)));
        assert_eq!(replayed(ABSENT), None, "an absent key leaves it unchanged");
    }

    fn replayed_limits(json: &str) -> (Limits, PartitionClears) {
        match serde_json::from_str(json).expect("must deserialize") {
            WalOperation::PartitionUpdate {
                max_time_minutes,
                default_time_minutes,
                max_nodes,
                preempt_exempt_time,
                clear,
                ..
            } => (
                [
                    max_time_minutes,
                    default_time_minutes,
                    max_nodes,
                    preempt_exempt_time,
                ],
                clear,
            ),
            _ => panic!("wrong variant"),
        }
    }

    const ALL_CLEARS: PartitionClears = PartitionClears {
        max_time: true,
        default_time: true,
        max_nodes: true,
        preempt_exempt_time: true,
    };

    // The four legacy limits are always serialized, so every old entry carries
    // `null` for "unchanged". It must stay "unchanged", never a clear.
    // Never regenerate.
    #[test]
    fn partition_update_legacy_limit_fields_read_null_as_unchanged_and_numbers_as_sets() {
        const NULLS: &str = r#"{"PartitionUpdate":{"name":"gpu","nodes":null,"selector":null,"state":"UP","max_time_minutes":null,"default_time_minutes":null,"max_nodes":null,"min_nodes":null,"allow_accounts":null,"allow_groups":null,"deny_accounts":null,"deny_qos":null,"allow_qos":null,"priority_tier":null,"preempt_mode":null,"is_default":null,"preempt_exempt_time":null,"grace_time":null,"resource_limits":null}}"#;
        const NUMBERS: &str = r#"{"PartitionUpdate":{"name":"gpu","nodes":null,"selector":null,"state":null,"max_time_minutes":90,"default_time_minutes":30,"max_nodes":8,"min_nodes":null,"allow_accounts":null,"allow_groups":null,"deny_accounts":null,"deny_qos":null,"allow_qos":null,"priority_tier":null,"preempt_mode":null,"is_default":null,"preempt_exempt_time":120}}"#;
        const PRE_PREEMPT_EXEMPT: &str = r#"{"PartitionUpdate":{"name":"gpu","nodes":null,"selector":null,"state":"UP","max_time_minutes":null,"default_time_minutes":null,"max_nodes":null,"min_nodes":null,"allow_accounts":null,"allow_groups":null,"deny_accounts":null,"deny_qos":null,"allow_qos":null,"priority_tier":null,"preempt_mode":null,"is_default":null}}"#;

        assert_eq!(
            replayed_limits(NULLS),
            ([None; 4], PartitionClears::default())
        );
        assert_eq!(
            replayed_limits(NUMBERS),
            (
                [Some(90), Some(30), Some(8), Some(120)],
                PartitionClears::default()
            )
        );
        assert_eq!(
            replayed_limits(PRE_PREEMPT_EXEMPT),
            ([None; 4], PartitionClears::default())
        );
    }

    // A clear is the case a bare `Option<Option<_>>` loses in JSON, which the
    // leader never sees because it applies its in-memory copy.
    #[test]
    fn partition_update_limit_clears_survive_the_log() {
        let clears = [
            PartitionClears {
                max_time: true,
                ..Default::default()
            },
            PartitionClears {
                default_time: true,
                ..Default::default()
            },
            PartitionClears {
                max_nodes: true,
                ..Default::default()
            },
            PartitionClears {
                preempt_exempt_time: true,
                ..Default::default()
            },
            ALL_CLEARS,
        ];
        for clear in clears {
            let json = serde_json::to_string(&partition_update([None; 4], clear, None)).unwrap();
            assert_eq!(replayed_limits(&json), ([None; 4], clear), "{json}");
        }
    }

    #[test]
    fn partition_update_limit_sets_survive_the_log() {
        let limits = [Some(90), Some(0), Some(8), Some(120)];
        let json =
            serde_json::to_string(&partition_update(limits, PartitionClears::default(), None))
                .unwrap();
        assert_eq!(
            replayed_limits(&json),
            (limits, PartitionClears::default()),
            "{json}"
        );
    }

    // Frozen wire form of a clear entry: an older controller reading it ignores
    // `clear` and keeps the limit. Never regenerate.
    #[test]
    fn partition_update_clear_wire_format_is_frozen() {
        const CLEAR: &str = r#"{"PartitionUpdate":{"name":"gpu","nodes":null,"selector":null,"state":null,"max_time_minutes":null,"default_time_minutes":null,"max_nodes":null,"min_nodes":null,"allow_accounts":null,"allow_groups":null,"deny_accounts":null,"deny_qos":null,"allow_qos":null,"priority_tier":null,"preempt_mode":null,"is_default":null,"preempt_exempt_time":null,"clear":{"max_time":true,"default_time":false,"max_nodes":true,"preempt_exempt_time":false},"resource_limits":null}}"#;
        let expected = PartitionClears {
            max_time: true,
            max_nodes: true,
            ..Default::default()
        };

        assert_eq!(replayed_limits(CLEAR), ([None; 4], expected));
        let op = partition_update([None; 4], expected, None);
        assert_eq!(serde_json::to_string(&op).unwrap(), CLEAR);
    }

    // Without clears the entry must serialize as it did before the field existed.
    #[test]
    fn partition_update_without_clears_omits_the_clear_key() {
        let json = serde_json::to_string(&partition_update(
            [Some(60), None, None, None],
            PartitionClears::default(),
            None,
        ))
        .unwrap();
        assert!(!json.contains("clear"), "{json}");
    }

    #[test]
    fn partition_update_ignores_unknown_keys() {
        const WITH_FUTURE_KEY: &str = r#"{"PartitionUpdate":{"name":"gpu","nodes":null,"selector":null,"state":null,"max_time_minutes":60,"default_time_minutes":null,"max_nodes":null,"min_nodes":null,"allow_accounts":null,"allow_groups":null,"deny_accounts":null,"deny_qos":null,"allow_qos":null,"priority_tier":null,"preempt_mode":null,"is_default":null,"preempt_exempt_time":null,"clear":{"max_nodes":true,"a_future_flag":true},"a_future_field":1}}"#;
        let expected = PartitionClears {
            max_nodes: true,
            ..Default::default()
        };
        assert_eq!(
            replayed_limits(WITH_FUTURE_KEY),
            ([Some(60), None, None, None], expected)
        );
    }

    // These entries are already in deployed logs, so a change to how they read
    // changes what a replay does. Never regenerate.
    #[test]
    fn preempt_run_attempt_wire_format_is_frozen() {
        const CANCEL: &str = r#"{"JobPreemptCancel":{"job_id":7,"preempted_by":42,"preempt_qos":null,"at":"2026-03-04T05:06:07Z","run_attempt":3}}"#;
        const REQUEUE: &str = r#"{"JobPreemptRequeue":{"job_id":7,"begin_time":"2999-01-01T00:00:00Z","preempted_by":42,"preempt_qos":null,"at":null,"run_attempt":3}}"#;

        match serde_json::from_str(CANCEL).expect("must deserialize") {
            WalOperation::JobPreemptCancel { run_attempt, .. } => assert_eq!(run_attempt, Some(3)),
            _ => panic!("wrong variant"),
        }
        match serde_json::from_str(REQUEUE).expect("must deserialize") {
            WalOperation::JobPreemptRequeue { run_attempt, .. } => {
                assert_eq!(run_attempt, Some(3))
            }
            _ => panic!("wrong variant"),
        }
        for frozen in [CANCEL, REQUEUE] {
            let op: WalOperation = serde_json::from_str(frozen).expect("must deserialize");
            assert_eq!(
                serde_json::to_string(&op).expect("must serialize"),
                frozen,
                "the entry must keep its wire shape"
            );
        }
    }

    // A legacy entry that carried a real action must keep it.
    #[test]
    fn partition_create_pre_optional_preserves_configured_action() {
        const LEGACY_CANCEL: &str = r#"{"PartitionCreate":{"partition":{"name":"gpu","state":"Up","is_default":false,"nodes":"ALL","selector":{},"max_time_minutes":null,"default_time_minutes":null,"max_nodes":null,"min_nodes":1,"allow_root":true,"exclusive_user":false,"allow_accounts":[],"allow_groups":[],"allow_qos":[],"deny_accounts":[],"deny_qos":[],"preempt_mode":"Cancel","priority_tier":1,"preempt_exempt_time":null}}}"#;

        let op: WalOperation = serde_json::from_str(LEGACY_CANCEL).expect("must deserialize");
        match op {
            WalOperation::PartitionCreate { partition } => assert_eq!(
                partition.preempt_mode,
                Some(crate::partition::PreemptMode::Cancel)
            ),
            _ => panic!("wrong variant"),
        }
    }

    // Frozen pre-multi-CP K0sSetPhase entry (no control_plane_nodes field); must still deserialize
    // or spurctld crashes on upgrade replay. Never regenerate.
    #[test]
    fn k0s_set_phase_pre_multi_cp_payload_still_deserializes() {
        const K0S_SET_PHASE_PRE_MULTI_CP: &str = r#"{"K0sSetPhase":{"phase":"ready","control_plane_node":"head-node","reset_requested":false}}"#;
        let op: WalOperation = serde_json::from_str(K0S_SET_PHASE_PRE_MULTI_CP).expect(
            "pre-multi-CP K0sSetPhase must deserialize; a new field needs #[serde(default)]",
        );
        match op {
            WalOperation::K0sSetPhase {
                phase,
                control_plane_node,
                control_plane_nodes,
                member_nodes,
                reset_requested,
            } => {
                assert_eq!(phase, K0sPhase::Ready);
                assert_eq!(control_plane_node.as_deref(), Some("head-node"));
                assert!(control_plane_nodes.is_empty());
                assert!(member_nodes.is_empty());
                assert!(!reset_requested);
            }
            _ => panic!("wrong variant"),
        }
    }

    // Frozen pre-member-scope K0sSetPhase entry (has control_plane_nodes, no member_nodes); must
    // still deserialize with member_nodes defaulting empty (= whole inventory). Never regenerate.
    #[test]
    fn k0s_set_phase_pre_member_scope_payload_still_deserializes() {
        const K0S_SET_PHASE_PRE_MEMBER_SCOPE: &str = r#"{"K0sSetPhase":{"phase":"ready","control_plane_node":"head-node","control_plane_nodes":["head-node","cp-2","cp-3"],"reset_requested":false}}"#;
        let op: WalOperation = serde_json::from_str(K0S_SET_PHASE_PRE_MEMBER_SCOPE).expect(
            "pre-member-scope K0sSetPhase must deserialize; member_nodes needs #[serde(default)]",
        );
        match op {
            WalOperation::K0sSetPhase {
                control_plane_nodes,
                member_nodes,
                ..
            } => {
                assert_eq!(control_plane_nodes, vec!["head-node", "cp-2", "cp-3"]);
                assert!(member_nodes.is_empty());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn node_remove_none_reason_round_trips() {
        let op = WalOperation::NodeRemove {
            name: "n0".into(),
            reason: None,
            at: None,
        };
        let json = serde_json::to_string(&op).unwrap();
        let back: WalOperation = serde_json::from_str(&json).unwrap();
        match back {
            WalOperation::NodeRemove { name, reason, .. } => {
                assert_eq!(name, "n0");
                assert!(reason.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    // Frozen on-wire shape; a new field without #[serde(default)] fails here
    // instead of crashing a controller replaying this entry after upgrade.
    #[test]
    fn evict_terminal_jobs_frozen_payload_still_deserializes() {
        const EVICT: &str = r#"{"EvictTerminalJobs":{"job_ids":[7,42]}}"#;
        let op: WalOperation = serde_json::from_str(EVICT).expect(
            "frozen EvictTerminalJobs must deserialize; a new field needs #[serde(default)]",
        );
        match op {
            WalOperation::EvictTerminalJobs { job_ids, .. } => {
                assert_eq!(job_ids, vec![7, 42]);
            }
            _ => panic!("wrong variant"),
        }
    }

    // Frozen pre-capabilities Raft entries; a new field needs #[serde(default)],
    // not a fixture edit.
    #[test]
    fn node_register_without_capabilities_still_deserializes() {
        const NODE_REGISTER: &str = r#"{"NodeRegister":{"name":"n1","hostname":"n1","resources":{"cpus":4,"memory_mb":8000,"gpus":[],"generic":{}},"address":"10.0.0.1","port":6818,"wg_pubkey":"","version":"0.1.0","labels":{},"source":{"type":"NativeHost"}}}"#;
        let op: WalOperation = serde_json::from_str(NODE_REGISTER)
            .expect("frozen NodeRegister must deserialize; a new field needs #[serde(default)]");
        match op {
            WalOperation::NodeRegister { capabilities, .. } => assert!(capabilities.is_empty()),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn node_update_without_capabilities_still_deserializes() {
        const NODE_UPDATE: &str = r#"{"NodeUpdate":{"name":"n1","hostname":"n1","resources":{"cpus":4,"memory_mb":8000,"gpus":[],"generic":{}},"address":"10.0.0.1","port":6818,"wg_pubkey":"","version":"0.1.0","source":{"type":"NativeHost"}}}"#;
        let op: WalOperation = serde_json::from_str(NODE_UPDATE)
            .expect("frozen NodeUpdate must deserialize; a new field needs #[serde(default)]");
        match op {
            WalOperation::NodeUpdate { capabilities, .. } => assert!(capabilities.is_empty()),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn node_register_capabilities_round_trip() {
        let op = WalOperation::NodeRegister {
            name: "n1".into(),
            hostname: "n1".into(),
            resources: ResourceSet::default(),
            address: "10.0.0.1".into(),
            port: 6818,
            wg_pubkey: String::new(),
            version: String::new(),
            labels: HashMap::new(),
            source: NodeSource::NativeHost,
            capabilities: vec![crate::node::CAP_SIGNAL_JOB.to_string()],
        };
        let json = serde_json::to_string(&op).expect("serialize");
        let back: WalOperation = serde_json::from_str(&json).expect("deserialize");
        match back {
            WalOperation::NodeRegister { capabilities, .. } => {
                assert_eq!(capabilities, vec!["signal-job".to_string()]);
            }
            _ => panic!("wrong variant"),
        }
    }
}

#[cfg(test)]
mod suspend_wal_tests {
    use super::*;

    #[test]
    fn preempt_cancel_op_round_trips() {
        let stamped = chrono::Utc::now();
        let op = WalOperation::JobPreemptCancel {
            preemption_claim: None,
            job_id: 7,
            preempted_by: Some(3),
            preempt_qos: Some("burst".into()),
            at: Some(stamped),
            run_attempt: Some(4),
        };
        assert_eq!(op.occurred_at(), Some(stamped));
        let json = serde_json::to_string(&op).unwrap();
        let back: WalOperation = serde_json::from_str(&json).unwrap();
        match back {
            WalOperation::JobPreemptCancel {
                preemption_claim: _,
                job_id,
                preempted_by,
                preempt_qos,
                at,
                run_attempt,
            } => {
                assert_eq!(job_id, 7);
                assert_eq!(preempted_by, Some(3));
                assert_eq!(preempt_qos.as_deref(), Some("burst"));
                assert_eq!(at, Some(stamped));
                assert_eq!(run_attempt, Some(4));
            }
            _ => panic!("wrong variant"),
        }

        // Deserializing a legacy entry (no provenance fields) must succeed with None defaults.
        let legacy = r#"{"JobPreemptCancel":{"job_id":7}}"#;
        let back: WalOperation = serde_json::from_str(legacy).unwrap();
        match back {
            WalOperation::JobPreemptCancel {
                preemption_claim: _,
                job_id,
                preempted_by,
                preempt_qos,
                at,
                run_attempt,
            } => {
                assert_eq!(job_id, 7);
                assert_eq!(preempted_by, None, "legacy entry defaults to None");
                assert_eq!(preempt_qos, None, "legacy entry defaults to None");
                assert_eq!(at, None, "legacy entry falls back to apply time");
                assert_eq!(run_attempt, None, "legacy entry checks no run");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn preempt_requeue_op_round_trips() {
        let begin_time = chrono::Utc::now();
        let op = WalOperation::JobPreemptRequeue {
            preemption_claim: None,
            handoff: true,
            job_id: 42,
            begin_time,
            preempted_by: Some(7),
            preempt_qos: Some("highprio".into()),
            at: None,
            run_attempt: Some(2),
        };
        let json = serde_json::to_string(&op).unwrap();
        let back: WalOperation = serde_json::from_str(&json).unwrap();
        match back {
            WalOperation::JobPreemptRequeue {
                job_id,
                begin_time: b,
                preempted_by,
                preempt_qos,
                run_attempt,
                handoff,
                ..
            } => {
                assert!(handoff);
                assert_eq!(job_id, 42);
                assert_eq!(b, begin_time);
                assert_eq!(preempted_by, Some(7));
                assert_eq!(preempt_qos.as_deref(), Some("highprio"));
                assert_eq!(run_attempt, Some(2));
            }
            _ => panic!("wrong variant"),
        }

        // Deserializing a legacy entry (no provenance fields) must succeed with None defaults.
        let legacy = r#"{"JobPreemptRequeue":{"job_id":42,"begin_time":"2026-01-01T00:00:00Z"}}"#;
        let back: WalOperation = serde_json::from_str(legacy).unwrap();
        match back {
            WalOperation::JobPreemptRequeue {
                preempted_by,
                preempt_qos,
                run_attempt,
                handoff,
                ..
            } => {
                assert!(!handoff, "legacy entry hands nothing off");
                assert_eq!(preempted_by, None, "legacy entry defaults to None");
                assert_eq!(preempt_qos, None, "legacy entry defaults to None");
                assert_eq!(run_attempt, None, "legacy entry checks no run");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn preempt_requeue_legacy_provenance_decodes_without_handoff() {
        let json = r#"{"JobPreemptRequeue":{"job_id":42,"begin_time":"2026-01-01T00:00:00Z","preempted_by":7,"preempt_qos":"highprio","at":null}}"#;
        let op: WalOperation = serde_json::from_str(json).unwrap();
        assert!(matches!(
            op,
            WalOperation::JobPreemptRequeue {
                preempted_by: Some(7),
                handoff: false,
                ..
            }
        ));
    }

    #[test]
    fn user_requeue_op_round_trips() {
        let begin = chrono::Utc::now();
        for (hold, begin_time) in [(false, Some(begin)), (true, None), (false, None)] {
            let op = WalOperation::JobUserRequeue {
                job_id: 42,
                hold,
                begin_time,
                by: None,
                at: None,
            };
            let json = serde_json::to_string(&op).unwrap();
            let back: WalOperation = serde_json::from_str(&json).unwrap();
            match back {
                WalOperation::JobUserRequeue {
                    job_id,
                    hold: got_hold,
                    begin_time: got_begin,
                    ..
                } => {
                    assert_eq!(job_id, 42);
                    assert_eq!(got_hold, hold);
                    assert_eq!(got_begin, begin_time);
                }
                _ => panic!("wrong variant"),
            }
        }
    }

    /// The optional fields default when absent, so a minimal/older-encoded
    /// entry still replays.
    #[test]
    fn user_requeue_op_defaults_missing_fields() {
        let json = r#"{"JobUserRequeue":{"job_id":7}}"#;
        let back: WalOperation = serde_json::from_str(json).unwrap();
        match back {
            WalOperation::JobUserRequeue {
                job_id,
                hold,
                begin_time,
                by,
                ..
            } => {
                assert_eq!(job_id, 7);
                assert!(!hold);
                assert_eq!(begin_time, None);
                assert_eq!(by, None);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn suspend_resume_ops_round_trip() {
        let at = chrono::Utc::now();
        for op in [
            WalOperation::JobSuspend {
                job_id: 7,
                at,
                preempted_by: None,
                preempt_qos: None,
                by: None,
            },
            WalOperation::JobResume { job_id: 7, at },
        ] {
            let json = serde_json::to_string(&op).unwrap();
            let back: WalOperation = serde_json::from_str(&json).unwrap();
            match (op, back) {
                (
                    WalOperation::JobSuspend {
                        job_id: a,
                        at: at_a,
                        ..
                    },
                    WalOperation::JobSuspend {
                        job_id: b,
                        at: at_b,
                        ..
                    },
                ) => {
                    assert_eq!(a, b);
                    assert_eq!(at_a, at_b);
                }
                (
                    WalOperation::JobResume {
                        job_id: a,
                        at: at_a,
                    },
                    WalOperation::JobResume {
                        job_id: b,
                        at: at_b,
                    },
                ) => {
                    assert_eq!(a, b);
                    assert_eq!(at_a, at_b);
                }
                _ => panic!("variant mismatch after round-trip"),
            }
        }
    }

    #[test]
    fn job_time_limit_signaled_op_round_trips() {
        let at = chrono::Utc::now();
        let op = WalOperation::JobTimeLimitSignaled { job_id: 13, at };
        let json = serde_json::to_string(&op).unwrap();
        let back: WalOperation = serde_json::from_str(&json).unwrap();
        match back {
            WalOperation::JobTimeLimitSignaled {
                job_id,
                at: at_back,
            } => {
                assert_eq!(job_id, 13);
                assert_eq!(at_back, at);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn job_time_limit_expired_has_a_frozen_wire_form() {
        let at = chrono::DateTime::parse_from_rfc3339("2026-03-04T05:06:07Z")
            .expect("fixture instant")
            .with_timezone(&chrono::Utc);
        let frozen =
            r#"{"JobTimeLimitExpired":{"job_id":13,"run_attempt":2,"at":"2026-03-04T05:06:07Z"}}"#;
        let op = WalOperation::JobTimeLimitExpired {
            job_id: 13,
            run_attempt: 2,
            at,
        };
        assert_eq!(serde_json::to_string(&op).unwrap(), frozen);

        let mut back: WalOperation = serde_json::from_str(frozen).unwrap();
        assert!(matches!(
            back,
            WalOperation::JobTimeLimitExpired { job_id: 13, run_attempt: 2, at: a } if a == at
        ));
        assert_eq!(back.occurred_at(), Some(at));
        back.stamp_occurred_at(at + chrono::Duration::hours(1));
        assert_eq!(back.occurred_at(), Some(at), "it carries its own stamp");
    }

    #[test]
    fn job_preempt_signaled_has_a_frozen_wire_form() {
        let at = chrono::DateTime::parse_from_rfc3339("2026-03-04T05:06:07Z")
            .expect("fixture instant")
            .with_timezone(&chrono::Utc);
        let deadline = at + chrono::Duration::seconds(90);
        let frozen = concat!(
            r#"{"JobPreemptSignaled":{"job_id":13,"pending":{"#,
            r#""signaled_at":"2026-03-04T05:06:07Z","deadline":"2026-03-04T05:07:37Z","#,
            r#""preempted_by":21,"preempt_qos":"high","mode":"Requeue","#,
            r#""run_attempt":2,"requeue_hold_secs":30}}}"#
        );
        let pending = crate::job::PreemptPending {
            preemption_claim: None,
            signaled_at: at,
            deadline,
            preempted_by: 21,
            preempt_qos: Some("high".into()),
            mode: crate::partition::PreemptMode::Requeue,
            run_attempt: 2,
            requeue_hold_secs: 30,
        };
        let op = WalOperation::JobPreemptSignaled {
            job_id: 13,
            pending: pending.clone(),
        };
        assert_eq!(serde_json::to_string(&op).unwrap(), frozen);

        let mut back: WalOperation = serde_json::from_str(frozen).unwrap();
        assert!(matches!(
            &back,
            WalOperation::JobPreemptSignaled { job_id: 13, pending: p } if *p == pending
        ));
        assert_eq!(back.occurred_at(), Some(at));
        back.stamp_occurred_at(at + chrono::Duration::hours(1));
        assert_eq!(back.occurred_at(), Some(at), "it carries its own stamp");
    }

    #[test]
    fn job_cancel_signaled_op_round_trips() {
        let at = chrono::Utc::now();
        let op = WalOperation::JobCancelSignaled {
            job_id: 21,
            at,
            cancel: None,
        };
        let json = serde_json::to_string(&op).unwrap();
        let back: WalOperation = serde_json::from_str(&json).unwrap();
        match back {
            WalOperation::JobCancelSignaled {
                job_id,
                at: at_back,
                ..
            } => {
                assert_eq!(job_id, 21);
                assert_eq!(at_back, at);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn a_job_complete_written_before_unreported_nodes_existed_still_replays() {
        let old = r#"{"JobComplete":{"job_id":5,"exit_code":1,"state":"FAILED"}}"#;
        let op: WalOperation = serde_json::from_str(old).unwrap();
        assert!(matches!(
            op,
            WalOperation::JobComplete { job_id: 5, ref unreported_nodes, .. }
                if unreported_nodes.is_empty()
        ));
    }

    #[test]
    fn a_job_complete_written_before_controller_initiated_existed_replays_as_the_runs_own_end() {
        let old = r#"{"JobComplete":{"job_id":5,"exit_code":1,"state":"FAILED"}}"#;
        let op: WalOperation = serde_json::from_str(old).unwrap();
        assert!(matches!(
            op,
            WalOperation::JobComplete {
                job_id: 5,
                controller_initiated: false,
                ..
            }
        ));
    }

    #[test]
    fn a_controller_initiated_job_complete_has_a_frozen_wire_form() {
        let frozen = r#"{"JobComplete":{"job_id":5,"exit_code":-1,"state":"FAILED","controller_initiated":true}}"#;
        let op: WalOperation = serde_json::from_str(frozen).unwrap();
        assert!(matches!(
            op,
            WalOperation::JobComplete {
                job_id: 5,
                exit_code: -1,
                state: JobState::Failed,
                controller_initiated: true,
                ..
            }
        ));
        assert_eq!(
            serde_json::to_string(&WalOperation::job_failed_by_controller(5)).unwrap(),
            frozen
        );
    }

    #[test]
    fn a_cancel_signal_written_before_provenance_existed_still_replays() {
        let old = r#"{"JobCancelSignaled":{"job_id":5,"at":"2026-10-13T00:00:00Z"}}"#;
        let op: WalOperation = serde_json::from_str(old).unwrap();
        assert!(matches!(
            op,
            WalOperation::JobCancelSignaled {
                job_id: 5,
                cancel: None,
                ..
            }
        ));
    }

    #[test]
    fn a_suspend_written_before_the_actor_existed_still_replays() {
        let old = r#"{"JobSuspend":{"job_id":5,"at":"2026-10-13T00:00:00Z"}}"#;
        let op: WalOperation = serde_json::from_str(old).unwrap();
        assert!(matches!(
            op,
            WalOperation::JobSuspend {
                job_id: 5,
                by: None,
                ..
            }
        ));
    }

    #[test]
    fn an_ordinary_job_complete_serializes_as_before() {
        let op = WalOperation::job_complete(5, 1, JobState::Failed);
        assert_eq!(
            serde_json::to_string(&op).unwrap(),
            r#"{"JobComplete":{"job_id":5,"exit_code":1,"state":"FAILED"}}"#
        );
    }
}

#[cfg(test)]
mod evict_wal_tests {
    use super::*;
    use crate::step::{JobStep, StepState, TaskDistribution};

    #[test]
    fn job_evict_op_round_trips() {
        let op = WalOperation::JobEvict {
            job_id: 9,
            detail: Some("PMIx prepare failed".into()),
            at: None,
        };
        let json = serde_json::to_string(&op).unwrap();
        let back: WalOperation = serde_json::from_str(&json).unwrap();
        match back {
            WalOperation::JobEvict { job_id, detail, .. } => {
                assert_eq!(job_id, 9);
                assert_eq!(detail.as_deref(), Some("PMIx prepare failed"));
            }
            _ => panic!("wrong variant"),
        }

        let detail_op = WalOperation::JobLaunchFailureDetail {
            job_id: 42,
            detail: "PMIx prepare failed: n1: connect failed".into(),
        };
        let detail_json = serde_json::to_string(&detail_op).unwrap();
        let detail_back: WalOperation = serde_json::from_str(&detail_json).unwrap();
        match detail_back {
            WalOperation::JobLaunchFailureDetail { job_id, detail } => {
                assert_eq!(job_id, 42);
                assert_eq!(detail, "PMIx prepare failed: n1: connect failed");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn job_step_create_op_round_trips() {
        let step = JobStep {
            job_id: 7,
            step_id: 1,
            name: "hostname".into(),
            state: StepState::Running,
            num_tasks: 2,
            cpus_per_task: 1,
            resources: Default::default(),
            nodes: vec!["n1".into(), "n2".into()],
            distribution: TaskDistribution::Block,
            start_time: None,
            end_time: None,
            exit_code: None,
            pty: false,
            job_shell: false,
        };
        let op = WalOperation::JobStepCreate {
            step: Box::new(step.clone()),
        };
        let json = serde_json::to_string(&op).unwrap();
        let back: WalOperation = serde_json::from_str(&json).unwrap();
        match back {
            WalOperation::JobStepCreate { step: restored } => {
                assert_eq!(restored.job_id, 7);
                assert_eq!(restored.step_id, 1);
                assert_eq!(restored.name, "hostname");
            }
            _ => panic!("wrong variant"),
        }
    }
}
