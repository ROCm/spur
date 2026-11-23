// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Golden and catalog tests for checkpoint-telemetry metrics export.
//!
//! The golden fixtures use a single observation per family: `Family` stores series in a
//! `HashMap`, so with 2+ distinct label keys in one family, encode order is randomized per
//! process and an exact-text comparison would be flaky. The multi-series catalog tests below use
//! order-independent `contains` assertions instead.

use spur_metrics::{
    encode_checkpoint_job_failed_metrics, encode_checkpoint_job_metrics, encode_checkpoint_metrics,
    CheckpointJobFailed, CheckpointJobSnapshot, CheckpointMetrics, CheckpointState, GraceOutcome,
    RunEnd,
};
use std::path::PathBuf;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn golden_metrics() -> CheckpointMetrics {
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
    metrics.add_dropped("rate_limited", 0);
    metrics.add_invalid("malformed_json", 2);
    metrics.add_invalid("oversize", 0);
    metrics
}

fn catalog_metrics() -> CheckpointMetrics {
    let metrics = CheckpointMetrics::new();
    metrics.record_run_end(&RunEnd {
        reason: "completed",
        checkpoint: CheckpointState::Completed,
        seconds_since_last_checkpoint: Some(120.0),
        checkpoints_completed: 5,
        grace: None,
        restored: false,
    });
    metrics.record_run_end(&RunEnd {
        reason: "preempted",
        checkpoint: CheckpointState::InProgress,
        seconds_since_last_checkpoint: None,
        checkpoints_completed: 0,
        grace: Some(GraceOutcome::Completed),
        restored: true,
    });
    metrics.record_run_end(&RunEnd {
        reason: "failed",
        checkpoint: CheckpointState::None,
        seconds_since_last_checkpoint: None,
        checkpoints_completed: 0,
        grace: Some(GraceOutcome::Failed),
        restored: false,
    });
    metrics.record_run_end(&RunEnd {
        reason: "cancelled",
        checkpoint: CheckpointState::None,
        seconds_since_last_checkpoint: None,
        checkpoints_completed: 0,
        grace: Some(GraceOutcome::None),
        restored: false,
    });
    metrics.add_dropped("busy", 3);
    metrics.add_dropped("rate_limited", 0);
    metrics.add_invalid("malformed_json", 2);
    metrics.add_invalid("oversize", 0);
    metrics
}

fn golden_jobs() -> Vec<CheckpointJobSnapshot> {
    vec![CheckpointJobSnapshot {
        job_id: 10,
        user: "alice".into(),
        account: "teamA".into(),
        job_name: "train".into(),
        completed: 3,
        last_step: Some(300),
        seconds_since_last: Some(45.5),
        in_progress: true,
        dropped: 0,
    }]
}

fn catalog_jobs() -> Vec<CheckpointJobSnapshot> {
    let mut jobs = golden_jobs();
    jobs.push(CheckpointJobSnapshot {
        job_id: 11,
        user: "bob".into(),
        account: "teamB".into(),
        job_name: "eval".into(),
        completed: 0,
        last_step: None,
        seconds_since_last: None,
        in_progress: false,
        dropped: 2,
    });
    jobs
}

fn golden_jobs_failed() -> Vec<CheckpointJobFailed> {
    vec![CheckpointJobFailed {
        job_id: 10,
        user: "alice".into(),
        account: "teamA".into(),
        job_name: "train".into(),
        reasons: vec![("job_mismatch".into(), 2)],
    }]
}

fn catalog_jobs_failed() -> Vec<CheckpointJobFailed> {
    let mut jobs = golden_jobs_failed();
    jobs.push(CheckpointJobFailed {
        job_id: 11,
        user: "bob".into(),
        account: String::new(),
        job_name: "eval".into(),
        reasons: vec![("malformed_json".into(), 1), ("oversize".into(), 4)],
    });
    jobs
}

#[test]
fn golden_checkpoint_metrics() {
    let body = encode_checkpoint_metrics(&golden_metrics());
    let path = fixtures_dir().join("checkpoint.prom");
    let expected =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert_eq!(body, expected);
}

