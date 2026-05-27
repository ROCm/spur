// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! prometheus-client registry encoding for spurctld metrics HTTP export.

use prometheus_client::encoding::text::{
    encode as encode_openmetrics_strict, encode_eof, encode_registry,
};
use prometheus_client::registry::Registry;
use spur_core::config::MetricsExpositionFormat;

pub mod jobs;
pub mod nodes;
pub mod partitions;
pub mod scheduler;

/// Build a registry, run `register`, and encode with `format`.
pub fn encode_registered<F>(register: F, format: MetricsExpositionFormat) -> String
where
    F: FnOnce(&mut Registry),
{
    let mut registry = Registry::default();
    register(&mut registry);
    encode_registry_body(&registry, format)
}

/// Encode all metrics in `registry` using `format`.
pub fn encode_registry_body(registry: &Registry, format: MetricsExpositionFormat) -> String {
    let mut body = String::new();
    match format {
        MetricsExpositionFormat::Slurm_0_0_4 => {
            encode_registry(&mut body, registry).expect("in-memory encode_registry");
        }
        MetricsExpositionFormat::OpenMetrics_1_0 => {
            encode_openmetrics_strict(&mut body, registry).expect("in-memory encode");
        }
    }
    body
}

/// Append the OpenMetrics `# EOF` trailer (for strict 1.0 responses only).
pub fn append_eof(body: &mut String) {
    encode_eof(body).expect("in-memory encode_eof");
}
