use serde::{Deserialize, Serialize};

use crate::job::{JobId, JobSpec, JobState};
use crate::node::NodeState;
use crate::resource::ResourceSet;

/// All state-mutating operations that get logged to the Raft log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WalOperation {
    // Job operations
    JobSubmit {
        job_id: JobId,
        spec: JobSpec,
    },
    JobStateChange {
        job_id: JobId,
        old_state: JobState,
        new_state: JobState,
    },
    JobStart {
        job_id: JobId,
        nodes: Vec<String>,
        resources: ResourceSet,
    },
    JobComplete {
        job_id: JobId,
        exit_code: i32,
        state: JobState,
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
    },
    NodeStateChange {
        name: String,
        old_state: NodeState,
        new_state: NodeState,
        reason: Option<String>,
    },
    NodeHeartbeat {
        name: String,
        cpu_load: u32,
        free_memory_mb: u64,
    },
}
