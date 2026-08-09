// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

use crate::job::JobId;
use crate::resource::ResourceAllocations;

/// Stable identity for one job run's allocation on one node.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AllocationKey {
    pub job_id: JobId,
    pub run_attempt: u32,
    pub node: String,
}

impl AllocationKey {
    pub fn new(job_id: JobId, run_attempt: u32, node: impl Into<String>) -> Self {
        Self {
            job_id,
            run_attempt,
            node: node.into(),
        }
    }
}

/// Controller lifecycle for an exact per-node allocation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AllocationPhase {
    #[default]
    Prepared,
    Launching,
    Active,
    Releasing,
    Released,
}

impl AllocationPhase {
    pub fn is_charged(self) -> bool {
        self != Self::Released
    }
}

/// Durable controller truth for an exact per-node resource owner.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AllocationRecord {
    pub key: AllocationKey,
    pub resources: ResourceAllocations,
    pub phase: AllocationPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_boot_id: Option<String>,
    #[serde(default)]
    pub last_command_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_started_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentCommandKind {
    #[default]
    Launch,
    Register,
    Release,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentCommandState {
    #[default]
    Pending,
    Succeeded,
    Failed,
}

/// Raft-committed mutation delivered by an agent-initiated poll.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCommandRecord {
    pub command_id: u64,
    pub key: AllocationKey,
    pub kind: AgentCommandKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub payload: Vec<u8>,
    #[serde(default)]
    pub signal: i32,
    #[serde(default)]
    pub state: AgentCommandState,
    #[serde(default)]
    pub agent_session_id: String,
    #[serde(default)]
    pub node_boot_id: String,
    #[serde(default)]
    pub response_payload: Vec<u8>,
    #[serde(default)]
    pub error: String,
}

impl AgentCommandRecord {
    pub fn new(
        command_id: u64,
        key: AllocationKey,
        kind: AgentCommandKind,
        payload: Vec<u8>,
        signal: i32,
    ) -> Self {
        Self {
            command_id,
            key,
            kind,
            created_at: None,
            payload,
            signal,
            state: AgentCommandState::Pending,
            agent_session_id: String::new(),
            node_boot_id: String::new(),
            response_payload: Vec::new(),
            error: String::new(),
        }
    }
}

impl AllocationRecord {
    pub fn new(key: AllocationKey, resources: ResourceAllocations, phase: AllocationPhase) -> Self {
        Self {
            key,
            resources,
            phase,
            agent_session_id: None,
            node_boot_id: None,
            last_command_id: 0,
            launch_started_at: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_record_without_session_or_command_fields_deserializes() {
        let frozen = r#"{
            "key":{"job_id":7,"run_attempt":2,"node":"n1"},
            "resources":{"cpus":4,"memory_mb":8192,"devices":{}},
            "phase":"Active"
        }"#;

        let record: AllocationRecord = serde_json::from_str(frozen).unwrap();
        assert_eq!(record.key, AllocationKey::new(7, 2, "n1"));
        assert_eq!(record.phase, AllocationPhase::Active);
        assert_eq!(record.agent_session_id, None);
        assert_eq!(record.node_boot_id, None);
        assert_eq!(record.last_command_id, 0);
        assert_eq!(record.launch_started_at, None);
    }

    #[test]
    fn old_command_without_result_fields_deserializes() {
        let frozen = r#"{
            "command_id":11,
            "key":{"job_id":7,"run_attempt":2,"node":"n1"},
            "kind":"Release"
        }"#;

        let command: AgentCommandRecord = serde_json::from_str(frozen).unwrap();
        assert_eq!(command.command_id, 11);
        assert_eq!(command.kind, AgentCommandKind::Release);
        assert_eq!(command.state, AgentCommandState::Pending);
        assert_eq!(command.created_at, None);
        assert!(command.payload.is_empty());
        assert!(command.response_payload.is_empty());
    }
}
