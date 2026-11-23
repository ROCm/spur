// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Leader-local store of in-flight checkpoint telemetry. Pure (no `ClusterManager`
//! dependency): `ingest` accepts or rejects a node's record against the run it
//! claims to belong to, `mark_ended` records a run's finalization (from the
//! `JobFinalized` hook), and `sweep` finalizes an ended run once `FINALIZE_GRACE`
//! has elapsed since it ended, recording the outcome into the long-lived
//! `CheckpointMetrics` accumulator and dropping the run's state. A run that
//! never reported a single checkpoint event (see `has_any_event`) is dropped
//! without recording anything into the run-end family at all.
//!
//! The store lives on `ClusterManager` for the whole process -- it is NOT
//! rebuilt on a leadership change, and `mark_ended` only fires while this node
//! is leader. A run that ends while this node is a follower would otherwise
//! leave a not-yet-ended entry here forever. `retain_live` bounds that: the
//! periodic sweep task calls it with the authoritative set of still-running
//! `(job_id, run_attempt)` pairs, dropping every not-ended entry absent from
//! it. Ended entries are left alone -- `sweep`'s own grace-deadline logic
//! finalizes or holds those.
//!
//! A run's invalid-event counts, by reason, additionally feed the per-job
//! `spur_job_checkpoints_failed_total` series (`job_failed_snapshots`). Those
//! counts are cumulative per `job_id` in `failed_totals`, added to by every
//! attempt's deltas as `ingest` sees them -- so the series stays monotonic
//! across a requeue instead of resetting to the new run_attempt's own count.
//! A separate `failed_jobs` map, keyed by `job_id`, tracks only when each job
//! last finalized with an invalid event, so the cumulative count stays
//! scrapeable for `FAILED_SERIES_RETENTION` after a job's last run ends, even
//! with no live run to report it from; `sweep` prunes expired entries (along
//! with their `failed_totals` counterpart, unless the job is live again by
//! then) and `MAX_RETAINED_FAILED_JOBS` bounds `failed_jobs`'s size, evicting
//! the oldest entry first.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::{DateTime, Utc};

use spur_core::checkpoint::DropReason;
use spur_metrics::{
    CheckpointJobFailed, CheckpointJobSnapshot, CheckpointMetrics, CheckpointState, GraceOutcome,
    RunEnd,
};
use spur_proto::proto::StepCheckpointRecord;

/// How long a leader waits, after a run ends, for every allocated node to
/// report its final checkpoint record before finalizing with partial data.
pub const FINALIZE_GRACE: Duration = Duration::from_secs(60);

/// `FINALIZE_GRACE` as a `chrono::Duration`, for arithmetic against `DateTime<Utc>`.
const FINALIZE_GRACE_CHRONO: chrono::Duration =
    chrono::Duration::seconds(FINALIZE_GRACE.as_secs() as i64);

/// Per-`ReportCheckpointTelemetry` request cap; `ClusterManager::checkpoint_ingest`
/// takes only this many records from a batch, counting the rest rejected.
pub const MAX_RECORDS_PER_REQUEST: usize = 1024;

/// Per-run cap on distinct (node, step) records this store will hold. Guards
/// a single run from growing unbounded on a pathological number of steps.
pub const MAX_RECORDS_PER_RUN: usize = 4096;

/// How long a finalized run's invalid-event counts stay visible on
/// `spur_job_checkpoints_failed_total` after the run itself is gone.
pub const FAILED_SERIES_RETENTION: Duration = Duration::from_secs(3600);

/// `FAILED_SERIES_RETENTION` as a `chrono::Duration`, for arithmetic against `DateTime<Utc>`.
const FAILED_SERIES_RETENTION_CHRONO: chrono::Duration =
    chrono::Duration::seconds(FAILED_SERIES_RETENTION.as_secs() as i64);

/// Cap on retained finalized-run failed-job entries. The oldest `finalized_at` is evicted first.
pub const MAX_RETAINED_FAILED_JOBS: usize = 10_000;

/// The allocation a run's checkpoint records are validated against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunInfo {
    pub job_id: u32,
    pub run_attempt: u32,
    pub nodes: Vec<String>,
    pub user: String,
    pub account: String,
    pub job_name: String,
}

/// Why `ingest` refused a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// The (job_id, run_attempt) names no live or just-ended run this store
    /// knows about -- including a run that already finalized and was dropped.
    UnknownRun,
    /// The run is known, but the reporting node is not in its allocation.
    NodeNotAllocated,
    /// The run already holds `MAX_RECORDS_PER_RUN` distinct (node, step) records.
    OverCap,
}

/// A run's finalization, captured by `mark_ended` from the `JobFinalized`
/// side-effect hook.
struct EndedInfo {
    /// The finalized `JobState`, lowercase snake_case -- passed straight to
    /// `RunEnd::reason`.
    reason: &'static str,
    ended_at: DateTime<Utc>,
    /// Set only when this run ended inside a preempt grace window.
    grace_started_at: Option<DateTime<Utc>>,
}

/// One run's collected checkpoint state: every node's latest record per step,
/// plus the last-seen cumulative drop counts used to compute deltas.
struct RunEntry {
    info: RunInfo,
    ended: Option<EndedInfo>,
    /// node -> step_id -> that node's latest record for the step.
    records: HashMap<String, HashMap<u32, StepCheckpointRecord>>,
    /// (node, step_id) -> last-seen cumulative `dropped` counts, so a resend
    /// of the same cumulative record contributes a zero delta.
    last_dropped: HashMap<(String, u32), HashMap<String, u64>>,
    /// Set by `note_grace` when this run enters a preempt-grace window, ahead
    /// of knowing whether or how it will end. `mark_ended` falls back to this
    /// when its own `grace_started_at` argument is `None`.
    noted_grace_started_at: Option<DateTime<Utc>>,
}

impl RunEntry {
    fn new(info: RunInfo) -> Self {
        Self {
            info,
            ended: None,
            records: HashMap::new(),
            last_dropped: HashMap::new(),
            noted_grace_started_at: None,
        }
    }
}

/// Merged view of a run's records: training-step progress rolled up across
/// nodes and job steps.
struct Merged {
    completed: u64,
    last_completed_step: Option<u64>,
    last_completed_at: Option<DateTime<Utc>>,
    in_progress: bool,
    any_failed: bool,
}

fn proto_timestamp_to_datetime(ts: &Option<prost_types::Timestamp>) -> Option<DateTime<Utc>> {
    let ts = ts.as_ref()?;
    DateTime::from_timestamp(ts.seconds, ts.nanos as u32)
}

