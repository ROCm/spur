// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

/// One rebuild trigger's accumulated drift.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RebuildSnapshot {
    pub trigger: String,
    pub rebuilds: u64,
    /// Nodes short of what their job records prove is in use. Corrected.
    pub nodes_undercharged: u64,
    /// Nodes charged beyond their job records. Reported only — a release still
    /// in flight is indistinguishable from a leak, so neither is acted on here.
    pub nodes_overcharged: u64,
    /// Nodes undercharged on the most recent rebuild for this trigger.
    pub last_nodes_undercharged: u64,
    pub cpus_undercharged: u64,
    pub cpus_overcharged: u64,
    pub memory_undercharged_mb: u64,
    pub memory_overcharged_mb: u64,
    pub devices_undercharged: u64,
    pub devices_overcharged: u64,
    /// Active job/node pairs naming an untracked node, or carrying no allocation
    /// for it. Nothing is charged and the diff cannot see the shortfall.
    pub unaccounted_slices: u64,
    /// Nodes examined on the most recent pass, as a denominator for the rest.
    pub nodes_checked: u64,
}

/// One reclaim cause's fire count.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReclaimCauseSnapshot {
    pub cause: String,
    pub count: u64,
}

/// Snapshot of allocation-reconciliation statistics since process start.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileStatsSnapshot {
    pub rebuilds: Vec<RebuildSnapshot>,
    pub reclaims: Vec<ReclaimCauseSnapshot>,
    pub heartbeats: u64,
    /// Heartbeats reporting no held jobs. The agent returns an empty list both
    /// when it holds nothing and when its job map was locked, so this is an
    /// upper bound on "node is idle", not a measurement of it.
    pub heartbeats_empty: u64,
}
