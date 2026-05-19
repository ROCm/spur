// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

/// Partition (queue) configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Partition {
    pub name: String,
    pub state: PartitionState,
    pub is_default: bool,

    /// Nodes belonging to this partition (hostlist pattern).
    pub nodes: String,

    /// Limits
    pub max_time_minutes: Option<u32>,
    pub default_time_minutes: Option<u32>,
    pub max_nodes: Option<u32>,
    pub min_nodes: u32,

    /// Access control
    pub allow_root: bool,
    pub exclusive_user: bool,
    pub allow_accounts: Vec<String>,
    pub allow_groups: Vec<String>,
    pub allow_qos: Vec<String>,
    pub deny_accounts: Vec<String>,
    pub deny_qos: Vec<String>,

    /// Scheduling
    pub preempt_mode: PreemptMode,
    pub priority_tier: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PartitionState {
    Up,
    Down,
    Drain,
    Inactive,
}

impl PartitionState {
    pub fn display(&self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::Drain => "drain",
            Self::Inactive => "inactive",
        }
    }
}

impl std::fmt::Display for PartitionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.display())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PreemptMode {
    #[default]
    Off,
    Cancel,
    Requeue,
    Suspend,
}

impl Default for Partition {
    fn default() -> Self {
        Self {
            name: String::new(),
            state: PartitionState::Up,
            is_default: false,
            nodes: String::new(),
            max_time_minutes: None,
            default_time_minutes: None,
            max_nodes: None,
            min_nodes: 1,
            allow_root: true,
            exclusive_user: false,
            allow_accounts: Vec::new(),
            allow_groups: Vec::new(),
            allow_qos: Vec::new(),
            deny_accounts: Vec::new(),
            deny_qos: Vec::new(),
            preempt_mode: PreemptMode::Off,
            priority_tier: 1,
        }
    }
}
