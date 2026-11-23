// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Checkpoint-telemetry metric registration: the long-lived `spur_job_runs_ended` /
//! `spur_job_runs_restored` / `spur_job_run_end_seconds_since_last_checkpoint` /
//! `spur_job_run_checkpoints_completed` / `spur_preempt_grace_checkpoint` /
//! `spur_job_run_checkpoints_failed` / `spur_checkpoint_events_dropped` families (appended to
//! `/metrics/jobs`), and the per-job gauges plus the per-job `spur_job_checkpoints_failed`
//! counter on `/metrics/jobs-users-accts`.

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use std::sync::atomic::AtomicU64;

use crate::checkpoint::{CheckpointJobFailed, CheckpointJobSnapshot, CheckpointMetrics};
use crate::export::encode_registered;

/// Register the long-lived checkpoint counters/histograms into a fresh `registry` by cloning the
/// live `Family` handles out of `metrics` (cheap `Arc` clones). The families themselves are never
/// rebuilt -- only re-registered each scrape so the accumulator can be encoded alongside the rest
/// of `/metrics/jobs` via a single `Registry`.
pub fn register_checkpoint_metrics(registry: &mut Registry, metrics: &CheckpointMetrics) {
    registry.register(
        "spur_job_runs_ended",
        "Finalized job runs, labeled by terminal reason and checkpoint state",
        metrics.runs_ended.clone(),
    );
    registry.register(
        "spur_job_runs_restored",
        "Finalized job runs that reported a restore, labeled by terminal reason",
        metrics.runs_restored.clone(),
    );
    registry.register(
        "spur_job_run_end_seconds_since_last_checkpoint",
        "Seconds between a finalized run's last completed checkpoint and run end",
        metrics.seconds_since_last_checkpoint.clone(),
    );
    registry.register(
        "spur_job_run_checkpoints_completed",
        "Checkpoints completed over a finalized run's lifetime",
        metrics.checkpoints_completed.clone(),
    );
    registry.register(
        "spur_preempt_grace_checkpoint",
        "Preempt-grace checkpoint windows, labeled by outcome",
        metrics.preempt_grace.clone(),
    );
    registry.register(
        "spur_checkpoint_events_dropped",
        "Checkpoint telemetry events dropped, labeled by reason",
        metrics.events_dropped.clone(),
    );
    registry.register(
        "spur_job_run_checkpoints_failed",
        "Checkpoint events rejected as invalid, by cause",
        metrics.events_invalid.clone(),
    );
}

/// Encode the checkpoint accumulator as OpenMetrics 1.0 text.
pub fn encode_checkpoint_metrics(metrics: &CheckpointMetrics) -> String {
    encode_registered(|registry| register_checkpoint_metrics(registry, metrics))
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct JobLabel {
    job_id: u32,
    user: String,
    account: String,
    job_name: String,
}

fn job_label(job: &CheckpointJobSnapshot) -> JobLabel {
    JobLabel {
        job_id: job.job_id,
        user: job.user.clone(),
        account: job.account.clone(),
        job_name: sanitize_job_name(&job.job_name),
    }
}

/// Register per-job checkpoint gauges into `registry` from `jobs`. `last_step` and
/// `seconds_since_last` only get a sample when the snapshot carries one -- a job with no
/// completed checkpoint yet has no meaningful step or recency to report.
pub fn register_checkpoint_job_metrics(registry: &mut Registry, jobs: &[CheckpointJobSnapshot]) {
    let completed = Family::<JobLabel, Gauge<u64, AtomicU64>>::default();
    let last_step = Family::<JobLabel, Gauge<u64, AtomicU64>>::default();
    let seconds_since_last = Family::<JobLabel, Gauge<f64, AtomicU64>>::default();
    let in_progress = Family::<JobLabel, Gauge<u64, AtomicU64>>::default();
    let dropped = Family::<JobLabel, Gauge<u64, AtomicU64>>::default();

    for job in jobs {
        let label = job_label(job);
        completed.get_or_create(&label).set(job.completed);
        if let Some(step) = job.last_step {
            last_step.get_or_create(&label).set(step);
        }
        if let Some(seconds) = job.seconds_since_last {
            seconds_since_last.get_or_create(&label).set(seconds);
        }
        in_progress
            .get_or_create(&label)
            .set(u64::from(job.in_progress));
        dropped.get_or_create(&label).set(job.dropped);
    }

    registry.register(
        "spur_job_checkpoints_completed",
        "Checkpoints completed so far by a job's live (not yet finalized) run",
        completed,
    );
    registry.register(
        "spur_job_checkpoint_last_step",
        "Training step of a job's most recently completed checkpoint",
        last_step,
    );
    registry.register(
        "spur_job_checkpoint_seconds_since_last",
        "Seconds since a job's most recently completed checkpoint",
        seconds_since_last,
    );
    registry.register(
        "spur_job_checkpoint_in_progress",
        "Whether a job's live run has a checkpoint in progress past its last completed step",
        in_progress,
    );
    registry.register(
        "spur_job_checkpoint_events_dropped",
        "Checkpoint telemetry events dropped for a job's live run",
        dropped,
    );
}

/// Encode per-job checkpoint gauges as OpenMetrics 1.0 text.
pub fn encode_checkpoint_job_metrics(jobs: &[CheckpointJobSnapshot]) -> String {
    encode_registered(|registry| register_checkpoint_job_metrics(registry, jobs))
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct JobFailedLabel {
    job_id: u32,
    user: String,
    account: String,
    job_name: String,
    reason: String,
}

/// Longest `job_name` label value kept, so an unbounded user-supplied job name cannot grow this
/// series' label set without limit. Truncated on a char boundary, never mid-codepoint.
const JOB_NAME_LABEL_MAX_CHARS: usize = 64;

/// Replace any control character with `_` (OpenMetrics label values may contain them, but a raw
/// control character makes scraped output hard to read and easy to confuse with the exposition
/// format's own control sequences), then cap at `JOB_NAME_LABEL_MAX_CHARS` characters.
fn sanitize_job_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_control() { '_' } else { c })
        .take(JOB_NAME_LABEL_MAX_CHARS)
        .collect()
}