/// Roll up every node's per-step record into one run-level view. Per step
/// (ranks on all nodes report the same training progression), `completed`
/// takes the max over nodes; the run's `completed` sums that max across
/// steps. `last_completed_step`/`last_completed_at` are the max over every
/// record in the run, which is authoritative when ranks disagree on a step.
fn merge(entry: &RunEntry) -> Merged {
    let mut per_step_completed: HashMap<u32, u64> = HashMap::new();
    let mut last_completed_step: Option<u64> = None;
    let mut last_completed_at: Option<DateTime<Utc>> = None;
    let mut max_in_progress_step: Option<u64> = None;
    let mut any_failed = false;

    for steps in entry.records.values() {
        for (step_id, rec) in steps {
            let slot = per_step_completed.entry(*step_id).or_insert(0);
            *slot = (*slot).max(rec.completed);

            if let Some(step) = rec.last_completed_step {
                last_completed_step = Some(last_completed_step.map_or(step, |cur| cur.max(step)));
            }
            if let Some(at) = proto_timestamp_to_datetime(&rec.last_completed_at) {
                last_completed_at = Some(last_completed_at.map_or(at, |cur| cur.max(at)));
            }
            if let Some(step) = rec.in_progress_step {
                max_in_progress_step = Some(max_in_progress_step.map_or(step, |cur| cur.max(step)));
            }
            any_failed |= rec.failed > 0;
        }
    }

    let completed = per_step_completed.values().sum();
    // No completed step anywhere counts as "exceeded": an in-progress report
    // with nothing finished yet is still in progress.
    let in_progress = match (max_in_progress_step, last_completed_step) {
        (Some(_), None) => true,
        (Some(in_step), Some(done)) => in_step > done,
        (None, _) => false,
    };

    Merged {
        completed,
        last_completed_step,
        last_completed_at,
        in_progress,
        any_failed,
    }
}

fn checkpoint_state(merged: &Merged) -> CheckpointState {
    if merged.in_progress {
        CheckpointState::InProgress
    } else if merged.completed > 0 {
        CheckpointState::Completed
    } else {
        CheckpointState::None
    }
}

fn seconds_since(
    reference: DateTime<Utc>,
    last_completed_at: Option<DateTime<Utc>>,
) -> Option<f64> {
    last_completed_at.map(|at| {
        let secs = (reference - at).num_milliseconds() as f64 / 1000.0;
        secs.max(0.0)
    })
}

/// Preempt-grace outcome. The wire record carries no event
/// timestamp for a failed checkpoint, so this counts ANY failed checkpoint
/// anywhere in the run's lifetime, not only one that actually failed during
/// the grace window: `Failed` means "the run has a failed checkpoint and did
/// not complete one at or after the grace started" (completing one takes
/// priority as `Completed`) -- not "failed specifically during the grace".
fn grace_outcome(ended: &EndedInfo, merged: &Merged) -> Option<GraceOutcome> {
    let grace_started_at = ended.grace_started_at?;
    let outcome = if merged
        .last_completed_at
        .is_some_and(|at| at >= grace_started_at)
    {
        GraceOutcome::Completed
    } else if merged.any_failed {
        GraceOutcome::Failed
    } else {
        GraceOutcome::None
    };
    Some(outcome)
}

/// Whether any node's record for this run shows a restore, counted on
/// `spur_job_runs_restored_total`.
fn any_restore(entry: &RunEntry) -> bool {
    entry
        .records
        .values()
        .flat_map(HashMap::values)
        .any(|rec| rec.restore_step.is_some())
}

fn build_run_end(entry: &RunEntry, ended: &EndedInfo) -> RunEnd {
    let merged = merge(entry);
    RunEnd {
        reason: ended.reason,
        checkpoint: checkpoint_state(&merged),
        seconds_since_last_checkpoint: seconds_since(ended.ended_at, merged.last_completed_at),
        checkpoints_completed: merged.completed,
        grace: grace_outcome(ended, &merged),
        restored: any_restore(entry),
    }
}

fn build_job_snapshot(entry: &RunEntry, now: DateTime<Utc>) -> CheckpointJobSnapshot {
    let merged = merge(entry);
    let dropped = entry
        .last_dropped
        .values()
        .flatten()
        .filter(|(reason, _)| !is_invalid_reason(reason))
        .map(|(_, count)| *count)
        .sum();
    CheckpointJobSnapshot {
        job_id: entry.info.job_id,
        user: entry.info.user.clone(),
        account: entry.info.account.clone(),
        job_name: entry.info.job_name.clone(),
        completed: merged.completed,
        last_step: merged.last_completed_step,
        seconds_since_last: seconds_since(now, merged.last_completed_at),
        in_progress: merged.in_progress,
        dropped,
    }
}

/// Whether `reason` (a wire name from `StepCheckpointRecord.dropped`) belongs to the
/// invalid-event family (`spur_job_run_checkpoints_failed_total`) rather than the
/// delivery/attribution one (`spur_checkpoint_events_dropped_total`). An unrecognized name
/// (a newer agent's reason an older controller does not yet know) falls back to the dropped
/// family rather than being silently discarded.
fn is_invalid_reason(reason: &str) -> bool {
    DropReason::parse(reason).is_some_and(DropReason::is_invalid)
}

/// Whether any node's record for this run shows an applied checkpoint event: a completed or
/// failed checkpoint, one in progress, or a restore. A run with none of these across every
/// (node, step) record never exercised its workload's reporting path at all, so run-end
/// metrics are skipped for it entirely (see [`CheckpointStore::finalize_if_ready`]).
fn has_any_event(entry: &RunEntry) -> bool {
    entry.records.values().flat_map(HashMap::values).any(|rec| {
        rec.completed > 0
            || rec.failed > 0
            || rec.in_progress_step.is_some()
            || rec.restore_step.is_some()
    })
}

/// Marks that `job_id` finalized with an invalid event, so its cumulative count in
/// `CheckpointStore::failed_totals` stays visible on `spur_job_checkpoints_failed_total` for
/// `FAILED_SERIES_RETENTION` after it ends, even with no live run to report it from. Keyed by
/// `job_id` in `CheckpointStore::failed_jobs`, so a later run_attempt of the same job simply
/// overwrites the earlier one's labels and timestamp.
struct FailedJobEntry {
    user: String,
    account: String,
    job_name: String,
    finalized_at: DateTime<Utc>,
}

/// Leader-local accumulator of in-flight checkpoint telemetry. Lives on
/// `ClusterManager` for the whole process -- it is NOT rebuilt when this
/// node gains or loses leadership (see the module doc for how `retain_live`
/// bounds the entries a non-leader stretch can leave behind).
pub struct CheckpointStore {
    metrics: CheckpointMetrics,
    runs: HashMap<u32, HashMap<u32, RunEntry>>,
    failed_jobs: HashMap<u32, FailedJobEntry>,
    /// Cumulative invalid-event counts by reason, per `job_id`, added to by every run
    /// attempt's deltas (see `ingest`) so the series stays monotonic across a requeue.
    failed_totals: HashMap<u32, HashMap<String, u64>>,
}

impl CheckpointStore {
    pub fn new(metrics: CheckpointMetrics) -> Self {
        Self {
            metrics,
            runs: HashMap::new(),
            failed_jobs: HashMap::new(),
            failed_totals: HashMap::new(),
        }
    }

