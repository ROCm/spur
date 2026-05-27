// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Partition gauge registration for `/metrics/partitions` (Layer 1d).

use prometheus_client::registry::Registry;
use spur_core::config::MetricsExpositionFormat;

use crate::export::encode_registered;

/// Register partition catalog gauges (stub until `PartitionMetricsSnapshot` exists).
pub fn register_partitions(_registry: &mut Registry) {}

/// Encode partition metrics for `/metrics/partitions`.
pub fn encode_partitions_metrics(format: MetricsExpositionFormat) -> String {
    encode_registered(register_partitions, format)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_partitions_export_openmetrics_has_eof_only() {
        let body = encode_partitions_metrics(MetricsExpositionFormat::OpenMetrics_1_0);
        assert_eq!(body, "# EOF\n");
    }
}
