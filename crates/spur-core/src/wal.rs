// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

use crate::admission::AdmissionToken;
use crate::job::{JobId, JobSpec, JobState};
use crate::node::NodeState;
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
    },
    JobStart {
        job_id: JobId,
        nodes: Vec<String>,
        resources: ResourceAllocations,
        /// Per-node allocation slices (device IDs are node-local).
        #[serde(default)]
        per_node_alloc: HashMap<String, ResourceAllocations>,
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
    JobPriorityChange {
        job_id: JobId,
        old_priority: u32,
        new_priority: u32,
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
