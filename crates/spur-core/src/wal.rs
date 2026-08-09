// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

use crate::admission::AdmissionToken;
use crate::job::{JobId, JobSpec, JobState, PendingReason};
use crate::k0s::{K0sPhase, K0sRole};
use crate::node::{NodeSource, NodeState};
use crate::partition::Partition;
use crate::reservation::Reservation;
use std::collections::HashMap;

use crate::resource::{ResourceAllocations, ResourceSet};

/// A controller-owned resource hold that remains in force after a cancel or
/// eviction until the owning node confirms that it no longer holds the job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingKillReservation {
    pub job_id: JobId,
    pub node: String,
    pub resources: ResourceAllocations,
    pub attempt: u64,
    #[serde(default)]
    pub run_attempt: u32,
}

/// Identity of a pending-kill hold that a node heartbeat has confirmed released.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingKillRelease {
    pub job_id: JobId,
    pub node: String,
    pub attempt: u64,
}

fn default_port() -> u16 {
    6818
}

fn default_job_evict_reason() -> PendingReason {
    PendingReason::JobLaunchFailure
}

/// All state-mutating operations that get logged to the Raft log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WalOperation {
    PendingKillReserve {
        reservations: Vec<PendingKillReservation>,
    },
    /// Applies a job lifecycle transition and its release holds in one Raft
    /// entry, so a leader change cannot expose the freed allocation alone.
    PendingKillTransition {
        reservations: Vec<PendingKillReservation>,
        operation: Box<WalOperation>,
    },
    /// A heartbeat established that these exact pending-kill holds are gone.
    PendingKillRelease {
        releases: Vec<PendingKillRelease>,
    },

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
    },
    JobStart {
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
    },
    JobComplete {
        job_id: JobId,
        exit_code: i32,
        state: JobState,
    },
    JobNodeComplete {
        job_id: JobId,
        node_name: String,
        exit_code: i32,
        signal: i32,
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
    },
    /// Back off a job that failed dispatch before ever leaving Pending, where
    /// `JobStateChange`'s transition-gated backoff can't apply. NoOp on replay
    /// if the job has since left Pending.
    JobDispatchBackoff {
        job_id: JobId,
        begin_time: chrono::DateTime<chrono::Utc>,
    },
    /// Persist the epoch before dispatching any agent RPC so delayed requests
    /// from an aborted dispatch cannot target its retry.
    JobDispatchAttempt {
        job_id: JobId,
        run_attempt: u32,
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
        job_id: JobId,
        begin_time: chrono::DateTime<chrono::Utc>,
    },
    JobSuspend {
        job_id: JobId,
        /// Controller-stamped instant of suspension (for replay-deterministic accounting).
        at: chrono::DateTime<chrono::Utc>,
    },
    JobResume {
        job_id: JobId,
        /// Controller-stamped instant of resume.
        at: chrono::DateTime<chrono::Utc>,
    },
    /// Evict a single job to NodeFail: same effect as a node health-check
    /// failure, but scoped to one job. `reason` drives the requeue path
    /// (e.g. `JobLaunchFailure` backs off, `NodeDown` retries immediately).
    JobEvict {
        job_id: JobId,
        #[serde(default = "default_job_evict_reason")]
        reason: PendingReason,
        /// Human-readable bootstrap failure (shown via scontrol / logs).
        #[serde(default)]
        detail: Option<String>,
        /// Caller's observed epoch; 0 (legacy/unfenced) always applies.
        /// A mismatch at apply time means the job already moved on.
        #[serde(default)]
        run_attempt: u32,
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
    },
    NodeStateChange {
        name: String,
        old_state: NodeState,
        new_state: NodeState,
        reason: Option<String>,
        #[serde(default)]
        admin_locked: bool,
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
        max_time_minutes: Option<Option<u32>>,
        default_time_minutes: Option<Option<u32>>,
        max_nodes: Option<Option<u32>>,
        min_nodes: Option<u32>,
        allow_accounts: Option<Vec<String>>,
        allow_groups: Option<Vec<String>>,
        deny_accounts: Option<Vec<String>>,
        deny_qos: Option<Vec<String>>,
        allow_qos: Option<Vec<String>>,
        priority_tier: Option<u32>,
        preempt_mode: Option<String>,
        is_default: Option<bool>,
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
        reset_requested: bool,
    },
    NodeK0sClear {
        name: String,
    },

    /// Evict the named terminal jobs to bound controller memory. The leader
    /// resolves the aged-out set so every replica evicts identically.
    EvictTerminalJobs {
        job_ids: Vec<JobId>,
    },
}

impl WalOperation {
    pub fn job_state_change(job_id: JobId, old_state: JobState, new_state: JobState) -> Self {
        Self::JobStateChange {
            job_id,
            old_state,
            new_state,
            pending_reason: None,
            pending_priority: None,
            begin_time: None,
            pending_reason_desc: None,
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
            job_id,
            nodes,
            resources,
            per_node_alloc,
            srun_step_dispatch: false,
            run_attempt: 0,
        }
    }
}

