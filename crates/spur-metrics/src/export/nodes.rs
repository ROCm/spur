// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Node gauge registration for `/metrics/nodes` (Layer 1b).

use prometheus_client::registry::Registry;
use spur_core::config::MetricsExpositionFormat;

use crate::export::encode_registered;

/// Register node catalog gauges (stub until `NodeMetricsSnapshot` exists).
pub fn register_nodes(_registry: &mut Registry) {}

/// Encode node metrics for `/metrics/nodes`.
pub fn encode_nodes_metrics(format: MetricsExpositionFormat) -> String {
    encode_registered(register_nodes, format)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_nodes_export_slurm_has_no_samples() {
        let body = encode_nodes_metrics(MetricsExpositionFormat::Slurm_0_0_4);
        assert!(!body.contains("spur_"));
        assert!(!body.contains("# EOF"));
    }

    #[test]
    fn empty_nodes_export_openmetrics_has_eof_only() {
        let body = encode_nodes_metrics(MetricsExpositionFormat::OpenMetrics_1_0);
        assert_eq!(body, "# EOF\n");
    }
}