#[test]
fn golden_checkpoint_job_metrics() {
    let body = encode_checkpoint_job_metrics(&golden_jobs());
    let path = fixtures_dir().join("checkpoint_jobs.prom");
    let expected =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert_eq!(body, expected);
}

#[test]
fn golden_checkpoint_job_failed_metrics() {
    let body = encode_checkpoint_job_failed_metrics(&golden_jobs_failed());
    let path = fixtures_dir().join("checkpoint_jobs_failed.prom");
    let expected =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert_eq!(body, expected);
}

/// Every family name from the task brief appears with the right `# TYPE`, and only observed
/// series are present (no pre-seeded reasons/outcomes).
#[test]
fn checkpoint_metrics_catalog() {
    let body = encode_checkpoint_metrics(&catalog_metrics());

    assert!(body.contains("# TYPE spur_job_runs_ended counter"));
    assert!(body.contains("# TYPE spur_job_runs_restored counter"));
    assert!(body.contains("# TYPE spur_job_run_end_seconds_since_last_checkpoint histogram"));
    assert!(body.contains("# TYPE spur_job_run_checkpoints_completed histogram"));
    assert!(body.contains("# TYPE spur_preempt_grace_checkpoint counter"));
    assert!(body.contains("# TYPE spur_checkpoint_events_dropped counter"));
    assert!(body.contains("# TYPE spur_job_run_checkpoints_failed counter"));

    assert!(body
        .contains("spur_job_runs_ended_total{reason=\"completed\",checkpoint=\"completed\"} 1\n"));
    assert!(body.contains(
        "spur_job_runs_ended_total{reason=\"preempted\",checkpoint=\"in_progress\"} 1\n"
    ));
    assert!(body.contains("spur_job_runs_ended_total{reason=\"failed\",checkpoint=\"none\"} 1\n"));
    assert!(
        body.contains("spur_job_runs_ended_total{reason=\"cancelled\",checkpoint=\"none\"} 1\n")
    );

    // Only the restored run (reason="preempted") gets a restored-count series.
    assert!(body.contains("spur_job_runs_restored_total{reason=\"preempted\"} 1\n"));
    assert!(!body.contains("spur_job_runs_restored_total{reason=\"completed\"}"));
    assert!(!body.contains("spur_job_runs_restored_total{reason=\"failed\"}"));
    assert!(!body.contains("spur_job_runs_restored_total{reason=\"cancelled\"}"));

    assert!(body.contains("spur_preempt_grace_checkpoint_total{outcome=\"completed\"} 1\n"));
    assert!(body.contains("spur_preempt_grace_checkpoint_total{outcome=\"failed\"} 1\n"));
    assert!(body.contains("spur_preempt_grace_checkpoint_total{outcome=\"none\"} 1\n"));

    assert!(body.contains("spur_checkpoint_events_dropped_total{reason=\"busy\"} 3\n"));
    assert!(body.contains("spur_job_run_checkpoints_failed_total{reason=\"malformed_json\"} 2\n"));
    // A zero-count reason must never create a series, in either family.
    assert!(!body.contains("rate_limited"));
    assert!(!body.contains("oversize"));

    assert!(body.ends_with("# EOF\n"));
    assert_eq!(body.matches("# EOF").count(), 1);
}

#[test]
fn checkpoint_job_metrics_catalog() {
    let body = encode_checkpoint_job_metrics(&catalog_jobs());

    assert!(body.contains("# TYPE spur_job_checkpoints_completed gauge"));
    assert!(body.contains("# TYPE spur_job_checkpoint_last_step gauge"));
    assert!(body.contains("# TYPE spur_job_checkpoint_seconds_since_last gauge"));
    assert!(body.contains("# TYPE spur_job_checkpoint_in_progress gauge"));
    assert!(body.contains("# TYPE spur_job_checkpoint_events_dropped gauge"));
    // Superseded by the per-reason spur_job_checkpoints_failed_total counter.
    assert!(!body.contains("spur_job_checkpoint_events_failed"));

    // Job 11 has no completed checkpoint yet; last_step/seconds_since_last must be absent.
    assert!(!body.contains("spur_job_checkpoint_last_step{job_id=\"11\""));
    assert!(!body.contains("spur_job_checkpoint_seconds_since_last{job_id=\"11\""));
    assert!(body.contains(
        "spur_job_checkpoint_last_step{job_id=\"10\",user=\"alice\",account=\"teamA\",\
         job_name=\"train\"} 300\n"
    ));

    assert!(body.ends_with("# EOF\n"));
}

