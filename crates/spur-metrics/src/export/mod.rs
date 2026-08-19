// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! prometheus-client registry encoding for spurctld metrics HTTP export.

use prometheus_client::encoding::text::encode as encode_openmetrics;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use std::sync::atomic::AtomicU64;

/// HTTP `Content-Type` for OpenMetrics 1.0 text responses.
pub const CONTENT_TYPE: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

pub mod jobs;
pub mod jobs_users_accts;
pub mod k8s_cluster;
pub mod nodes;
pub mod partitions;
pub mod rpc;
pub mod scheduler;

/// Register a scalar `u64` gauge with an initial value.
pub(crate) fn register_gauge(registry: &mut Registry, name: &str, help: &str, value: u64) {
    let gauge = Gauge::<u64, AtomicU64>::default();
    gauge.set(value);
    registry.register(name, help, gauge);
}

/// Register a scalar `u64` counter seeded from a snapshot value.
pub(crate) fn register_counter(registry: &mut Registry, name: &str, help: &str, value: u64) {
    let counter = Counter::<u64, AtomicU64>::default();
    if value > 0 {
        counter.inc_by(value);
    }
    registry.register(name, help, counter);
}

/// Build a registry, run `register`, and encode as OpenMetrics 1.0 text.
pub fn encode_registered<F>(register: F) -> String
where
    F: FnOnce(&mut Registry),
{
    let mut registry = Registry::default();
    register(&mut registry);
    let mut body = String::new();
    encode_openmetrics(&mut body, &registry).expect("in-memory encode");
    body
}

/// Merge independently-encoded OpenMetrics 1.0 bodies into one valid exposition. Each input ends
/// in its own `# EOF` terminator, which per spec ends the exposition — concatenating raw bodies
/// would leave everything after the first `# EOF` unparsed by a strict scraper. Strips every
/// trailing marker and appends exactly one.
pub fn concat_encoded(bodies: impl IntoIterator<Item = String>) -> String {
    let mut out = String::new();
    for body in bodies {
        out.push_str(body.strip_suffix("# EOF\n").unwrap_or(&body));
    }
    out.push_str("# EOF\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concat_encoded_keeps_exactly_one_eof_marker() {
        let a = "metric_a 1\n# EOF\n".to_string();
        let b = "metric_b 2\n# EOF\n".to_string();

        let merged = concat_encoded([a, b]);

        assert_eq!(merged, "metric_a 1\nmetric_b 2\n# EOF\n");
        assert_eq!(merged.matches("# EOF").count(), 1);
    }

    #[test]
    fn concat_encoded_of_one_body_is_unchanged() {
        let body = "metric_a 1\n# EOF\n".to_string();
        assert_eq!(concat_encoded([body.clone()]), body);
    }
}
