// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Long-lived accumulator for checkpoint-telemetry counters/histograms (job run endings,
//! restored-run counts, preempt-grace outcomes, invalid- and dropped-event counts), plus the
//! per-job snapshot types exported on `/metrics/jobs-users-accts`. See `export::checkpoint` for
//! OpenMetrics registration/encoding.

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::histogram::{exponential_buckets, Histogram};

/// Checkpoint state of a finalized job run.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum CheckpointState {
    None,
    Completed,
    InProgress,
}

impl CheckpointState {
    /// Label value used on `spur_job_runs_ended`.
    pub fn label(self) -> &'static str {
        match self {
            CheckpointState::None => "none",
            CheckpointState::Completed => "completed",
            CheckpointState::InProgress => "in_progress",
        }
    }
}

/// Outcome of a preempt-grace checkpoint window.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum GraceOutcome {
    Completed,
    Failed,
    None,
}

impl GraceOutcome {
    /// Label value used on `spur_preempt_grace_checkpoint`.
    pub fn label(self) -> &'static str {
        match self {
            GraceOutcome::Completed => "completed",
            GraceOutcome::Failed => "failed",
            GraceOutcome::None => "none",
        }
    }
}

/// A finalized job run's checkpoint outcome, reported to [`CheckpointMetrics::record_run_end`].
pub struct RunEnd {
    /// Finalized `JobState`, lowercase snake_case (e.g. `completed`, `node_fail`).
    pub reason: &'static str,
    pub checkpoint: CheckpointState,
    pub seconds_since_last_checkpoint: Option<f64>,
    pub checkpoints_completed: u64,
    /// `None` when the run never entered a preempt-grace window.
    pub grace: Option<GraceOutcome>,
    /// Whether any record in this run reported a restore, counted on
    /// `spur_job_runs_restored_total`.
    pub restored: bool,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub(crate) struct RunEndLabel {
    reason: String,
    checkpoint: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub(crate) struct ReasonLabel {
    reason: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub(crate) struct GraceLabel {
    outcome: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub(crate) struct DropLabel {
    reason: String,
}

/// Long-lived accumulator for checkpoint-telemetry counters/histograms. `Family` is
/// `Arc`-backed, so a clone of `CheckpointMetrics` shares the same series with the original --
/// a fresh `Registry` is built per encode from clones of these families (see
/// `export::checkpoint`), the same accumulator pattern as [`crate::k8s::K8sMetrics`].
#[derive(Clone, Debug)]
pub struct CheckpointMetrics {
    pub(crate) runs_ended: Family<RunEndLabel, Counter>,
    pub(crate) runs_restored: Family<ReasonLabel, Counter>,
    pub(crate) seconds_since_last_checkpoint: Family<ReasonLabel, Histogram>,
    pub(crate) checkpoints_completed: Family<ReasonLabel, Histogram>,
    pub(crate) preempt_grace: Family<GraceLabel, Counter>,
    pub(crate) events_dropped: Family<DropLabel, Counter>,
    pub(crate) events_invalid: Family<DropLabel, Counter>,
}

impl Default for CheckpointMetrics {
    fn default() -> Self {
        // Seconds-since-last-checkpoint: 1 min .. ~34 h. Checkpoints-completed: 1 .. ~2048.
        let seconds_buckets = || Histogram::new(exponential_buckets(60.0, 2.0, 12));
        let completed_buckets = || Histogram::new(exponential_buckets(1.0, 2.0, 12));
        Self {
            runs_ended: Family::default(),
            runs_restored: Family::default(),
            seconds_since_last_checkpoint: Family::new_with_constructor(seconds_buckets),
            checkpoints_completed: Family::new_with_constructor(completed_buckets),
            preempt_grace: Family::default(),
            events_dropped: Family::default(),
            events_invalid: Family::default(),
        }
    }
}

impl CheckpointMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a finalized job run's checkpoint outcome: one `runs_ended` count, one
    /// `checkpoints_completed` observation, an optional `seconds_since_last_checkpoint`
    /// observation, an optional `preempt_grace` count, and -- when the run reported a restore --
    /// one `runs_restored` count.
    pub fn record_run_end(&self, end: &RunEnd) {
        self.runs_ended
            .get_or_create(&RunEndLabel {
                reason: end.reason.to_string(),
                checkpoint: end.checkpoint.label().to_string(),
            })
            .inc();

        if end.restored {
            self.runs_restored
                .get_or_create(&ReasonLabel {
                    reason: end.reason.to_string(),
                })
                .inc();
        }

        self.checkpoints_completed
            .get_or_create(&ReasonLabel {
                reason: end.reason.to_string(),
            })
            .observe(end.checkpoints_completed as f64);

        if let Some(seconds) = end.seconds_since_last_checkpoint {
            self.seconds_since_last_checkpoint
                .get_or_create(&ReasonLabel {
                    reason: end.reason.to_string(),
                })
                .observe(seconds);
        }

        if let Some(outcome) = end.grace {
            self.preempt_grace
                .get_or_create(&GraceLabel {
                    outcome: outcome.label().to_string(),
                })
                .inc();
        }
    }

    /// Add `n` to the dropped-event counter for `reason` (a [`DropReason::as_str`] value whose
    /// [`DropReason::is_invalid`] is `false`). A no-op when `n == 0`, so a reason with no drops
    /// never creates a series -- only observed series are exported.
    pub fn add_dropped(&self, reason: &str, n: u64) {
        if n == 0 {
            return;
        }
        self.events_dropped
            .get_or_create(&DropLabel {
                reason: reason.to_string(),
            })
            .inc_by(n);
    }

    /// Add `n` to the invalid-event counter for `reason` (a [`DropReason::as_str`] value whose
    /// [`DropReason::is_invalid`] is `true`), exported as `spur_job_run_checkpoints_failed_total`.
    /// Same zero-is-a-no-op rule as [`Self::add_dropped`].
    pub fn add_invalid(&self, reason: &str, n: u64) {
        if n == 0 {
            return;
        }
        self.events_invalid
            .get_or_create(&DropLabel {
                reason: reason.to_string(),
            })
            .inc_by(n);
    }
}

/// Per-job checkpoint snapshot for `/metrics/jobs-users-accts`, built from the live (not yet
/// finalized) runs held in the controller's `CheckpointStore`.
pub struct CheckpointJobSnapshot {
    pub job_id: u32,
    pub user: String,
    pub account: String,
    pub job_name: String,
    pub completed: u64,
    pub last_step: Option<u64>,
    pub seconds_since_last: Option<f64>,
    pub in_progress: bool,
    pub dropped: u64,
}

/// Per-job, per-reason cumulative invalid-event count for `spur_job_checkpoints_failed_total`
/// on `/metrics/jobs-users-accts`, cumulative across every run attempt the job has had --
/// reported from a job's current live run, or -- once that run is gone -- from its most
/// recently finalized one (see `CheckpointStore::job_failed_snapshots`).
pub struct CheckpointJobFailed {
    pub job_id: u32,
    pub user: String,
    pub account: String,
    pub job_name: String,
    /// `(reason, count)` pairs with `count > 0` -- one series per reason.
    pub reasons: Vec<(String, u64)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::checkpoint::encode_checkpoint_metrics;

    #[test]
    fn record_run_end_does_not_count_grace_when_none() {
        let metrics = CheckpointMetrics::new();
        metrics.record_run_end(&RunEnd {
            reason: "completed",
            checkpoint: CheckpointState::Completed,
            seconds_since_last_checkpoint: Some(30.0),
            checkpoints_completed: 2,
            grace: None,
            restored: false,
        });
        let body = encode_checkpoint_metrics(&metrics);
        assert!(!body.contains("spur_preempt_grace_checkpoint_total"));
    }

    #[test]
    fn add_dropped_zero_creates_no_series() {
        let metrics = CheckpointMetrics::new();
        metrics.add_dropped("rate_limited", 0);
        let body = encode_checkpoint_metrics(&metrics);
        assert!(!body.contains("rate_limited"));
        assert!(!body.contains("spur_checkpoint_events_dropped_total"));
    }

    #[test]
    fn add_invalid_zero_creates_no_series() {
        let metrics = CheckpointMetrics::new();
        metrics.add_invalid("oversize", 0);
        let body = encode_checkpoint_metrics(&metrics);
        assert!(!body.contains("oversize"));
        assert!(!body.contains("spur_job_run_checkpoints_failed_total"));
    }

    #[test]
    fn add_invalid_populates_the_failed_family_not_the_dropped_one() {
        let metrics = CheckpointMetrics::new();
        metrics.add_invalid("malformed_json", 4);
        let body = encode_checkpoint_metrics(&metrics);
        assert!(
            body.contains("spur_job_run_checkpoints_failed_total{reason=\"malformed_json\"} 4\n")
        );
        assert!(!body.contains("spur_checkpoint_events_dropped_total"));
    }

    #[test]
    fn record_run_end_counts_a_restore_only_when_reported() {
        let metrics = CheckpointMetrics::new();
        metrics.record_run_end(&RunEnd {
            reason: "completed",
            checkpoint: CheckpointState::Completed,
            seconds_since_last_checkpoint: None,
            checkpoints_completed: 0,
            grace: None,
            restored: false,
        });
        let body = encode_checkpoint_metrics(&metrics);
        assert!(!body.contains("spur_job_runs_restored_total"));

        metrics.record_run_end(&RunEnd {
            reason: "preempted",
            checkpoint: CheckpointState::None,
            seconds_since_last_checkpoint: None,
            checkpoints_completed: 0,
            grace: None,
            restored: true,
        });
        let body = encode_checkpoint_metrics(&metrics);
        assert!(body.contains("spur_job_runs_restored_total{reason=\"preempted\"} 1\n"));
    }
}
