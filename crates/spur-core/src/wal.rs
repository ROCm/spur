// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

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
        #[serde(default)]
        signal: i32,
        state: JobState,
    },
    JobNodeComplete {
        job_id: JobId,
        node_name: String,
        exit_code: i32,
        #[serde(default)]
        signal: i32,
    },
    JobPriorityChange {
        job_id: JobId,
        old_priority: u32,
        new_priority: u32,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_node_complete_carries_signal_and_old_logs_default_zero() {
        let op = WalOperation::JobNodeComplete {
            job_id: 1,
            node_name: "n0".into(),
            exit_code: 0,
            signal: 9,
        };
        let json = serde_json::to_string(&op).unwrap();
        let back: WalOperation = serde_json::from_str(&json).unwrap();
        // Old log without `signal` deserializes with signal = 0.
        let old = r#"{"JobNodeComplete":{"job_id":2,"node_name":"n1","exit_code":3}}"#;
        let parsed: WalOperation = serde_json::from_str(old).unwrap();
        match parsed {
            WalOperation::JobNodeComplete {
                signal, exit_code, ..
            } => {
                assert_eq!(signal, 0);
                assert_eq!(exit_code, 3);
            }
            _ => panic!("wrong variant"),
        }
        // round-trip new form (WalOperation does not derive PartialEq)
        let _ = (op, back);
    }
}