#[cfg(test)]
mod job_state_change_wal_tests {
    use super::*;

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
        };
        let json = serde_json::to_string(&op).unwrap();
        let back: WalOperation = serde_json::from_str(&json).unwrap();
        match back {
            WalOperation::JobDispatchBackoff { job_id, begin_time } => {
                assert_eq!(job_id, 8);
                assert_eq!(begin_time, hold);
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
        };
        let json = serde_json::to_string(&op).unwrap();
        let back: WalOperation = serde_json::from_str(&json).unwrap();
        match back {
            WalOperation::JobPriorityChange {
                job_id,
                new_priority,
                pending_reason,
                pending_reason_desc,
                ..
            } => {
                assert_eq!(job_id, 4);
                assert_eq!(new_priority, 0);
                assert_eq!(pending_reason, Some(PendingReason::Held));
                assert_eq!(
                    pending_reason_desc.as_deref(),
                    Some("launch failed requeued held")
                );
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn pre_upgrade_job_priority_change_without_description_deserializes() {
        // A WAL written before pending_reason_desc existed on this variant
        // must still replay (e.g. hold_job / release_job entries from before
        // this fix).
        let json = r#"{"JobPriorityChange":{"job_id":4,"old_priority":500,"new_priority":0,"pending_reason":"Held"}}"#;
        let back: WalOperation = serde_json::from_str(json).unwrap();
        match back {
            WalOperation::JobPriorityChange {
                job_id,
                pending_reason,
                pending_reason_desc,
                reset_requeue_count,
                clear_reservation,
                ..
            } => {
                assert_eq!(job_id, 4);
                assert_eq!(pending_reason, Some(PendingReason::Held));
                assert_eq!(pending_reason_desc, None);
                assert!(!reset_requeue_count);
                assert!(!clear_reservation);
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
            } => {
                assert_eq!(job_id, 1);
                assert_eq!(node_name, "n0");
                assert_eq!(exit_code, 0);
                assert_eq!(signal, 9);
            }
            _ => panic!("wrong variant"),
        }
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
        };
        let json = serde_json::to_string(&op).unwrap();
        let back: WalOperation = serde_json::from_str(&json).unwrap();
        match back {
            WalOperation::NodeRemove { name, reason } => {
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

        let op = WalOperation::K0sSetPhase {
            phase: K0sPhase::Ready,
            control_plane_node: Some("head-node".into()),
            control_plane_nodes: vec!["head-node".into(), "cp-2".into(), "cp-3".into()],
            reset_requested: false,
        };
        let back: WalOperation =
            serde_json::from_str(&serde_json::to_string(&op).unwrap()).unwrap();
        match back {
            WalOperation::K0sSetPhase {
                phase,
                control_plane_node,
                control_plane_nodes,
                reset_requested,
            } => {
                assert_eq!(phase, K0sPhase::Ready);
                assert_eq!(control_plane_node.as_deref(), Some("head-node"));
                assert_eq!(control_plane_nodes, vec!["head-node", "cp-2", "cp-3"]);
                assert!(!reset_requested);
            }
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
                reset_requested,
            } => {
                assert_eq!(phase, K0sPhase::Ready);
                assert_eq!(control_plane_node.as_deref(), Some("head-node"));
                assert!(control_plane_nodes.is_empty());
                assert!(!reset_requested);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn node_remove_none_reason_round_trips() {
        let op = WalOperation::NodeRemove {
            name: "n0".into(),
            reason: None,
        };
        let json = serde_json::to_string(&op).unwrap();
        let back: WalOperation = serde_json::from_str(&json).unwrap();
        match back {
            WalOperation::NodeRemove { name, reason } => {
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
            WalOperation::EvictTerminalJobs { job_ids } => {
                assert_eq!(job_ids, vec![7, 42]);
            }
            _ => panic!("wrong variant"),
        }
    }
}

#[cfg(test)]
mod suspend_wal_tests {
    use super::*;

    #[test]
    fn preempt_requeue_op_round_trips() {
        let begin_time = chrono::Utc::now();
        let op = WalOperation::JobPreemptRequeue {
            job_id: 42,
            begin_time,
        };
        let json = serde_json::to_string(&op).unwrap();
        let back: WalOperation = serde_json::from_str(&json).unwrap();
        match back {
            WalOperation::JobPreemptRequeue {
                job_id,
                begin_time: b,
            } => {
                assert_eq!(job_id, 42);
                assert_eq!(b, begin_time);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn suspend_resume_ops_round_trip() {
        let at = chrono::Utc::now();
        for op in [
            WalOperation::JobSuspend { job_id: 7, at },
            WalOperation::JobResume { job_id: 7, at },
        ] {
            let json = serde_json::to_string(&op).unwrap();
            let back: WalOperation = serde_json::from_str(&json).unwrap();
            match (op, back) {
                (
                    WalOperation::JobSuspend {
                        job_id: a,
                        at: at_a,
                    },
                    WalOperation::JobSuspend {
                        job_id: b,
                        at: at_b,
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
}

#[cfg(test)]
mod evict_wal_tests {
    use super::*;
    use crate::step::{JobStep, StepState, TaskDistribution};

    #[test]
    fn job_evict_op_round_trips() {
        let op = WalOperation::JobEvict {
            job_id: 9,
            reason: PendingReason::NodeDown,
            detail: Some("PMIx prepare failed".into()),
            run_attempt: 3,
        };
        let json = serde_json::to_string(&op).unwrap();
        let back: WalOperation = serde_json::from_str(&json).unwrap();
        match back {
            WalOperation::JobEvict {
                job_id,
                reason,
                detail,
                run_attempt,
            } => {
                assert_eq!(job_id, 9);
                assert_eq!(reason, PendingReason::NodeDown);
                assert_eq!(detail.as_deref(), Some("PMIx prepare failed"));
                assert_eq!(run_attempt, 3);
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
    fn job_evict_op_deserializes_frozen_pre_reason_and_detail_payload() {
        let frozen = r#"{"JobEvict":{"job_id":9}}"#;
        let op: WalOperation = serde_json::from_str(frozen).unwrap();
        match op {
            WalOperation::JobEvict {
                job_id,
                reason,
                detail,
                run_attempt,
            } => {
                assert_eq!(job_id, 9);
                assert_eq!(reason, PendingReason::JobLaunchFailure);
                assert_eq!(detail, None);
                assert_eq!(run_attempt, 0);
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
