// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

/// One node's reading of a running job's cgroup. Carried on the heartbeat and
/// held only in the leader's memory: it is telemetry, same as node load, and
/// is not written to the Raft log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobUsage {
    pub job_id: u32,
    pub cpu_usage_usec: u64,
    pub rss_bytes: u64,
    pub virtual_bytes: u64,
    pub swap_bytes: u64,
    pub disk_read_bytes: u64,
    pub disk_write_bytes: u64,
}