    /// Accept or reject one node's record. `live` is the job's current run as
    /// the caller read it from `jobs` (`None` if unknown or not running); this
    /// store never takes the `jobs` lock itself.
    pub fn ingest(
        &mut self,
        node: &str,
        rec: StepCheckpointRecord,
        live: Option<RunInfo>,
        now: DateTime<Utc>,
    ) -> Result<(), Rejection> {
        let job_id = rec.job_id;
        let run_attempt = rec.run_attempt;

        let live_match = live.filter(|l| l.job_id == job_id && l.run_attempt == run_attempt);
        let stored_ended = self
            .runs
            .get(&job_id)
            .and_then(|by_attempt| by_attempt.get(&run_attempt))
            .filter(|entry| entry.ended.is_some());

        let info = match (&live_match, stored_ended) {
            (Some(live), _) => {
                if !live.nodes.iter().any(|n| n == node) {
                    return Err(Rejection::NodeNotAllocated);
                }
                live.clone()
            }
            (None, Some(entry)) => {
                if !entry.info.nodes.iter().any(|n| n == node) {
                    return Err(Rejection::NodeNotAllocated);
                }
                entry.info.clone()
            }
            (None, None) => return Err(Rejection::UnknownRun),
        };

        let by_attempt = self.runs.entry(job_id).or_default();
        let entry = by_attempt
            .entry(run_attempt)
            .or_insert_with(|| RunEntry::new(info.clone()));
        // An ended entry's `info` is frozen at `mark_ended` time; only a
        // still-live match (checked above) may refresh it.
        if live_match.is_some() {
            entry.info = info;
        }

        let total_records: usize = entry.records.values().map(HashMap::len).sum();
        let is_new_key = entry
            .records
            .get(node)
            .is_none_or(|steps| !steps.contains_key(&rec.step_id));
        if is_new_key && total_records >= MAX_RECORDS_PER_RUN {
            return Err(Rejection::OverCap);
        }

        let last = entry
            .last_dropped
            .entry((node.to_string(), rec.step_id))
            .or_default();
        let mut deltas = Vec::new();
        for (reason, &count) in &rec.dropped {
            let prev = last.get(reason).copied().unwrap_or(0);
            let delta = count.saturating_sub(prev);
            if delta > 0 {
                deltas.push((reason.clone(), delta));
            }
        }
        *last = rec.dropped.clone();

        entry
            .records
            .entry(node.to_string())
            .or_default()
            .insert(rec.step_id, rec);

        for (reason, delta) in deltas {
            if is_invalid_reason(&reason) {
                self.metrics.add_invalid(&reason, delta);
                *self
                    .failed_totals
                    .entry(job_id)
                    .or_default()
                    .entry(reason)
                    .or_insert(0) += delta;
            } else {
                self.metrics.add_dropped(&reason, delta);
            }
        }

        self.finalize_if_ready(job_id, run_attempt, now);

        Ok(())
    }

    /// Record that a run ended, from the `JobFinalized` side-effect hook.
    /// Creates the run's entry if no record has arrived yet. Falls back to a
    /// previously `note_grace`d instant when `grace_started_at` is `None`.
    pub fn mark_ended(
        &mut self,
        info: RunInfo,
        reason: &'static str,
        ended_at: DateTime<Utc>,
        grace_started_at: Option<DateTime<Utc>>,
    ) {
        let job_id = info.job_id;
        let run_attempt = info.run_attempt;
        let by_attempt = self.runs.entry(job_id).or_default();
        let entry = by_attempt
            .entry(run_attempt)
            .or_insert_with(|| RunEntry::new(info.clone()));
        entry.info = info;
        let grace_started_at = grace_started_at.or(entry.noted_grace_started_at);
        entry.ended = Some(EndedInfo {
            reason,
            ended_at,
            grace_started_at,
        });

        self.finalize_if_ready(job_id, run_attempt, ended_at);
    }

    /// Record that `job_id`'s `run_attempt` entered a preempt-grace window
    /// starting at `signaled_at`, creating a live entry (bounded by
    /// `retain_live` like any other) if this run has no entry yet.
    /// `mark_ended` uses this when its own `grace_started_at` argument is
    /// `None` -- covering `JobPreemptCancel`'s node-report-driven finalize
    /// path, which by the time it builds `JobFinalized` has already lost the
    /// job's own grace state to the reset that cleared `preempt_pending`.
    pub fn note_grace(&mut self, job_id: u32, run_attempt: u32, signaled_at: DateTime<Utc>) {
        let by_attempt = self.runs.entry(job_id).or_default();
        let entry = by_attempt.entry(run_attempt).or_insert_with(|| {
            RunEntry::new(RunInfo {
                job_id,
                run_attempt,
                nodes: Vec::new(),
                user: String::new(),
                account: String::new(),
                job_name: String::new(),
            })
        });
        entry.noted_grace_started_at = Some(signaled_at);
    }

    /// Finalize every ended run that is past its grace deadline, recording its
    /// outcome into `metrics` and dropping its state. Also prunes any retained
    /// `failed_jobs` entry older than `FAILED_SERIES_RETENTION`, dropping its
    /// `failed_totals` counterpart too unless the job is live again by then.
    pub fn sweep(&mut self, now: DateTime<Utc>) {
        let candidates: Vec<(u32, u32)> = self
            .runs
            .iter()
            .flat_map(|(job_id, by_attempt)| by_attempt.keys().map(|attempt| (*job_id, *attempt)))
            .collect();
        for (job_id, run_attempt) in candidates {
            self.finalize_if_ready(job_id, run_attempt, now);
        }
        let expired: Vec<u32> = self
            .failed_jobs
            .iter()
            .filter(|(_, entry)| now - entry.finalized_at > FAILED_SERIES_RETENTION_CHRONO)
            .map(|(job_id, _)| *job_id)
            .collect();
        for job_id in expired {
            self.failed_jobs.remove(&job_id);
            if !self.job_is_live(job_id) {
                self.failed_totals.remove(&job_id);
            }
        }
    }

    /// Per-job checkpoint gauges for jobs with a live (not yet finalized) run.
    pub fn job_snapshots(&self, now: DateTime<Utc>) -> Vec<CheckpointJobSnapshot> {
        self.runs
            .values()
            .flat_map(|by_attempt| by_attempt.values())
            .filter(|entry| entry.ended.is_none())
            .map(|entry| build_job_snapshot(entry, now))
            .collect()
    }

    /// Per-job, per-reason invalid-event counts for `spur_job_checkpoints_failed_total`,
    /// cumulative across every run_attempt the job has had (see `failed_totals`) so the series
    /// is monotonic through a requeue. A job with a live (not yet ended) run reports from its
    /// current labels; a job with no live run reports from its most recently finalized run's
    /// labels, until `FAILED_SERIES_RETENTION` after that run ended (see `sweep`). A job is
    /// never reported from both at once: a live run supersedes a retained entry for the same
    /// job, even one with no invalid events of its own right now -- the cumulative total still
    /// includes whatever an earlier, already-finalized run_attempt contributed.
    pub fn job_failed_snapshots(&self) -> Vec<CheckpointJobFailed> {
        let mut result = Vec::new();
        let mut live_jobs: HashSet<u32> = HashSet::new();
        for (job_id, by_attempt) in &self.runs {
            // A run still in its finalize grace counts as current too, so the
            // series never drops out between run end and retention.
            let Some(entry) = by_attempt
                .iter()
                .max_by_key(|(run_attempt, _)| **run_attempt)
                .map(|(_, entry)| entry)
            else {
                continue;
            };
            live_jobs.insert(*job_id);
            let Some(reasons) = self.cumulative_failed_reasons(*job_id) else {
                continue;
            };
            result.push(CheckpointJobFailed {
                job_id: *job_id,
                user: entry.info.user.clone(),
                account: entry.info.account.clone(),
                job_name: entry.info.job_name.clone(),
                reasons,
            });
        }
        for (job_id, retained) in &self.failed_jobs {
            if live_jobs.contains(job_id) {
                continue;
            }
            let Some(reasons) = self.cumulative_failed_reasons(*job_id) else {
                continue;
            };
            result.push(CheckpointJobFailed {
                job_id: *job_id,
                user: retained.user.clone(),
                account: retained.account.clone(),
                job_name: retained.job_name.clone(),
                reasons,
            });
        }
        result
    }

