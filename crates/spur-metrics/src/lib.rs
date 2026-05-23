// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Cluster metrics aggregation and OpenMetrics export for spurctld.

pub mod job;
pub mod job_export;
pub mod openmetrics;

pub use job_export::encode_job_metrics;
