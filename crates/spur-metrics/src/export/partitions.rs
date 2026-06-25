// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Partition gauge registration for `/metrics/partitions` (Layer 1d).

use prometheus_client::registry::Registry;

use crate::export::encode_registered;

/// Register partition catalog gauges (stub until `PartitionMetricsSnapshot` exists).
pub fn register_partitions(_registry: &mut Registry) {}

/// Encode partition metrics for `/metrics/partitions` as OpenMetrics 1.0 text.
pub fn encode_partitions_metrics() -> String {
    encode_registered(register_partitions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_partitions_export_has_eof_only() {
        let body = encode_partitions_metrics();
        assert_eq!(body, "# EOF\n");
    }
}