    /// `job_id`'s cumulative invalid-event counts by reason from `failed_totals`, sorted by
    /// reason, or `None` if it has never had one. `None` rather than an empty `Vec` so a caller
    /// building a `CheckpointJobFailed` can skip it the same way an empty reason list used to be
    /// skipped.
    fn cumulative_failed_reasons(&self, job_id: u32) -> Option<Vec<(String, u64)>> {
        let totals = self.failed_totals.get(&job_id)?;
        if totals.is_empty() {
            return None;
        }
        let mut reasons: Vec<(String, u64)> = totals
            .iter()
            .map(|(reason, count)| (reason.clone(), *count))
            .collect();
        reasons.sort_by(|a, b| a.0.cmp(&b.0));
        Some(reasons)
    }

    /// Whether `job_id` currently has any not-yet-ended run entry.
    fn job_is_live(&self, job_id: u32) -> bool {
        self.runs
            .get(&job_id)
            .is_some_and(|by_attempt| by_attempt.values().any(|entry| entry.ended.is_none()))
    }

    /// Drop every not-yet-ended entry whose `(job_id, run_attempt)` is absent
    /// from `live`; ended entries are left for `sweep` to finalize or hold.
    /// See the module doc: this bounds entries left behind by a run that
    /// ended while this node was not leader.
    pub fn retain_live(&mut self, live: &HashSet<(u32, u32)>) {
        self.runs.retain(|job_id, by_attempt| {
            by_attempt.retain(|run_attempt, entry| {
                entry.ended.is_some() || live.contains(&(*job_id, *run_attempt))
            });
            !by_attempt.is_empty()
        });
        // A run that ended while this replica was a follower never finalizes
        // here, so its job's totals would otherwise outlive the job.
        let (runs, failed_jobs) = (&self.runs, &self.failed_jobs);
        self.failed_totals
            .retain(|job_id, _| runs.contains_key(job_id) || failed_jobs.contains_key(job_id));
    }

    pub fn metrics(&self) -> &CheckpointMetrics {
        &self.metrics
    }

    /// Whether the store already holds an entry (live or ended, not yet
    /// finalized) for `(job_id, run_attempt)`. Used by
    /// `run_job_finalized_side_effects` as a fallback leadership signal when no
    /// raft handle is set at all (construction, or a test that never calls
    /// `set_raft`): recording an outcome only when ingest already created an
    /// entry here keeps an untouched job from spontaneously gaining one.
    pub fn has_entry(&self, job_id: u32, run_attempt: u32) -> bool {
        self.runs
            .get(&job_id)
            .is_some_and(|by_attempt| by_attempt.contains_key(&run_attempt))
    }

    /// Finalize the run if it is ended and `FINALIZE_GRACE` has elapsed since
    /// it ended, in which case whatever arrived is recorded as-is -- unless
    /// the run never reported a single checkpoint event (see
    /// [`has_any_event`]), in which case the whole run-end family is skipped
    /// and the run is simply dropped, uncounted. A job whose cumulative
    /// invalid-event total (`failed_totals`) is nonzero is additionally
    /// retained in `failed_jobs` (see [`Self::retain_failed_job`]),
    /// independent of `has_any_event`. Check and build happen under one
    /// borrow of the entry so there is no second lookup to (not) find it gone.
    fn finalize_if_ready(&mut self, job_id: u32, run_attempt: u32, now: DateTime<Utc>) {
        let (run_end, info) = {
            let Some(entry) = self
                .runs
                .get(&job_id)
                .and_then(|by_attempt| by_attempt.get(&run_attempt))
            else {
                return;
            };
            let Some(ended) = &entry.ended else {
                return;
            };
            if now < ended.ended_at + FINALIZE_GRACE_CHRONO {
                return;
            }
            let run_end = has_any_event(entry).then(|| build_run_end(entry, ended));
            (run_end, entry.info.clone())
        };
        if let Some(run_end) = run_end {
            self.metrics.record_run_end(&run_end);
        }
        if self
            .failed_totals
            .get(&job_id)
            .is_some_and(|totals| !totals.is_empty())
        {
            self.retain_failed_job(info, now);
        }

        if let Some(by_attempt) = self.runs.get_mut(&job_id) {
            by_attempt.remove(&run_attempt);
            if by_attempt.is_empty() {
                self.runs.remove(&job_id);
            }
        }
    }

