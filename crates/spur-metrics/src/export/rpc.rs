// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! RPC handler gauge registration for `/metrics/rpc`.

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use std::sync::atomic::AtomicU64;

use crate::export::encode_registered;
use crate::rpc::RpcStatsSnapshot;

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct OperationLabel {
    operation: String,
}

fn set_family_gauge(
    family: &Family<OperationLabel, Gauge<u64, AtomicU64>>,
    operation: &str,
    value: u64,
) {
    family
        .get_or_create(&OperationLabel {
            operation: operation.to_string(),
        })
        .set(value);
}

/// Register RPC handler gauges into `registry` from `snap`.
pub fn register_rpc(registry: &mut Registry, snap: &RpcStatsSnapshot) {
    let count_family = Family::<OperationLabel, Gauge<u64, AtomicU64>>::default();
    let avg_family = Family::<OperationLabel, Gauge<u64, AtomicU64>>::default();
    let total_family = Family::<OperationLabel, Gauge<u64, AtomicU64>>::default();

    for op in &snap.by_operation {
        set_family_gauge(&count_family, &op.operation, op.count);
        set_family_gauge(&avg_family, &op.operation, op.avg_time_us());
        set_family_gauge(&total_family, &op.operation, op.total_time_us);
    }

    registry.register(
        "spur_rpc_stats",
        "RPC call count by operation",
        count_family,
    );
    registry.register(
        "spur_rpc_stats_avg_time",
        "Average RPC handler time in microseconds by operation",
        avg_family,
    );
    registry.register(
        "spur_rpc_stats_total_time",
        "Cumulative RPC handler time in microseconds by operation",
        total_family,
    );
}

/// Encode RPC metrics for `/metrics/rpc` as OpenMetrics 1.0 text.
pub fn encode_rpc_metrics(snap: &RpcStatsSnapshot) -> String {
    encode_registered(|registry| register_rpc(registry, snap))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::RpcOperationSnapshot;

    fn sample_snapshot() -> RpcStatsSnapshot {
        RpcStatsSnapshot {
            by_operation: vec![
                RpcOperationSnapshot {
                    operation: "GetJobs".into(),
                    count: 10,
                    total_time_us: 500,
                },
                RpcOperationSnapshot {
                    operation: "SubmitJob".into(),
                    count: 2,
                    total_time_us: 3000,
                },
            ],
        }
    }

    #[test]
    fn export_includes_labeled_operation_metrics() {
        let body = encode_rpc_metrics(&sample_snapshot());
        assert!(body.contains("spur_rpc_stats{operation=\"SubmitJob\"} 2"));
        assert!(body.contains("spur_rpc_stats_avg_time{operation=\"SubmitJob\"} 1500"));
        assert!(body.contains("spur_rpc_stats_total_time{operation=\"SubmitJob\"} 3000"));
        assert!(body.ends_with("# EOF\n"));
    }

    #[test]
    fn empty_snapshot_produces_no_operation_labels() {
        let body = encode_rpc_metrics(&RpcStatsSnapshot::default());
        assert!(!body.contains("operation="));
        assert!(body.ends_with("# EOF\n"));
    }
}
