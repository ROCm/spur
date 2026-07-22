// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Partition (queue) configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Partition {
    pub name: String,
    pub state: PartitionState,
    pub is_default: bool,

    /// Nodes belonging to this partition (hostlist pattern).
    pub nodes: String,

    /// Label selector: node joins this partition if ALL key-value pairs match.
    #[serde(default)]
    pub selector: HashMap<String, String>,

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

impl PreemptMode {
    /// Ranking used to pick a single mode when a job spans multiple partitions
    /// (comma-separated OR list). Higher = more disruptive to the preempted
    /// job. A job occupying a node in a partition that permits a harder
    /// preemption can be preempted that hard, so the most aggressive matched
    /// mode wins — maximizing the scheduler's ability to free resources.
    pub fn aggressiveness(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Suspend => 1,
            Self::Requeue => 2,
            Self::Cancel => 3,
        }
    }
}

/// Non-empty names from a comma-separated partition OR-list.
pub fn requested_partition_names(spec: Option<&str>) -> impl Iterator<Item = &str> {
    spec.into_iter()
        .flat_map(|spec| spec.split(','))
        .map(str::trim)
        .filter(|name| !name.is_empty())
}

/// Partitions a job requests, resolved by name.
pub fn matched_partitions<'a>(
    spec: Option<&str>,
    partitions: &'a [Partition],
) -> Vec<&'a Partition> {
    requested_partition_names(spec)
        .filter_map(|req| partitions.iter().find(|p| p.name == req))
        .collect()
}

/// Highest `priority_tier` among a job's requested partitions (see
/// `matched_partitions`), or 1 if none match.
pub fn max_priority_tier(spec: Option<&str>, partitions: &[Partition]) -> u32 {
    matched_partitions(spec, partitions)
        .into_iter()
        .map(|p| p.priority_tier)
        .max()
        .unwrap_or(1)
}

impl Default for Partition {
    fn default() -> Self {
        Self {
            name: String::new(),
            state: PartitionState::Up,
            is_default: false,
            nodes: String::new(),
            selector: HashMap::new(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preempt_mode_aggressiveness_orders_cancel_over_requeue_over_suspend_over_off() {
        assert!(PreemptMode::Cancel.aggressiveness() > PreemptMode::Requeue.aggressiveness());
        assert!(PreemptMode::Requeue.aggressiveness() > PreemptMode::Suspend.aggressiveness());
        assert!(PreemptMode::Suspend.aggressiveness() > PreemptMode::Off.aggressiveness());
    }

    fn partition_with_tier(name: &str, priority_tier: u32) -> Partition {
        Partition {
            name: name.into(),
            priority_tier,
            ..Default::default()
        }
    }

    #[test]
    fn max_priority_tier_picks_highest_among_matched_partitions() {
        let parts = vec![
            partition_with_tier("low", 1),
            partition_with_tier("high", 9),
        ];
        assert_eq!(max_priority_tier(Some("low,high"), &parts), 9);
        assert_eq!(max_priority_tier(Some("high, low"), &parts), 9);
    }

    #[test]
    fn requested_partition_names_trims_and_ignores_empty_entries() {
        assert_eq!(
            requested_partition_names(Some(" gpu, ,cpu,, ")).collect::<Vec<_>>(),
            vec!["gpu", "cpu"]
        );
        assert!(requested_partition_names(None).next().is_none());
    }

    #[test]
    fn max_priority_tier_defaults_to_one_when_unset_or_unmatched() {
        let parts = vec![partition_with_tier("gpu", 5)];
        assert_eq!(max_priority_tier(None, &parts), 1);
        assert_eq!(max_priority_tier(Some("nope"), &parts), 1);
    }
}