    /// Record that `info.job_id` finalized with an invalid event, keyed by `job_id` (a later
    /// run_attempt's entry simply overwrites an earlier one's labels and timestamp -- see the
    /// `FailedJobEntry` doc). Evicts the single oldest `failed_jobs` entry by `finalized_at`,
    /// along with its `failed_totals` counterpart (unless that job is live again), when this
    /// would push `failed_jobs` over `MAX_RETAINED_FAILED_JOBS`.
    fn retain_failed_job(&mut self, info: RunInfo, now: DateTime<Utc>) {
        self.failed_jobs.insert(
            info.job_id,
            FailedJobEntry {
                user: info.user,
                account: info.account,
                job_name: info.job_name,
                finalized_at: now,
            },
        );
        if self.failed_jobs.len() > MAX_RETAINED_FAILED_JOBS {
            if let Some(oldest) = self
                .failed_jobs
                .iter()
                .min_by_key(|(_, entry)| entry.finalized_at)
                .map(|(job_id, _)| *job_id)
            {
                self.failed_jobs.remove(&oldest);
                if !self.job_is_live(oldest) {
                    self.failed_totals.remove(&oldest);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).expect("valid timestamp")
    }

    fn proto_ts(dt: DateTime<Utc>) -> prost_types::Timestamp {
        prost_types::Timestamp {
            seconds: dt.timestamp(),
            nanos: dt.timestamp_subsec_nanos() as i32,
        }
    }

    fn run_info(nodes: &[&str]) -> RunInfo {
        RunInfo {
            job_id: 7,
            run_attempt: 1,
            nodes: nodes.iter().map(|n| n.to_string()).collect(),
            user: "alice".to_string(),
            account: "acct".to_string(),
            job_name: "train".to_string(),
        }
    }

    fn rec(step_id: u32, completed: u64, is_final: bool) -> StepCheckpointRecord {
        rec_for(7, step_id, completed, is_final)
    }

    fn rec_for(job_id: u32, step_id: u32, completed: u64, is_final: bool) -> StepCheckpointRecord {
        StepCheckpointRecord {
            job_id,
            run_attempt: 1,
            step_id,
            is_final,
            completed,
            failed: 0,
            last_completed_step: None,
            last_completed_at: None,
            in_progress_step: None,
            in_progress_since: None,
            restore_step: None,
            uid_verified: 0,
            dropped: HashMap::new(),
        }
    }

    fn store() -> CheckpointStore {
        CheckpointStore::new(CheckpointMetrics::new())
    }

    // --- ingest acceptance ---

    #[test]
    fn ingest_accepts_a_record_matching_the_live_run_and_an_allocated_node() {
        let mut s = store();
        let live = run_info(&["n1", "n2"]);
        assert_eq!(s.ingest("n1", rec(0, 1, false), Some(live), at(0)), Ok(()));
    }

    #[test]
    fn ingest_rejects_unknown_run_when_no_live_or_ended_entry_matches() {
        let mut s = store();
        assert_eq!(
            s.ingest("n1", rec(0, 1, false), None, at(0)),
            Err(Rejection::UnknownRun)
        );
    }

    #[test]
    fn ingest_rejects_a_node_not_in_the_live_runs_allocation() {
        let mut s = store();
        let live = run_info(&["n1"]);
        assert_eq!(
            s.ingest("n2", rec(0, 1, false), Some(live), at(0)),
            Err(Rejection::NodeNotAllocated)
        );
    }

    #[test]
    fn ingest_accepts_a_late_record_against_a_just_ended_entrys_allocation() {
        let mut s = store();
        s.mark_ended(run_info(&["n1", "n2"]), "completed", at(0), None);
        // No `live` this time -- the job isn't running anymore -- but the
        // ended entry's allocation still names n2.
        assert_eq!(s.ingest("n2", rec(0, 1, true), None, at(1)), Ok(()));
    }

    #[test]
    fn ingest_rejects_a_node_not_in_an_ended_entrys_allocation() {
        let mut s = store();
        s.mark_ended(run_info(&["n1"]), "completed", at(0), None);
        assert_eq!(
            s.ingest("n2", rec(0, 1, true), None, at(1)),
            Err(Rejection::NodeNotAllocated)
        );
    }

    #[test]
    fn ingest_rejects_a_record_for_a_finalized_and_dropped_run() {
        let mut s = store();
        s.mark_ended(run_info(&["n1"]), "completed", at(0), None);
        s.ingest("n1", rec(0, 1, true), None, at(1)).unwrap();
        // Finalize past the grace.
        s.sweep(at(0) + FINALIZE_GRACE_CHRONO);
        // The run is now gone; a further record for it is UnknownRun.
        assert_eq!(
            s.ingest("n1", rec(0, 2, true), None, at(2)),
            Err(Rejection::UnknownRun)
        );
    }

    #[test]
    fn ingest_rejects_records_past_the_per_run_cap() {
        let mut s = store();
        let live = run_info(&["n1"]);
        for step in 0..MAX_RECORDS_PER_RUN as u32 {
            s.ingest("n1", rec(step, 1, false), Some(live.clone()), at(0))
                .unwrap();
        }
        assert_eq!(
            s.ingest(
                "n1",
                rec(MAX_RECORDS_PER_RUN as u32, 1, false),
                Some(live.clone()),
                at(0)
            ),
            Err(Rejection::OverCap)
        );
        // An update to an already-stored key is not a new key, so it is not
        // capped even once the run is "full".
        assert_eq!(s.ingest("n1", rec(0, 2, false), Some(live), at(0)), Ok(()));
    }

    // --- merge rules ---

    #[test]
    fn merge_does_not_double_count_two_nodes_reporting_the_same_step() {
        let mut s = store();
        let live = run_info(&["n1", "n2"]);
        let mut r1 = rec(0, 3, true);
        r1.last_completed_step = Some(30);
        r1.last_completed_at = Some(proto_ts(at(5)));
        let mut r2 = rec(0, 3, true);
        r2.last_completed_step = Some(30);
        r2.last_completed_at = Some(proto_ts(at(5)));
        s.ingest("n1", r1, Some(live.clone()), at(0)).unwrap();
        s.ingest("n2", r2, Some(live), at(0)).unwrap();

        s.mark_ended(run_info(&["n1", "n2"]), "completed", at(10), None);
        s.sweep(at(10) + FINALIZE_GRACE_CHRONO);
        let body = render(&s);
        // One step reported identically by two nodes contributes 3, not 6.
        assert!(body.contains(r#"reason="completed",checkpoint="completed"} 1"#));
    }

    #[test]
    fn merge_sums_across_distinct_job_steps() {
        let mut s = store();
        let live = run_info(&["n1"]);
        s.ingest("n1", rec(0, 2, true), Some(live.clone()), at(0))
            .unwrap();
        s.ingest("n1", rec(1, 3, true), Some(live), at(0)).unwrap();
        s.mark_ended(run_info(&["n1"]), "completed", at(10), None);
        s.sweep(at(10) + FINALIZE_GRACE_CHRONO);
        let snap = completed_total(&s);
        assert_eq!(snap, 5);
    }

    #[test]
    fn merge_in_progress_beats_completed_when_step_exceeds_last_completed() {
        let mut s = store();
        let live = run_info(&["n1"]);
        let mut r = rec(0, 1, true);
        r.last_completed_step = Some(10);
        r.in_progress_step = Some(20);
        s.ingest("n1", r, Some(live), at(0)).unwrap();
        s.mark_ended(run_info(&["n1"]), "completed", at(10), None);
        s.sweep(at(10) + FINALIZE_GRACE_CHRONO);
        assert!(render(&s).contains(r#"checkpoint="in_progress"} 1"#));
    }

    #[test]
    fn merge_completed_when_no_in_progress_step_exceeds_last_completed() {
        let mut s = store();
        let live = run_info(&["n1"]);
        let mut r = rec(0, 1, true);
        r.last_completed_step = Some(10);
        r.in_progress_step = Some(5);
        s.ingest("n1", r, Some(live), at(0)).unwrap();
        s.mark_ended(run_info(&["n1"]), "completed", at(10), None);
        s.sweep(at(10) + FINALIZE_GRACE_CHRONO);
        assert!(render(&s).contains(r#"checkpoint="completed"} 1"#));
    }

    #[test]
    fn merge_none_for_a_failed_only_run_that_had_events() {
        // The run had an event (a failed checkpoint), just never one that
        // completed or was in progress -- still recorded, per the decision
        // that a failed-only run keeps its current labeling.
        let mut s = store();
        let live = run_info(&["n1"]);
        let mut r = rec(0, 0, true);
        r.failed = 1;
        s.ingest("n1", r, Some(live), at(0)).unwrap();
        s.mark_ended(run_info(&["n1"]), "failed", at(10), None);
        s.sweep(at(10) + FINALIZE_GRACE_CHRONO);
        assert!(render(&s).contains(r#"reason="failed",checkpoint="none"} 1"#));
    }

    #[test]
    fn a_run_with_no_checkpoint_events_is_never_recorded() {
        let mut s = store();
        let live = run_info(&["n1"]);
        s.ingest("n1", rec(0, 0, true), Some(live), at(0)).unwrap();
        s.mark_ended(run_info(&["n1"]), "failed", at(10), None);
        s.sweep(at(10) + FINALIZE_GRACE_CHRONO);
        let body = render(&s);
        assert!(!body.contains("spur_job_runs_ended_total"));
        assert!(!body.contains("spur_job_run_end_seconds_since_last_checkpoint"));
        assert!(!body.contains("spur_job_run_checkpoints_completed"));
    }

    // --- grace outcomes ---

    #[test]
    fn grace_outcome_completed_when_last_checkpoint_at_or_after_grace_start() {
        let mut s = store();
        let live = run_info(&["n1"]);
        let mut r = rec(0, 1, true);
        r.last_completed_step = Some(10);
        r.last_completed_at = Some(proto_ts(at(5)));
        s.ingest("n1", r, Some(live), at(0)).unwrap();
        s.mark_ended(run_info(&["n1"]), "preempted", at(10), Some(at(5)));
        s.sweep(at(10) + FINALIZE_GRACE_CHRONO);
        assert!(render(&s).contains(r#"outcome="completed"} 1"#));
    }

    #[test]
    fn grace_outcome_failed_when_a_record_failed_and_none_completed_in_grace() {
        let mut s = store();
        let live = run_info(&["n1"]);
        let mut r = rec(0, 0, true);
        r.failed = 1;
        s.ingest("n1", r, Some(live), at(0)).unwrap();
        s.mark_ended(run_info(&["n1"]), "preempted", at(10), Some(at(5)));
        s.sweep(at(10) + FINALIZE_GRACE_CHRONO);
        assert!(render(&s).contains(r#"outcome="failed"} 1"#));
    }

    #[test]
    fn grace_outcome_none_when_grace_started_but_nothing_happened() {
        let mut s = store();
        s.mark_ended(run_info(&["n1"]), "preempted", at(10), Some(at(5)));
        // The run had an event (a restore), just nothing that completed or
        // was in progress during or after the grace window.
        let mut r = rec(0, 0, true);
        r.restore_step = Some(1);
        s.ingest("n1", r, None, at(10)).unwrap();
        s.sweep(at(10) + FINALIZE_GRACE_CHRONO);
        assert!(render(&s).contains(r#"outcome="none"} 1"#));
    }

    #[test]
    fn grace_outcome_absent_when_run_never_entered_a_grace_window() {
        let mut s = store();
        s.mark_ended(run_info(&["n1"]), "completed", at(10), None);
        s.ingest("n1", rec(0, 1, true), None, at(10)).unwrap();
        s.sweep(at(10) + FINALIZE_GRACE_CHRONO);
        assert!(!render(&s).contains("spur_preempt_grace_checkpoint_total"));
    }

    // --- drops delta ---

    #[test]
    fn resend_of_same_cumulative_dropped_adds_zero_delta() {
        let mut s = store();
        let live = run_info(&["n1"]);
        let mut r = rec(0, 0, false);
        r.dropped = HashMap::from([("busy".to_string(), 3u64)]);
        s.ingest("n1", r.clone(), Some(live.clone()), at(0))
            .unwrap();
        assert!(render(&s).contains(r#"reason="busy"} 3"#));

        // Same cumulative value resent: no further increment.
        s.ingest("n1", r, Some(live.clone()), at(1)).unwrap();
        assert!(render(&s).contains(r#"reason="busy"} 3"#));

        // A higher cumulative value adds only the delta.
        let mut r2 = rec(0, 0, false);
        r2.dropped = HashMap::from([("busy".to_string(), 5u64)]);
        s.ingest("n1", r2, Some(live), at(2)).unwrap();
        assert!(render(&s).contains(r#"reason="busy"} 5"#));
    }

    #[test]
    fn ingest_routes_an_invalid_reason_to_the_failed_family_and_the_rest_to_dropped() {
        let mut s = store();
        let live = run_info(&["n1"]);
        let mut r = rec(0, 0, false);
        r.dropped = HashMap::from([
            ("job_mismatch".to_string(), 2u64), // invalid family
            ("busy".to_string(), 1u64),         // dropped family
        ]);
        s.ingest("n1", r, Some(live), at(0)).unwrap();
        let body = render(&s);
        assert!(body.contains(r#"spur_job_run_checkpoints_failed_total{reason="job_mismatch"} 2"#));
        assert!(body.contains(r#"spur_checkpoint_events_dropped_total{reason="busy"} 1"#));
    }

    // --- restored ---

    #[test]
    fn a_run_reporting_a_restore_counts_on_the_restored_family() {
        let mut s = store();
        let live = run_info(&["n1"]);
        let mut r = rec(0, 0, true);
        r.restore_step = Some(5);
        s.ingest("n1", r, Some(live.clone()), at(0)).unwrap();
        s.mark_ended(live, "completed", at(10), None);
        s.sweep(at(10) + FINALIZE_GRACE_CHRONO);
        assert!(render(&s).contains(r#"spur_job_runs_restored_total{reason="completed"} 1"#));
    }

    #[test]
    fn a_run_without_a_restore_does_not_count_on_the_restored_family() {
        let mut s = store();
        let live = run_info(&["n1"]);
        s.ingest("n1", rec(0, 1, true), Some(live.clone()), at(0))
            .unwrap();
        s.mark_ended(live, "completed", at(10), None);
        s.sweep(at(10) + FINALIZE_GRACE_CHRONO);
        assert!(!render(&s).contains("spur_job_runs_restored_total"));
    }

    // --- finalize grace ---

    #[test]
    fn does_not_finalize_before_the_grace_even_when_every_node_sent_a_final_record() {
        let mut s = store();
        let live = run_info(&["n1", "n2"]);
        s.ingest("n1", rec(0, 1, true), Some(live.clone()), at(0))
            .unwrap();
        s.ingest("n2", rec(0, 1, true), Some(live), at(0)).unwrap();
        s.mark_ended(run_info(&["n1", "n2"]), "completed", at(1), None);
        // Every node already sent a final record -- no early-finalize
        // shortcut: the run still waits out the grace.
        assert!(!render(&s).contains("spur_job_runs_ended_total"));
        s.sweep(at(1) + FINALIZE_GRACE_CHRONO - chrono::Duration::seconds(1));
        assert!(!render(&s).contains("spur_job_runs_ended_total"));
        s.sweep(at(1) + FINALIZE_GRACE_CHRONO);
        assert!(render(&s).contains(r#"reason="completed",checkpoint="completed"} 1"#));
    }

    #[test]
    fn a_late_record_within_the_grace_is_still_counted_in_the_outcome() {
        let mut s = store();
        let live = run_info(&["n1"]);
        // A short srun step's final record arrives first, from its only node --
        // the old completeness shortcut would have finalized (and dropped) the
        // run right here, on `mark_ended` below.
        s.ingest("n1", rec(1, 2, true), Some(live), at(0)).unwrap();
        s.mark_ended(run_info(&["n1"]), "completed", at(1), None);
        // The batch step's own final record lands within the grace.
        s.ingest("n1", rec(0, 3, true), None, at(5)).unwrap();
        s.sweep(at(1) + FINALIZE_GRACE_CHRONO);
        // Both steps' completed counts are summed into the outcome.
        assert_eq!(completed_total(&s), 5);
    }

    #[test]
    fn waits_for_every_node_before_the_grace_deadline() {
        let mut s = store();
        let live = run_info(&["n1", "n2"]);
        s.ingest("n1", rec(0, 1, true), Some(live), at(0)).unwrap();
        s.mark_ended(run_info(&["n1", "n2"]), "completed", at(1), None);
        s.sweep(at(1) + FINALIZE_GRACE_CHRONO - chrono::Duration::seconds(1));
        assert!(
            !render(&s).contains("spur_job_runs_ended_total"),
            "n2 never reported; run must not finalize before the grace deadline"
        );
        // Still held (not finalized-and-dropped): a late record for the
        // missing node is still accepted.
        assert_eq!(s.ingest("n2", rec(0, 1, true), None, at(1)), Ok(()));
    }

    #[test]
    fn finalizes_at_the_grace_deadline_with_partial_data() {
        let mut s = store();
        let live = run_info(&["n1", "n2"]);
        s.ingest("n1", rec(0, 1, true), Some(live), at(0)).unwrap();
        s.mark_ended(run_info(&["n1", "n2"]), "completed", at(1), None);
        s.sweep(at(1) + FINALIZE_GRACE_CHRONO);
        assert!(render(&s).contains(r#"reason="completed",checkpoint="completed"} 1"#));
        // Finalized and dropped: a further record for it is now unknown.
        assert_eq!(
            s.ingest("n2", rec(0, 1, true), None, at(1)),
            Err(Rejection::UnknownRun)
        );
    }

    // --- job_snapshots ---

    #[test]
    fn job_snapshot_reports_only_the_dropped_family_count() {
        let mut s = store();
        let live = run_info(&["n1"]);
        let mut r = rec(0, 0, false);
        r.dropped = HashMap::from([
            ("job_mismatch".to_string(), 3u64), // invalid family -- not counted here
            ("busy".to_string(), 2u64),         // dropped family
        ]);
        s.ingest("n1", r, Some(live), at(0)).unwrap();
        let snaps = s.job_snapshots(at(1));
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].dropped, 2);
    }

    #[test]
    fn job_snapshots_only_includes_live_not_yet_finalized_runs() {
        let mut s = store();
        let live = run_info(&["n1"]);
        s.ingest("n1", rec(0, 1, false), Some(live), at(0)).unwrap();

        let other = RunInfo {
            job_id: 9,
            ..run_info(&["n2"])
        };
        s.mark_ended(other.clone(), "completed", at(1), None);
        s.ingest("n2", rec_for(9, 0, 1, true), None, at(1)).unwrap();

        let snaps = s.job_snapshots(at(2));
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].job_id, 7);
    }

    // --- job_failed_snapshots ---

    #[test]
    fn job_failed_snapshots_reports_per_reason_counts_with_all_labels_for_a_live_run() {
        let mut s = store();
        let live = run_info(&["n1"]); // account="acct", job_name="train"
        let mut r = rec(0, 0, false);
        r.dropped = HashMap::from([
            ("job_mismatch".to_string(), 3u64),
            ("oversize".to_string(), 1u64),
            ("busy".to_string(), 10u64), // dropped family -- must never appear here
        ]);
        s.ingest("n1", r, Some(live), at(0)).unwrap();

        let snaps = s.job_failed_snapshots();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].job_id, 7);
        assert_eq!(snaps[0].user, "alice");
        assert_eq!(snaps[0].account, "acct");
        assert_eq!(snaps[0].job_name, "train");
        assert_eq!(
            snaps[0].reasons,
            vec![("job_mismatch".to_string(), 3), ("oversize".to_string(), 1),]
        );
    }

    #[test]
    fn job_failed_snapshots_omits_a_job_with_no_invalid_events() {
        let mut s = store();
        let live = run_info(&["n1"]);
        let mut r = rec(0, 0, false);
        r.dropped = HashMap::from([("busy".to_string(), 2u64)]); // dropped family only
        s.ingest("n1", r, Some(live), at(0)).unwrap();
        assert!(s.job_failed_snapshots().is_empty());
    }

    #[test]
    fn job_failed_snapshots_retains_a_finalized_runs_counts_past_the_runs_own_lifetime() {
        let mut s = store();
        let live = run_info(&["n1"]);
        let mut r = rec(0, 1, true);
        r.dropped = HashMap::from([("job_mismatch".to_string(), 1u64)]);
        s.ingest("n1", r, Some(live.clone()), at(0)).unwrap();
        s.mark_ended(live, "completed", at(1), None);
        // Past the grace, the run finalizes.
        s.sweep(at(1) + FINALIZE_GRACE_CHRONO);

        // The live run is gone (job_snapshots no longer reports it)...
        assert!(s.job_snapshots(at(1)).is_empty());
        // ...but the failed-job entry survives, retained.
        let snaps = s.job_failed_snapshots();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].reasons, vec![("job_mismatch".to_string(), 1)]);
    }

    #[test]
    fn job_failed_snapshots_expire_after_retention_via_sweep() {
        let mut s = store();
        let live = run_info(&["n1"]);
        let mut r = rec(0, 1, true);
        r.dropped = HashMap::from([("job_mismatch".to_string(), 1u64)]);
        s.ingest("n1", r, Some(live.clone()), at(0)).unwrap();
        s.mark_ended(live, "completed", at(1), None);
        // `finalized_at` is the `now` the run actually finalized at, not when it ended.
        let finalized_at = at(1) + FINALIZE_GRACE_CHRONO;
        s.sweep(finalized_at);
        assert_eq!(s.job_failed_snapshots().len(), 1);

        // At the retention boundary, the entry is still visible.
        s.sweep(finalized_at + FAILED_SERIES_RETENTION_CHRONO);
        assert_eq!(s.job_failed_snapshots().len(), 1);

        // Past it, sweep prunes it.
        s.sweep(finalized_at + FAILED_SERIES_RETENTION_CHRONO + chrono::Duration::seconds(1));
        assert!(s.job_failed_snapshots().is_empty());
    }

    #[test]
    fn retain_failed_job_evicts_the_oldest_entry_past_the_cap() {
        let mut s = store();
        for job_id in 0..=(MAX_RETAINED_FAILED_JOBS as u32) {
            let info = RunInfo {
                job_id,
                ..run_info(&["n1"])
            };
            s.retain_failed_job(info, at(job_id as i64));
        }
        assert_eq!(s.failed_jobs.len(), MAX_RETAINED_FAILED_JOBS);
        // job_id 0 had the oldest `finalized_at` and was evicted first.
        assert!(!s.failed_jobs.contains_key(&0));
        assert!(s
            .failed_jobs
            .contains_key(&(MAX_RETAINED_FAILED_JOBS as u32)));
    }

    #[test]
    fn a_new_run_attempt_adds_to_the_same_jobs_cumulative_invalid_count() {
        let mut s = store();
        // run_attempt 1 ends with 2 invalid events, retained in failed_jobs.
        let live1 = run_info(&["n1"]);
        let mut r1 = rec(0, 1, true);
        r1.dropped = HashMap::from([("job_mismatch".to_string(), 2u64)]);
        s.ingest("n1", r1, Some(live1.clone()), at(0)).unwrap();
        s.mark_ended(live1, "completed", at(1), None);
        s.sweep(at(1) + FINALIZE_GRACE_CHRONO);
        assert_eq!(
            s.job_failed_snapshots()[0].reasons,
            vec![("job_mismatch".to_string(), 2)]
        );

        // run_attempt 2 is now live and adds one more of the same reason.
        let live2 = RunInfo {
            run_attempt: 2,
            ..run_info(&["n1"])
        };
        let mut r2 = rec(0, 0, false);
        r2.run_attempt = 2;
        r2.dropped = HashMap::from([("job_mismatch".to_string(), 1u64)]);
        s.ingest("n1", r2, Some(live2), at(2)).unwrap();

        // The cumulative series reads 3 -- never resets to 1, and never sticks
        // at the first attempt's own count of 2.
        let snaps = s.job_failed_snapshots();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].reasons, vec![("job_mismatch".to_string(), 3)]);
    }

    #[test]
    fn a_run_in_its_finalize_grace_stays_on_the_per_job_failed_counter() {
        let mut s = store();
        let live = run_info(&["n1"]);
        let mut r = rec(0, 0, true);
        r.dropped = HashMap::from([("unknown_kind".to_string(), 1u64)]);
        s.ingest("n1", r, Some(live.clone()), at(0)).unwrap();
        s.mark_ended(live, "completed", at(1), None);
        s.sweep(at(2));

        let snaps = s.job_failed_snapshots();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].reasons, vec![("unknown_kind".to_string(), 1)]);
    }

    #[test]
    fn retain_live_drops_the_totals_of_a_job_that_left_without_finalizing() {
        let mut s = store();
        let live = run_info(&["n1"]);
        let mut r = rec(0, 0, false);
        r.dropped = HashMap::from([("job_mismatch".to_string(), 1u64)]);
        s.ingest("n1", r, Some(live), at(0)).unwrap();
        assert_eq!(s.job_failed_snapshots().len(), 1);

        s.retain_live(&HashSet::new());

        assert!(s.job_failed_snapshots().is_empty());
        assert!(s.failed_totals.is_empty());
    }

    #[test]
    fn checkpoint_job_failed_render_sanitizes_and_truncates_job_name() {
        let mut s = store();
        let raw_name = format!("bad\x07name{}", "y".repeat(100));
        let live = RunInfo {
            job_name: raw_name,
            ..run_info(&["n1"])
        };
        let mut r = rec(0, 0, false);
        r.dropped = HashMap::from([("malformed_json".to_string(), 1u64)]);
        s.ingest("n1", r, Some(live), at(0)).unwrap();

        let body = spur_metrics::encode_checkpoint_job_failed_metrics(&s.job_failed_snapshots());
        // The control character becomes `_`, and the whole value is capped at 64 chars:
        // "bad_name" (8 chars) + 56 of the 100 `y`s.
        let expected_name = format!("bad_name{}", "y".repeat(56));
        assert_eq!(expected_name.len(), 64);
        assert!(body.contains(&format!("job_name=\"{expected_name}\"")));
    }

    // --- note_grace ---

    #[test]
    fn note_grace_backfills_mark_ended_when_its_own_grace_is_none() {
        let mut s = store();
        s.note_grace(7, 1, at(5));
        let live = run_info(&["n1"]);
        let mut r = rec(0, 1, true);
        r.last_completed_step = Some(10);
        r.last_completed_at = Some(proto_ts(at(5)));
        s.ingest("n1", r, Some(live), at(0)).unwrap();
        s.mark_ended(run_info(&["n1"]), "preempted", at(10), None);
        s.sweep(at(10) + FINALIZE_GRACE_CHRONO);
        assert!(render(&s).contains(r#"outcome="completed"} 1"#));
    }

    #[test]
    fn mark_ended_s_own_grace_started_at_wins_over_a_noted_one() {
        let mut s = store();
        // Noted grace starts later than any completed checkpoint, which would
        // read as `Failed`/`None`; mark_ended's own earlier instant must win.
        s.note_grace(7, 1, at(20));
        let live = run_info(&["n1"]);
        let mut r = rec(0, 1, true);
        r.last_completed_at = Some(proto_ts(at(5)));
        s.ingest("n1", r, Some(live), at(0)).unwrap();
        s.mark_ended(run_info(&["n1"]), "preempted", at(10), Some(at(5)));
        s.sweep(at(10) + FINALIZE_GRACE_CHRONO);
        assert!(render(&s).contains(r#"outcome="completed"} 1"#));
    }

    // --- retain_live ---

    #[test]
    fn retain_live_drops_a_not_ended_entry_absent_from_the_live_set() {
        let mut s = store();
        let live = run_info(&["n1"]);
        s.ingest("n1", rec(0, 1, false), Some(live), at(0)).unwrap();
        s.retain_live(&HashSet::new());
        assert!(s.job_snapshots(at(1)).is_empty());
        // Gone, not merely hidden: a further record for it is unknown.
        assert_eq!(
            s.ingest("n1", rec(0, 1, false), None, at(1)),
            Err(Rejection::UnknownRun)
        );
    }

    #[test]
    fn retain_live_keeps_a_not_ended_entry_present_in_the_live_set() {
        let mut s = store();
        let live = run_info(&["n1"]);
        s.ingest("n1", rec(0, 1, false), Some(live), at(0)).unwrap();
        s.retain_live(&HashSet::from([(7u32, 1u32)]));
        assert_eq!(s.job_snapshots(at(1)).len(), 1);
    }

    #[test]
    fn retain_live_keeps_an_ended_entry_regardless_of_the_live_set() {
        let mut s = store();
        // n2 never reports, so the entry stays held (not yet finalized).
        s.mark_ended(run_info(&["n1", "n2"]), "completed", at(0), None);
        s.retain_live(&HashSet::new());
        // Still present: a late record for it is still accepted.
        assert_eq!(s.ingest("n1", rec(0, 1, true), None, at(1)), Ok(()));
    }

    // --- test helpers ---

    fn render(s: &CheckpointStore) -> String {
        spur_metrics::encode_checkpoint_metrics(s.metrics())
    }

    fn completed_total(s: &CheckpointStore) -> u64 {
        // Recover the summed `checkpoints_completed` via the rendered
        // histogram's sum line rather than re-deriving it independently.
        let body = render(s);
        let line = body
            .lines()
            .find(|l| l.starts_with("spur_job_run_checkpoints_completed_sum"))
            .expect("histogram sum line");
        line.rsplit(' ')
            .next()
            .expect("value")
            .parse::<f64>()
            .expect("numeric") as u64
    }
}
