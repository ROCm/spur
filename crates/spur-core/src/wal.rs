// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

use crate::admission::AdmissionToken;
use crate::job::{JobId, JobSpec, JobState, PendingReason};
use crate::k0s::{K0sPhase, K0sRole};
use crate::node::NodeState;
use crate::reservation::Reservation;
use std::collections::HashMap;

use crate::resource::{ResourceAllocations, ResourceSet};

fn default_port() -> u16 {
    6818
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
        /// When true, clears automatic requeue counter (admin release after max requeue).
        #[serde(default)]
        reset_requeue_count: bool,
        /// When true, clears `spec.reservation` (admin release after reservation delete hold).
        #[serde(default)]
        clear_reservation: bool,
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
    /// failure (frees allocations, feeds the auto-requeue path), but scoped
    /// to one job instead of every job on a node. Used when a subset of a
    /// job's assigned nodes never received the launch dispatch.
    JobEvict {
        job_id: JobId,
    },

    // Node operations
    NodeRegister {
        name: String,
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
    },
    NodeUpdate {
        name: String,
        resources: ResourceSet,
        address: String,
        port: u16,
        wg_pubkey: String,
        version: String,
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
        reset_requested: bool,
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
            } => {
                assert_eq!(job_id, 1);
                assert_eq!(old_state, JobState::Preempted);
                assert_eq!(new_state, JobState::Pending);
                assert_eq!(pending_reason, Some(PendingReason::JobHoldMaxRequeue));
                assert_eq!(pending_priority, Some(0));
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
                ..
            } => {
                assert_eq!(pending_reason, None);
                assert_eq!(pending_priority, None);
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

// Backward-compatibility guard for the WAL log-entry surface. spurctld persists
// every `WalOperation` as JSON and replays it on startup under strict recovery, so
// a field added to any embedded type without `#[serde(default)]` (or `Option`)
// makes old entries fail to deserialize and crashes the controller on upgrade.
//
// Each `FIXTURES` entry is a frozen JSON blob for one variant; collections hold at
// least one element so serde descends into the element structs (an empty Vec/Map
// would leave them unguarded). They must NOT be regenerated: a failure here means a
// new field needs `#[serde(default)]`, not a fixture edit. `variant_tag` is an
// exhaustive match (no `_` arm), so a new `WalOperation` variant breaks compilation
// until an arm — and a matching frozen fixture — is added.
//
// Scope/limits: this guards only the added-non-defaulted-field case (serde ignores
// unknown fields, so renames/removals are not caught here). The separate snapshot
// blob (`ClusterSnapshot`, full `Job`/`Node`) is a distinct surface, not covered.
#[cfg(test)]
mod backcompat_fixtures {
    use super::*;

    fn variant_tag(op: &WalOperation) -> &'static str {
        match op {
            WalOperation::JobSubmit { .. } => "JobSubmit",
            WalOperation::JobStateChange { .. } => "JobStateChange",
            WalOperation::JobStart { .. } => "JobStart",
            WalOperation::JobComplete { .. } => "JobComplete",
            WalOperation::JobNodeComplete { .. } => "JobNodeComplete",
            WalOperation::JobStepComplete { .. } => "JobStepComplete",
            WalOperation::JobStepCreate { .. } => "JobStepCreate",
            WalOperation::JobPriorityChange { .. } => "JobPriorityChange",
            WalOperation::JobPreemptRequeue { .. } => "JobPreemptRequeue",
            WalOperation::JobSuspend { .. } => "JobSuspend",
            WalOperation::JobResume { .. } => "JobResume",
            WalOperation::JobEvict { .. } => "JobEvict",
            WalOperation::NodeRegister { .. } => "NodeRegister",
            WalOperation::NodeUpdate { .. } => "NodeUpdate",
            WalOperation::NodeStateChange { .. } => "NodeStateChange",
            WalOperation::NodeLabelsUpdate { .. } => "NodeLabelsUpdate",
            WalOperation::NodeRemove { .. } => "NodeRemove",
            WalOperation::TokenCreate { .. } => "TokenCreate",
            WalOperation::TokenRevoke { .. } => "TokenRevoke",
            WalOperation::ReservationCreate { .. } => "ReservationCreate",
            WalOperation::ReservationUpdate { .. } => "ReservationUpdate",
            WalOperation::ReservationDelete { .. } => "ReservationDelete",
            WalOperation::NodeK0sAssign { .. } => "NodeK0sAssign",
            WalOperation::K0sSetPhase { .. } => "K0sSetPhase",
        }
    }

    const FIXTURES: &[(&str, &str)] = &[
        (
            "JobSubmit",
            r#"{"JobSubmit":{"job_id":7,"spec":{"name":"","partition":null,"account":null,"user":"","uid":0,"gid":0,"num_nodes":1,"num_tasks":1,"tasks_per_node":null,"cpus_per_task":1,"memory_per_node_mb":null,"memory_per_cpu_mb":null,"gres":[],"gpus":null,"gpus_per_node":null,"gpus_per_task":null,"script":null,"argv":[],"script_args":[],"work_dir":"/tmp","stdout_path":null,"stderr_path":null,"stdin_path":null,"environment":{},"time_limit":null,"time_min":null,"qos":null,"priority":null,"reservation":null,"dependency":[],"nodelist":null,"exclude":null,"constraint":null,"mpi":null,"distribution":null,"het_group":null,"array_spec":null,"array_job_id":null,"array_task_id":null,"array_max_concurrent":null,"requeue":false,"exclusive":false,"hold":false,"interactive":false,"srun_job":false,"mail_type":[],"mail_user":null,"comment":null,"wckey":null,"container_image":null,"container_mounts":[],"container_workdir":null,"container_name":null,"container_readonly":false,"container_mount_home":false,"container_env":{},"container_entrypoint":null,"container_remap_root":false,"burst_buffer":null,"begin_time":null,"deadline":null,"spread_job":false,"topology":null,"host_network":false,"privileged":false,"host_ipc":false,"shm_size":null,"extra_resources":{},"open_mode":null,"pty":false}}}"#,
        ),
        (
            "JobStateChange",
            r#"{"JobStateChange":{"job_id":7,"old_state":"PENDING","new_state":"RUNNING","pending_reason":null,"pending_priority":null}}"#,
        ),
        (
            "JobStart",
            r#"{"JobStart":{"job_id":7,"nodes":["n0"],"resources":{"cpus":0,"memory_mb":0,"devices":{"gpu":[{"device_id":0,"count":1}]}},"per_node_alloc":{},"srun_step_dispatch":false,"run_attempt":0}}"#,
        ),
        (
            "JobComplete",
            r#"{"JobComplete":{"job_id":7,"exit_code":0,"state":"COMPLETED"}}"#,
        ),
        (
            "JobNodeComplete",
            r#"{"JobNodeComplete":{"job_id":7,"node_name":"n0","exit_code":0,"signal":0}}"#,
        ),
        (
            "JobStepComplete",
            r#"{"JobStepComplete":{"job_id":7,"step_id":0,"exit_code":0}}"#,
        ),
        (
            "JobStepCreate",
            r#"{"JobStepCreate":{"step":{"job_id":7,"step_id":0,"name":"s","state":"Pending","num_tasks":1,"cpus_per_task":1,"resources":{"cpus":0,"memory_mb":0,"devices":{"gpu":[{"device_id":0,"count":1}]}},"nodes":["n0"],"distribution":"Block","start_time":null,"end_time":null,"exit_code":null}}}"#,
        ),
        (
            "JobPriorityChange",
            r#"{"JobPriorityChange":{"job_id":7,"old_priority":0,"new_priority":1,"pending_reason":null,"reset_requeue_count":false,"clear_reservation":false}}"#,
        ),
        (
            "JobPreemptRequeue",
            r#"{"JobPreemptRequeue":{"job_id":7,"begin_time":"2026-01-01T00:00:00Z"}}"#,
        ),
        (
            "JobSuspend",
            r#"{"JobSuspend":{"job_id":7,"at":"2026-01-01T00:00:00Z"}}"#,
        ),
        (
            "JobResume",
            r#"{"JobResume":{"job_id":7,"at":"2026-01-01T00:00:00Z"}}"#,
        ),
        ("JobEvict", r#"{"JobEvict":{"job_id":7}}"#),
        (
            "NodeRegister",
            r#"{"NodeRegister":{"name":"n0","resources":{"cpus":0,"memory_mb":0,"gpus":[{"device_id":0,"gpu_type":"mi300","memory_mb":0,"peer_gpus":[],"link_type":"PCIe"}],"generic":{}},"address":"1.2.3.4","port":6818,"wg_pubkey":"","version":"","labels":{}}}"#,
        ),
        (
            "NodeUpdate",
            r#"{"NodeUpdate":{"name":"n0","resources":{"cpus":0,"memory_mb":0,"gpus":[{"device_id":0,"gpu_type":"mi300","memory_mb":0,"peer_gpus":[],"link_type":"PCIe"}],"generic":{}},"address":"1.2.3.4","port":6818,"wg_pubkey":"","version":""}}"#,
        ),
        (
            "NodeStateChange",
            r#"{"NodeStateChange":{"name":"n0","old_state":"IDLE","new_state":"DOWN","reason":null,"admin_locked":false}}"#,
        ),
        (
            "NodeLabelsUpdate",
            r#"{"NodeLabelsUpdate":{"name":"n0","set":{},"remove":[]}}"#,
        ),
        (
            "NodeRemove",
            r#"{"NodeRemove":{"name":"n0","reason":null}}"#,
        ),
        (
            "TokenCreate",
            r#"{"TokenCreate":{"token":{"id":"tok","secret_hash":"hash","created_at":"2026-01-01T00:00:00Z","expires_at":null,"revoked":false}}}"#,
        ),
        ("TokenRevoke", r#"{"TokenRevoke":{"token_id":"tok"}}"#),
        (
            "ReservationCreate",
            r#"{"ReservationCreate":{"reservation":{"name":"r1","start_time":"2026-01-01T00:00:00Z","end_time":"2026-01-01T00:00:00Z","nodes":["n0"],"accounts":[],"users":["alice"],"flags":{"maint":false,"ignore_jobs":false,"no_hold_jobs":false,"overlap":false},"owner":""}}}"#,
        ),
        (
            "ReservationUpdate",
            r#"{"ReservationUpdate":{"name":"r1","duration_minutes":60,"add_nodes":[],"remove_nodes":[],"add_users":[],"remove_users":[],"add_accounts":[],"remove_accounts":[]}}"#,
        ),
        (
            "ReservationDelete",
            r#"{"ReservationDelete":{"name":"r1"}}"#,
        ),
        (
            "NodeK0sAssign",
            r#"{"NodeK0sAssign":{"name":"n0","role":"controller","mesh_ip":"10.0.0.1","pod_cidr":"10.1.0.0/24"}}"#,
        ),
        (
            "K0sSetPhase",
            r#"{"K0sSetPhase":{"phase":"ready","control_plane_node":null,"reset_requested":false}}"#,
        ),
    ];

    #[test]
    fn every_frozen_fixture_still_deserializes() {
        for (tag, json) in FIXTURES {
            let op: WalOperation = serde_json::from_str(json).unwrap_or_else(|e| {
                panic!("frozen {tag} WAL entry no longer deserializes ({e}); a new field needs #[serde(default)], or a variant/field was renamed or removed — none of which old Raft logs can survive")
            });
            assert_eq!(
                variant_tag(&op),
                *tag,
                "fixture keyed {tag} decoded to a different variant"
            );
        }
    }

    // Fails if a variant is missing from FIXTURES. Combined with the exhaustive
    // `variant_tag` match (which breaks the build on a new variant), this makes a
    // new WalOperation variant impossible to ship without a frozen fixture.
    #[test]
    fn every_variant_has_a_fixture() {
        let covered: std::collections::HashSet<&str> =
            FIXTURES.iter().map(|(tag, _)| *tag).collect();
        assert_eq!(covered.len(), FIXTURES.len(), "duplicate tag in FIXTURES");
        for op in all_variants() {
            let tag = variant_tag(&op);
            assert!(
                covered.contains(tag),
                "WalOperation::{tag} has no frozen fixture in FIXTURES"
            );
        }
    }

    // One constructed instance per variant, used only to enumerate the variant set
    // at runtime. Add a variant here when you add one to `WalOperation`.
    fn all_variants() -> Vec<WalOperation> {
        use crate::job::JobSpec;
        use crate::reservation::Reservation;
        use crate::resource::{
            AllocatedDevice, GpuLinkType, GpuResource, ResourceAllocations, ResourceSet,
        };
        use crate::step::{JobStep, StepState};

        let dt = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        // Non-empty so serde actually descends into the element structs — an empty
        // Vec/Map would leave GpuResource/AllocatedDevice untraversed and unguarded.
        let resource_set = ResourceSet {
            cpus: 0,
            memory_mb: 0,
            gpus: vec![GpuResource {
                device_id: 0,
                gpu_type: "mi300".into(),
                memory_mb: 0,
                peer_gpus: vec![],
                link_type: GpuLinkType::PCIe,
            }],
            generic: HashMap::new(),
        };
        let alloc = ResourceAllocations {
            cpus: 0,
            memory_mb: 0,
            devices: HashMap::from([("gpu".to_string(), vec![AllocatedDevice::injectable(0)])]),
        };
        vec![
            WalOperation::JobSubmit {
                job_id: 7,
                spec: Box::new(JobSpec::default()),
            },
            WalOperation::job_state_change(7, JobState::Pending, JobState::Running),
            WalOperation::job_start(7, vec!["n0".into()], alloc.clone(), HashMap::new()),
            WalOperation::JobComplete {
                job_id: 7,
                exit_code: 0,
                state: JobState::Completed,
            },
            WalOperation::JobNodeComplete {
                job_id: 7,
                node_name: "n0".into(),
                exit_code: 0,
                signal: 0,
            },
            WalOperation::JobStepComplete {
                job_id: 7,
                step_id: 0,
                exit_code: 0,
            },
            WalOperation::JobStepCreate {
                step: Box::new(JobStep {
                    job_id: 7,
                    step_id: 0,
                    name: "s".into(),
                    state: StepState::Pending,
                    num_tasks: 1,
                    cpus_per_task: 1,
                    resources: alloc.clone(),
                    nodes: vec!["n0".into()],
                    distribution: Default::default(),
                    start_time: None,
                    end_time: None,
                    exit_code: None,
                }),
            },
            WalOperation::JobPriorityChange {
                job_id: 7,
                old_priority: 0,
                new_priority: 1,
                pending_reason: None,
                reset_requeue_count: false,
                clear_reservation: false,
            },
            WalOperation::JobPreemptRequeue {
                job_id: 7,
                begin_time: dt,
            },
            WalOperation::JobSuspend { job_id: 7, at: dt },
            WalOperation::JobResume { job_id: 7, at: dt },
            WalOperation::JobEvict { job_id: 7 },
            WalOperation::NodeRegister {
                name: "n0".into(),
                resources: resource_set.clone(),
                address: "1.2.3.4".into(),
                port: 6818,
                wg_pubkey: String::new(),
                version: String::new(),
                labels: HashMap::new(),
            },
            WalOperation::NodeUpdate {
                name: "n0".into(),
                resources: resource_set.clone(),
                address: "1.2.3.4".into(),
                port: 6818,
                wg_pubkey: String::new(),
                version: String::new(),
            },
            WalOperation::NodeStateChange {
                name: "n0".into(),
                old_state: NodeState::Idle,
                new_state: NodeState::Down,
                reason: None,
                admin_locked: false,
            },
            WalOperation::NodeLabelsUpdate {
                name: "n0".into(),
                set: HashMap::new(),
                remove: vec![],
            },
            WalOperation::NodeRemove {
                name: "n0".into(),
                reason: None,
            },
            WalOperation::TokenCreate {
                token: AdmissionToken {
                    id: "tok".into(),
                    secret_hash: "hash".into(),
                    created_at: dt,
                    expires_at: None,
                    revoked: false,
                },
            },
            WalOperation::TokenRevoke {
                token_id: "tok".into(),
            },
            WalOperation::ReservationCreate {
                reservation: Reservation {
                    name: "r1".into(),
                    start_time: dt,
                    end_time: dt,
                    nodes: vec!["n0".into()],
                    accounts: vec![],
                    users: vec!["alice".into()],
                    flags: Default::default(),
                    owner: String::new(),
                },
            },
            WalOperation::ReservationUpdate {
                name: "r1".into(),
                duration_minutes: 60,
                add_nodes: vec![],
                remove_nodes: vec![],
                add_users: vec![],
                remove_users: vec![],
                add_accounts: vec![],
                remove_accounts: vec![],
            },
            WalOperation::ReservationDelete { name: "r1".into() },
            WalOperation::NodeK0sAssign {
                name: "n0".into(),
                role: K0sRole::Controller,
                mesh_ip: "10.0.0.1".into(),
                pod_cidr: "10.1.0.0/24".into(),
            },
            WalOperation::K0sSetPhase {
                phase: K0sPhase::Ready,
                control_plane_node: None,
                reset_requested: false,
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let op = WalOperation::K0sSetPhase {
            phase: K0sPhase::Ready,
            control_plane_node: Some("head-node".into()),
            reset_requested: false,
        };
        let back: WalOperation =
            serde_json::from_str(&serde_json::to_string(&op).unwrap()).unwrap();
        match back {
            WalOperation::K0sSetPhase {
                phase,
                control_plane_node,
                reset_requested,
            } => {
                assert_eq!(phase, K0sPhase::Ready);
                assert_eq!(control_plane_node.as_deref(), Some("head-node"));
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
}

#[cfg(test)]
mod evict_wal_tests {
    use super::*;
    use crate::step::{JobStep, StepState, TaskDistribution};

    #[test]
    fn job_evict_op_round_trips() {
        let op = WalOperation::JobEvict { job_id: 9 };
        let json = serde_json::to_string(&op).unwrap();
        let back: WalOperation = serde_json::from_str(&json).unwrap();
        match back {
            WalOperation::JobEvict { job_id } => assert_eq!(job_id, 9),
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
