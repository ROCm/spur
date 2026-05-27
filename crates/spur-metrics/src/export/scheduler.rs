// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Scheduler gauge registration for `/metrics/scheduler` (Layer 1c).

use prometheus_client::registry::Registry;
use spur_core::config::MetricsExpositionFormat;

use crate::export::encode_registered;

/// Register scheduler catalog gauges (stub until scheduler snapshot exists).
pub fn register_scheduler(_registry: &mut Registry) {}

/// Encode scheduler metrics for `/metrics/scheduler`.
pub fn encode_scheduler_metrics(format: MetricsExpositionFormat) -> String {
    encode_registered(register_scheduler, format)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_scheduler_export_openmetrics_has_eof_only() {
        let body = encode_scheduler_metrics(MetricsExpositionFormat::OpenMetrics_1_0);
        assert_eq!(body, "# EOF\n");
    }
}
