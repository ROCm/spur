// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Scheduler gauge registration for `/metrics/scheduler` (Layer 1c).

use prometheus_client::registry::Registry;

use crate::export::encode_registered;

/// Register scheduler catalog gauges (stub until scheduler snapshot exists).
pub fn register_scheduler(_registry: &mut Registry) {}

/// Encode scheduler metrics for `/metrics/scheduler` as OpenMetrics 1.0 text.
pub fn encode_scheduler_metrics() -> String {
    encode_registered(register_scheduler)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_scheduler_export_has_eof_only() {
        let body = encode_scheduler_metrics();
        assert_eq!(body, "# EOF\n");
    }
}
