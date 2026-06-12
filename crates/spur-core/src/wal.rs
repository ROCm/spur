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
        state: JobState,
    },
    JobNodeComplete {
        job_id: JobId,
        node_name: String,
        exit_code: i32,
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
}