/// Register the per-job, per-reason invalid-event counter into `registry` from `jobs`. Only
/// `(job, reason)` pairs with a nonzero count ever appear -- no series is pre-seeded.
pub fn register_checkpoint_job_failed_metrics(
    registry: &mut Registry,
    jobs: &[CheckpointJobFailed],
) {
    let failed = Family::<JobFailedLabel, Counter<u64, AtomicU64>>::default();

    for job in jobs {
        let job_name = sanitize_job_name(&job.job_name);
        for (reason, count) in &job.reasons {
            let label = JobFailedLabel {
                job_id: job.job_id,
                user: job.user.clone(),
                account: job.account.clone(),
                job_name: job_name.clone(),
                reason: reason.clone(),
            };
            failed.get_or_create(&label).inc_by(*count);
        }
    }

    registry.register(
        "spur_job_checkpoints_failed",
        "Cumulative invalid checkpoint events for a job across every run attempt, by reason",
        failed,
    );
}

/// Encode the per-job invalid-event counter as OpenMetrics 1.0 text.
pub fn encode_checkpoint_job_failed_metrics(jobs: &[CheckpointJobFailed]) -> String {
    encode_registered(|registry| register_checkpoint_job_failed_metrics(registry, jobs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint::{CheckpointState, GraceOutcome, RunEnd};

    #[test]
    fn empty_metrics_is_eof_only() {
        let body = encode_checkpoint_metrics(&CheckpointMetrics::new());
        assert_eq!(body, "# EOF\n");
    }

    #[test]
    fn record_run_end_populates_all_families() {
        let metrics = CheckpointMetrics::new();
        metrics.record_run_end(&RunEnd {
            reason: "completed",
            checkpoint: CheckpointState::Completed,
            seconds_since_last_checkpoint: Some(120.0),
            checkpoints_completed: 5,
            grace: Some(GraceOutcome::Completed),
            restored: true,
        });
        metrics.add_dropped("busy", 3);
        metrics.add_invalid("malformed_json", 2);

        let body = encode_checkpoint_metrics(&metrics);
        assert!(body.contains(
            "spur_job_runs_ended_total{reason=\"completed\",checkpoint=\"completed\"} 1\n"
        ));
        assert!(body.contains("spur_job_runs_restored_total{reason=\"completed\"} 1\n"));
        assert!(body.contains(
            "spur_job_run_end_seconds_since_last_checkpoint_sum{reason=\"completed\"} 120"
        ));
        assert!(body.contains("spur_job_run_checkpoints_completed_sum{reason=\"completed\"} 5"));
        assert!(body.contains("spur_preempt_grace_checkpoint_total{outcome=\"completed\"} 1\n"));
        assert!(body.contains("spur_checkpoint_events_dropped_total{reason=\"busy\"} 3\n"));
        assert!(
            body.contains("spur_job_run_checkpoints_failed_total{reason=\"malformed_json\"} 2\n")
        );
    }

    #[test]
    fn unobserved_reasons_and_outcomes_create_no_series() {
        let metrics = CheckpointMetrics::new();
        metrics.record_run_end(&RunEnd {
            reason: "completed",
            checkpoint: CheckpointState::Completed,
            seconds_since_last_checkpoint: Some(1.0),
            checkpoints_completed: 1,
            grace: None,
            restored: false,
        });
        let body = encode_checkpoint_metrics(&metrics);
        assert!(!body.contains("reason=\"failed\""));
        assert!(!body.contains("outcome=\"none\""));
        assert!(!body.contains("spur_checkpoint_events_dropped"));
        assert!(!body.contains("spur_job_run_checkpoints_failed"));
        assert!(!body.contains("spur_job_runs_restored_total"));
    }

    #[test]
    fn per_job_gauges_omit_optional_samples_when_none() {
        let jobs = [CheckpointJobSnapshot {
            job_id: 11,
            user: "bob".into(),
            account: "teamB".into(),
            job_name: "eval".into(),
            completed: 0,
            last_step: None,
            seconds_since_last: None,
            in_progress: false,
            dropped: 2,
        }];
        let body = encode_checkpoint_job_metrics(&jobs);
        assert!(body.contains(
            "spur_job_checkpoints_completed{job_id=\"11\",user=\"bob\",account=\"teamB\",\
             job_name=\"eval\"} 0\n"
        ));
        assert!(body.contains(
            "spur_job_checkpoint_in_progress{job_id=\"11\",user=\"bob\",account=\"teamB\",\
             job_name=\"eval\"} 0\n"
        ));
        assert!(body.contains(
            "spur_job_checkpoint_events_dropped{job_id=\"11\",user=\"bob\",account=\"teamB\",\
             job_name=\"eval\"} 2\n"
        ));
        assert!(!body.contains("spur_job_checkpoint_events_failed"));
        assert!(!body.contains("spur_job_checkpoint_last_step{"));
        assert!(!body.contains("spur_job_checkpoint_seconds_since_last{"));
    }

    #[test]
    fn per_job_gauges_include_optional_samples_when_some() {
        let jobs = [CheckpointJobSnapshot {
            job_id: 10,
            user: "alice".into(),
            account: "teamA".into(),
            job_name: "train".into(),
            completed: 3,
            last_step: Some(300),
            seconds_since_last: Some(45.5),
            in_progress: true,
            dropped: 0,
        }];
        let body = encode_checkpoint_job_metrics(&jobs);
        assert!(body.contains(
            "spur_job_checkpoint_last_step{job_id=\"10\",user=\"alice\",account=\"teamA\",\
             job_name=\"train\"} 300\n"
        ));
        assert!(body.contains(
            "spur_job_checkpoint_seconds_since_last{job_id=\"10\",user=\"alice\",account=\"teamA\",\
             job_name=\"train\"} 45.5\n"
        ));
        assert!(body.contains(
            "spur_job_checkpoint_in_progress{job_id=\"10\",user=\"alice\",account=\"teamA\",\
             job_name=\"train\"} 1\n"
        ));
    }

    #[test]
    fn job_failed_counter_includes_job_name_and_reason_labels() {
        let jobs = [CheckpointJobFailed {
            job_id: 10,
            user: "alice".into(),
            account: "teamA".into(),
            job_name: "train".into(),
            reasons: vec![("job_mismatch".into(), 3), ("oversize".into(), 1)],
        }];
        let body = encode_checkpoint_job_failed_metrics(&jobs);
        assert!(body.contains("# TYPE spur_job_checkpoints_failed counter"));
        assert!(body.contains(
            "spur_job_checkpoints_failed_total{job_id=\"10\",user=\"alice\",account=\"teamA\",\
             job_name=\"train\",reason=\"job_mismatch\"} 3\n"
        ));
        assert!(body.contains(
            "spur_job_checkpoints_failed_total{job_id=\"10\",user=\"alice\",account=\"teamA\",\
             job_name=\"train\",reason=\"oversize\"} 1\n"
        ));
    }

    #[test]
    fn job_failed_counter_empty_account_and_job_name_yield_empty_labels() {
        let jobs = [CheckpointJobFailed {
            job_id: 11,
            user: "bob".into(),
            account: String::new(),
            job_name: "".into(),
            reasons: vec![("malformed_json".into(), 1)],
        }];
        let body = encode_checkpoint_job_failed_metrics(&jobs);
        assert!(body.contains(
            "spur_job_checkpoints_failed_total{job_id=\"11\",user=\"bob\",account=\"\",\
             job_name=\"\",reason=\"malformed_json\"} 1\n"
        ));
    }

    #[test]
    fn job_failed_counter_sanitizes_and_truncates_job_name() {
        let raw = format!("quote\"\\name\x07{}", "x".repeat(100));
        let jobs = [CheckpointJobFailed {
            job_id: 12,
            user: "carol".into(),
            account: "teamC".into(),
            job_name: raw,
            reasons: vec![("unknown_kind".into(), 2)],
        }];
        let body = encode_checkpoint_job_failed_metrics(&jobs);
        let sanitized = sanitize_job_name(&format!("quote\"\\name\x07{}", "x".repeat(100)));
        assert_eq!(sanitized.chars().count(), JOB_NAME_LABEL_MAX_CHARS);
        assert!(!sanitized.contains('\x07'));
        // The encoder escapes the quote and backslash that survived sanitization.
        assert!(body.contains(r#"job_name="quote\"\\name_xxx"#));
        assert!(body.contains("reason=\"unknown_kind\"} 2\n"));
    }

    #[test]
    fn job_failed_counter_creates_no_series_for_an_empty_job_list() {
        let body = encode_checkpoint_job_failed_metrics(&[]);
        assert_eq!(body, "# EOF\n");
    }
}
