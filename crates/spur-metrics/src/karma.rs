// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Per-user karma counter snapshot, produced by `KarmaStatsCollector::snapshot()`.

/// Counters for a single user.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KarmaUserSnapshot {
    pub submitted: u64,
    pub completed: u64,
    pub failed: u64,
    pub timeout: u64,
    pub node_fail: u64,
    pub cancelled: u64,
    pub gpus_requested: u64,
    pub overflow_borrows: u64,
    pub walltime_requested_secs: u64,
    pub walltime_actual_secs: u64,
}

/// Per-user karma counters snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KarmaStatsSnapshot {
    pub users: Vec<(String, KarmaUserSnapshot)>,
}
