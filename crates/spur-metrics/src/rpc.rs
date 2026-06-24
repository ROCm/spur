// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

/// Per-operation RPC handler statistics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcOperationSnapshot {
    pub operation: String,
    pub count: u64,
    pub total_time_us: u64,
}

impl RpcOperationSnapshot {
    pub fn avg_time_us(&self) -> u64 {
        self.total_time_us.checked_div(self.count).unwrap_or(0)
    }
}

/// Snapshot of controller RPC handler statistics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RpcStatsSnapshot {
    pub by_operation: Vec<RpcOperationSnapshot>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avg_time_us_is_zero_when_count_zero() {
        let op = RpcOperationSnapshot {
            operation: "Ping".into(),
            count: 0,
            total_time_us: 100,
        };
        assert_eq!(op.avg_time_us(), 0);
    }

    #[test]
    fn avg_time_us_divides_total_by_count() {
        let op = RpcOperationSnapshot {
            operation: "SubmitJob".into(),
            count: 4,
            total_time_us: 1000,
        };
        assert_eq!(op.avg_time_us(), 250);
    }
}
