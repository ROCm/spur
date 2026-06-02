// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Job gauge registration for `/metrics/jobs`.

use prometheus_client::registry::Registry;
use spur_core::job::JobState;

use crate::export::encode_registered;
use crate::export::register_gauge;
use crate::job::JobMetricsSnapshot;
use spur_core::config::MetricsExpositionFormat;

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

/// Register job catalog gauges into `registry` from `snap`.
pub fn register_jobs(registry: &mut Registry, snap: &JobMetricsSnapshot) {
    register_gauge(registry, "spur_jobs", "Total number of jobs", snap.total);

    for &state in &JobState::ALL {
        let suffix = job_state_metric_suffix(state);
        let name = format!("spur_jobs_{suffix}");
        let help = if state == JobState::Pending {
            "Number of jobs in Pending state (includes held jobs)".to_string()
        } else {
            format!("Number of jobs in {} state", state.display())
        };
        register_gauge(registry, &name, &help, snap.count_state(state));
    }

    register_gauge(
        registry,
        "spur_jobs_cpus_alloc",
        "Total CPUs allocated to jobs in Running or Completing state",
        snap.running_cpus,
    );
    register_gauge(
        registry,
        "spur_jobs_memory_alloc_bytes",
        "Total memory in bytes allocated to jobs in Running or Completing state",
        snap.running_memory_bytes,
    );
    register_gauge(
        registry,
        "spur_jobs_gpus_alloc",
        "Total GPUs allocated to jobs in Running or Completing state",
        snap.running_gpus,
    );
}

/// Encode job metrics for `/metrics/jobs` (default: Slurm 0.0.4 text).
pub fn encode_job_metrics(snap: &JobMetricsSnapshot) -> String {
    encode_job_metrics_with_format(snap, MetricsExpositionFormat::default())
}

/// Encode job metrics using the selected wire format.
pub fn encode_job_metrics_with_format(
    snap: &JobMetricsSnapshot,
    format: MetricsExpositionFormat,
) -> String {
    encode_registered(|registry| register_jobs(registry, snap), format)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::JobMetricsSnapshot;
    use spur_core::job::{Job, JobSpec, JobState, PendingReason};
    use spur_core::resource::{AllocatedDevice, ResourceAllocations};

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
                let mut alloc = ResourceAllocations::with_scalar(4, 8192);
                alloc.devices.insert(
                    "gpu".into(),
                    vec![
                        AllocatedDevice::injectable(0),
                        AllocatedDevice::injectable(1),
                    ],
                );
                j.allocated_resources = Some(alloc);
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
        assert!(body.contains("# HELP spur_jobs "));
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
    fn openmetrics_format_includes_eof() {
        let body = encode_job_metrics_with_format(
            &sample_snapshot(),
            MetricsExpositionFormat::OpenMetrics_1_0,
        );
        assert!(body.ends_with("# EOF\n"));
    }

    #[test]
    fn slurm_format_has_no_eof() {
        let body = encode_job_metrics_with_format(
            &sample_snapshot(),
            MetricsExpositionFormat::Slurm_0_0_4,
        );
        assert!(!body.contains("# EOF"));
    }

    /// Slurm job metric names exported for every [`JobState`].
    const JOB_STATE_METRICS: &[&str] = &[
        "spur_jobs_pending",
        "spur_jobs_running",
        "spur_jobs_completing",
        "spur_jobs_completed",
        "spur_jobs_failed",
        "spur_jobs_cancelled",
        "spur_jobs_timeout",
        "spur_jobs_node_fail",
        "spur_jobs_preempted",
        "spur_jobs_suspended",
    ];

    const ALLOC_METRICS: &[&str] = &[
        "spur_jobs_cpus_alloc",
        "spur_jobs_memory_alloc_bytes",
        "spur_jobs_gpus_alloc",
    ];

    #[test]
    fn job_metrics_catalog_includes_all_state_gauges() {
        let body = encode_job_metrics(&sample_snapshot());
        assert!(body.contains("# TYPE spur_jobs gauge"));
        for name in JOB_STATE_METRICS {
            assert!(
                body.contains(&format!("# TYPE {name} gauge")),
                "missing TYPE for {name}"
            );
            assert!(
                body.contains(&format!("{name} ")),
                "missing sample line for {name}"
            );
        }
        for name in ALLOC_METRICS {
            assert!(
                body.contains(&format!("# TYPE {name} gauge")),
                "missing TYPE for {name}"
            );
        }
        for &state in &JobState::ALL {
            let suffix = job_state_metric_suffix(state);
            assert!(body.contains(&format!("spur_jobs_{suffix} ")));
        }
    }
}
