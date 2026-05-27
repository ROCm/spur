// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Cluster metrics aggregation and Prometheus/OpenMetrics export for spurctld.

pub mod export;
pub mod job;

pub use export::jobs::{
    encode_job_metrics, encode_job_metrics_with_format, job_state_metric_suffix,
};
pub use export::nodes::encode_nodes_metrics;
pub use export::partitions::encode_partitions_metrics;
pub use export::scheduler::encode_scheduler_metrics;
pub use spur_core::config::MetricsExpositionFormat;
