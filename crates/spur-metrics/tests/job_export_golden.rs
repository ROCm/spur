// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Golden and catalog tests for job metrics export.

use spur_core::config::MetricsExpositionFormat;
use spur_core::job::{Job, JobSpec, JobState, PendingReason};
use spur_core::resource::{GpuLinkType, GpuResource, ResourceSet};
use spur_metrics::job::JobMetricsSnapshot;
use spur_metrics::{encode_job_metrics, encode_job_metrics_with_format, job_state_metric_suffix};
use std::path::PathBuf;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn sample_snapshot() -> JobMetricsSnapshot {
    let jobs = [
        {
            let mut j = Job::new(1, JobSpec::default());
            j.state = JobState::Pending;
            j.pending_reason = PendingReason::Held;
            j
        },
        {
            let mut j = Job::new(2, JobSpec::default());
            j.state = JobState::Pending;
            j
        },
        {
            let mut j = Job::new(3, JobSpec::default());
            j.state = JobState::Running;
            j.allocated_resources = Some(ResourceSet {
                cpus: 4,
                memory_mb: 8192,
                gpus: vec![
                    GpuResource {
                        device_id: 0,
                        gpu_type: "mi300x".into(),
                        memory_mb: 0,
                        peer_gpus: vec![],
                        link_type: GpuLinkType::XGMI,
                    },
                    GpuResource {
                        device_id: 1,
                        gpu_type: "mi300x".into(),
                        memory_mb: 0,
                        peer_gpus: vec![],
                        link_type: GpuLinkType::XGMI,
                    },
                ],
                generic: Default::default(),
            });
            j
        },
        {
            let mut j = Job::new(4, JobSpec::default());
            j.state = JobState::Completed;
            j
        },
    ];
    JobMetricsSnapshot::collect(jobs.iter())
}

#[test]
fn golden_job_metrics_slurm_0_0_4() {
    let body =
        encode_job_metrics_with_format(&sample_snapshot(), MetricsExpositionFormat::Slurm_0_0_4);
    let path = fixtures_dir().join("jobs.slurm_0_0_4.prom");
    let expected =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert_eq!(body, expected);
}

#[test]
fn golden_job_metrics_openmetrics_1_0() {
    let body = encode_job_metrics_with_format(
        &sample_snapshot(),
        MetricsExpositionFormat::OpenMetrics_1_0,
    );
    let path = fixtures_dir().join("jobs.openmetrics_1_0.prom");
    let expected =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert_eq!(body, expected);
}

#[test]
fn job_metrics_catalog_uses_spur_prefix_and_gauges() {
    let body = encode_job_metrics(&sample_snapshot());
    assert!(!body.contains("slurm_"));
    assert!(body.contains("spur_jobs "));
    assert!(body.contains("# TYPE spur_jobs gauge"));
    for &state in &JobState::ALL {
        let suffix = job_state_metric_suffix(state);
        assert!(body.contains(&format!("spur_jobs_{suffix} ")));
        assert!(body.contains(&format!("# TYPE spur_jobs_{suffix} gauge")));
    }
}

/// Regenerate `tests/fixtures/jobs.*.prom` after intentional encoder changes:
/// `cargo test -p spur-metrics --test job_export_golden refresh_golden_fixtures -- --ignored --exact`
#[test]
#[ignore = "manual fixture refresh"]
fn refresh_golden_fixtures() {
    let snap = sample_snapshot();
    let dir = fixtures_dir();
    std::fs::create_dir_all(&dir).expect("fixtures dir");
    std::fs::write(
        dir.join("jobs.slurm_0_0_4.prom"),
        encode_job_metrics_with_format(&snap, MetricsExpositionFormat::Slurm_0_0_4),
    )
    .expect("write slurm fixture");
    std::fs::write(
        dir.join("jobs.openmetrics_1_0.prom"),
        encode_job_metrics_with_format(&snap, MetricsExpositionFormat::OpenMetrics_1_0),
    )
    .expect("write openmetrics fixture");
}
