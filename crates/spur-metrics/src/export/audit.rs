// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Audit-log write counters for `/metrics/audit`.

use crate::export::{encode_registered, register_counter};
use spur_core::audit_metrics::AuditMetricsSnapshot;

/// Encode audit write counters as OpenMetrics 1.0 text.
pub fn encode_audit_metrics(snap: &AuditMetricsSnapshot) -> String {
    encode_registered(|registry| {
        register_counter(
            registry,
            "spur_audit_rows_written",
            "Audit (txn) rows persisted",
            snap.rows_written,
        );
        register_counter(
            registry,
            "spur_audit_rows_dropped",
            "Audit (txn) rows abandoned after retries, so an action went unrecorded",
            snap.rows_dropped,
        );
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_contains_counter_names() {
        let text = encode_audit_metrics(&AuditMetricsSnapshot {
            rows_written: 7,
            rows_dropped: 2,
        });
        assert!(text.contains("spur_audit_rows_written"));
        assert!(text.contains("spur_audit_rows_dropped"));
        // The dropped count is the alertable one, so it must carry its value.
        assert!(text.contains("spur_audit_rows_dropped_total 2"));
    }
}