/// Every reason with a nonzero count gets its own series; `job_name` is present even when the
/// account is empty -- the per-job identity label set (`job_id, user, account, job_name`) never
/// depends on another label's value.
#[test]
fn checkpoint_job_failed_metrics_catalog() {
    let body = encode_checkpoint_job_failed_metrics(&catalog_jobs_failed());

    assert!(body.contains("# TYPE spur_job_checkpoints_failed counter"));
    assert!(body.contains(
        "spur_job_checkpoints_failed_total{job_id=\"10\",user=\"alice\",account=\"teamA\",\
         job_name=\"train\",reason=\"job_mismatch\"} 2\n"
    ));
    assert!(body.contains(
        "spur_job_checkpoints_failed_total{job_id=\"11\",user=\"bob\",account=\"\",\
         job_name=\"eval\",reason=\"malformed_json\"} 1\n"
    ));
    assert!(body.contains(
        "spur_job_checkpoints_failed_total{job_id=\"11\",user=\"bob\",account=\"\",\
         job_name=\"eval\",reason=\"oversize\"} 4\n"
    ));

    assert!(body.ends_with("# EOF\n"));
    assert_eq!(body.matches("# EOF").count(), 1);
}

/// Re-encoding after more observations reflects the live family state (the same families are
/// reused via cheap `Arc` clones, not rebuilt each call).
#[test]
fn encoding_twice_reflects_live_values() {
    let metrics = CheckpointMetrics::new();
    assert!(!encode_checkpoint_metrics(&metrics).contains("reason=\"timeout\""));

    metrics.record_run_end(&RunEnd {
        reason: "timeout",
        checkpoint: CheckpointState::Completed,
        seconds_since_last_checkpoint: Some(10.0),
        checkpoints_completed: 1,
        grace: None,
        restored: false,
    });
    let body = encode_checkpoint_metrics(&metrics);
    assert!(
        body.contains("spur_job_runs_ended_total{reason=\"timeout\",checkpoint=\"completed\"} 1\n")
    );

    metrics.record_run_end(&RunEnd {
        reason: "timeout",
        checkpoint: CheckpointState::Completed,
        seconds_since_last_checkpoint: Some(20.0),
        checkpoints_completed: 2,
        grace: None,
        restored: false,
    });
    let body = encode_checkpoint_metrics(&metrics);
    assert!(
        body.contains("spur_job_runs_ended_total{reason=\"timeout\",checkpoint=\"completed\"} 2\n")
    );
}

/// Regenerate `tests/fixtures/checkpoint.prom`, `checkpoint_jobs.prom`, and
/// `checkpoint_jobs_failed.prom` after intentional encoder changes:
/// `cargo test -p spur-metrics --test checkpoint_export_golden refresh_golden_fixtures -- --ignored --exact`
#[test]
#[ignore = "manual fixture refresh"]
fn refresh_golden_fixtures() {
    let dir = fixtures_dir();
    std::fs::create_dir_all(&dir).expect("fixtures dir");
    std::fs::write(
        dir.join("checkpoint.prom"),
        encode_checkpoint_metrics(&golden_metrics()),
    )
    .expect("write checkpoint fixture");
    std::fs::write(
        dir.join("checkpoint_jobs.prom"),
        encode_checkpoint_job_metrics(&golden_jobs()),
    )
    .expect("write checkpoint jobs fixture");
    std::fs::write(
        dir.join("checkpoint_jobs_failed.prom"),
        encode_checkpoint_job_failed_metrics(&golden_jobs_failed()),
    )
    .expect("write checkpoint jobs failed fixture");
}
