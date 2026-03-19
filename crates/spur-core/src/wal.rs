use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::job::{JobId, JobSpec, JobState};
use crate::node::NodeState;
use crate::resource::ResourceSet;

/// A WAL entry representing a state mutation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalEntry {
    pub sequence: u64,
    pub timestamp: DateTime<Utc>,
    pub operation: WalOperation,
}

/// All state-mutating operations that get logged to the WAL.
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

impl WalEntry {
    pub fn new(sequence: u64, operation: WalOperation) -> Self {
        Self {
            sequence,
            timestamp: Utc::now(),
            operation,
        }
    }
}

/// Trait for WAL storage backends.
pub trait WalStore: Send + Sync {
    /// Append an entry to the WAL.
    fn append(&mut self, entry: &WalEntry) -> Result<(), WalStoreError>;

    /// Read entries from a given sequence number.
    fn read_from(&self, sequence: u64) -> Result<Vec<WalEntry>, WalStoreError>;

    /// Get the latest sequence number.
    fn latest_sequence(&self) -> u64;

    /// Truncate entries before a given sequence (after snapshot).
    fn truncate_before(&mut self, sequence: u64) -> Result<(), WalStoreError>;
}

#[derive(Debug, thiserror::Error)]
pub enum WalStoreError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("WAL corrupted at sequence {0}")]
    Corrupted(u64),
}
