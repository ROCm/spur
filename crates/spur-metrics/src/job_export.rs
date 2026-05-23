// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! OpenMetrics export for job metrics ([`crate::job::JobMetricsSnapshot`]).

use spur_core::job::JobState;

use crate::job::JobMetricsSnapshot;
use crate::openmetrics::OpenMetricsEncoder;

/// Metric name suffix for a [`JobState`] (e.g. `pending`, `node_fail`).
pub fn job_state_metric_suffix(state: JobState) -> &'static str {
    match state {
        JobState::Pending => "pending",
        JobState::Running => "running",
        JobState::Completing => "completing",
        JobState::Completed => "completed",
        JobState::Failed => "failed",
        JobState::Cancelled => "cancelled",
        JobState::Timeout => "timeout",
        JobState::NodeFail => "node_fail",
        JobState::Preempted => "preempted",
        JobState::Suspended => "suspended",
    }
}

/// Encode job metrics as OpenMetrics 1.0 text for `/metrics/jobs`.
pub fn encode_job_metrics(snap: &JobMetricsSnapshot) -> String {
    let mut enc = OpenMetricsEncoder::new();

    enc.write_gauge("spur_jobs", "Total number of jobs", snap.total);

    for &state in &JobState::ALL {
        let suffix = job_state_metric_suffix(state);
        let name = format!("spur_jobs_{suffix}");
        let help = if state == JobState::Pending {
            "Number of jobs in Pending state (includes held jobs)".to_string()
        } else {
            format!("Number of jobs in {} state", state.display())
        };
        let value = snap.count_state(state);
        enc.write_gauge(&name, &help, value);
    }

    enc.write_gauge(
        "spur_jobs_cpus_alloc",
        "Total CPUs allocated to jobs in Running or Completing state",
        snap.running_cpus,
    );
    enc.write_gauge(
        "spur_jobs_memory_alloc_bytes",
        "Total memory in bytes allocated to jobs in Running or Completing state",
        snap.running_memory_bytes,
    );
    enc.write_gauge(
        "spur_jobs_gpus_alloc",
        "Total GPUs allocated to jobs in Running or Completing state",
        snap.running_gpus,
    );

    enc.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::JobMetricsSnapshot;
    use spur_core::job::{Job, JobSpec, JobState, PendingReason};
    use spur_core::resource::{GpuLinkType, GpuResource, ResourceSet};

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
    fn encode_contains_core_gauges() {
        let body = encode_job_metrics(&sample_snapshot());
        assert!(body.contains("# HELP spur_jobs Total number of jobs\n"));
        assert!(body.contains("spur_jobs 4\n"));
        assert!(body.contains("spur_jobs_pending 2\n"));
        assert!(body.contains("spur_jobs_running 1\n"));
        assert!(body.contains("spur_jobs_completed 1\n"));
        assert!(body.contains("spur_jobs_cpus_alloc 4\n"));
        assert!(body.contains("spur_jobs_memory_alloc_bytes 8589934592\n"));
        assert!(body.contains("spur_jobs_gpus_alloc 2\n"));
        assert!(body.contains("Number of jobs in Pending state (includes held jobs)"));
    }

    #[test]
    fn encode_empty_snapshot() {
        let body = encode_job_metrics(&JobMetricsSnapshot::default());
        assert!(body.contains("spur_jobs 0\n"));
        assert!(body.contains("spur_jobs_running 0\n"));
    }

    #[test]
    fn golden_job_metrics() {
        let body = encode_job_metrics(&sample_snapshot());
        let expected = include_str!("../tests/fixtures/job_metrics.prom");
        assert_eq!(body, expected);
    }
}
