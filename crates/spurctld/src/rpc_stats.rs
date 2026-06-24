// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! In-memory accumulator for controller RPC handler timings.

use std::collections::HashMap;
use std::time::Duration;

use parking_lot::Mutex;
use spur_metrics::{RpcOperationSnapshot, RpcStatsSnapshot};

#[derive(Debug, Default)]
struct OpAccum {
    count: u64,
    total_time_us: u64,
}

/// Leader-side RPC handler statistics since process start or the last reset.
#[derive(Debug, Default)]
pub struct RpcStatsCollector {
    ops: Mutex<HashMap<String, OpAccum>>,
}

impl RpcStatsCollector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, operation: &str, duration: Duration) {
        let micros = duration.as_micros().min(u64::MAX as u128) as u64;
        let mut ops = self.ops.lock();
        if let Some(accum) = ops.get_mut(operation) {
            accum.count += 1;
            accum.total_time_us = accum.total_time_us.saturating_add(micros);
        } else {
            ops.insert(
                operation.to_string(),
                OpAccum {
                    count: 1,
                    total_time_us: micros,
                },
            );
        }
    }

    pub fn snapshot(&self) -> RpcStatsSnapshot {
        let mut by_operation: Vec<RpcOperationSnapshot> = self
            .ops
            .lock()
            .iter()
            .map(|(operation, accum)| RpcOperationSnapshot {
                operation: operation.clone(),
                count: accum.count,
                total_time_us: accum.total_time_us,
            })
            .collect();
        by_operation.sort_by(|a, b| a.operation.cmp(&b.operation));
        RpcStatsSnapshot { by_operation }
    }

    pub fn reset(&self) {
        self.ops.lock().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn record_accumulates_count_and_time() {
        let stats = RpcStatsCollector::new();
        stats.record("SubmitJob", Duration::from_micros(100));
        stats.record("SubmitJob", Duration::from_micros(300));

        let snap = stats.snapshot();
        assert_eq!(snap.by_operation.len(), 1);
        let op = &snap.by_operation[0];
        assert_eq!(op.operation, "SubmitJob");
        assert_eq!(op.count, 2);
        assert_eq!(op.total_time_us, 400);
        assert_eq!(op.avg_time_us(), 200);
    }

    #[test]
    fn reset_clears_accumulators() {
        let stats = RpcStatsCollector::new();
        stats.record("Ping", Duration::from_micros(10));
        stats.reset();
        assert!(stats.snapshot().by_operation.is_empty());
    }
}
