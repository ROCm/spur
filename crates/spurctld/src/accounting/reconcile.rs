// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use sqlx::PgPool;
use tracing::{error, info, warn};

use spur_core::job::{Job, JobId, JobState};

use crate::cluster::{ClusterManager, JobFilter};
use crate::raft::RaftHandle;

use super::db::{self, AccountingRowState};

/// How often the reconcile pass runs. Terminal-job eviction floors its
/// effective retention above this so a job's DB row can always be repaired first.
pub const RECONCILE_INTERVAL_SECS: u64 = 120;

/// Periodically re-issue accounting writes for jobs whose accounting DB
/// record is missing or stale relative to the in-memory job store. Closes
/// the gap left by a `notify_job_start`/`notify_job_end` write that
/// exhausted its retries (see `notifier.rs`).
pub fn spawn_loop(
    pool: PgPool,
    cluster: Arc<ClusterManager>,
    raft: Arc<RaftHandle>,
    interval: Duration,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            // Consensus-backed check, not the cheap cached `is_leader()`:
            // reconciliation writes bypass Raft entirely (they go straight to
            // Postgres), so a partitioned former leader with a stale local
            // view must not keep resyncing accounting state against a job
            // store the rest of the cluster has moved on from. Residual risk:
            // this only guards the start of a pass — leadership can still
            // change during the writes `run_once` issues below.
            if !raft.ensure_leader().await {
                continue;
            }
            run_once(&pool, &cluster).await;
        }
    });
}

/// Periodically delete `txn` audit rows older than `retention_days`. Like
/// reconcile, this write bypasses Raft (straight to Postgres), so it uses the
/// consensus-backed leader check rather than the cached `is_leader()`.
pub fn spawn_txn_purge_loop(
    pool: PgPool,
    raft: Arc<RaftHandle>,
    retention_days: u32,
    interval: Duration,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            if !raft.ensure_leader().await {
                continue;
            }
            let cutoff = Utc::now() - chrono::Duration::days(retention_days as i64);
            match db::purge_txn(&pool, cutoff).await {
                Ok(n) if n > 0 => info!(removed = n, "purged expired txn audit rows"),
                Ok(_) => {}
                Err(e) => warn!(error = %e, "txn audit purge failed"),
            }
        }
    });
}

async fn run_once(pool: &PgPool, cluster: &ClusterManager) {
    let candidates: Vec<Job> = cluster
        .get_jobs(&JobFilter::default())
        .into_iter()
        .filter(is_accounted_job)
        .collect();
    if candidates.is_empty() {
        return;
    }

    let job_ids: Vec<JobId> = candidates.iter().map(|j| j.job_id).collect();
    let (accounting_states, unknown): (HashMap<JobId, AccountingRowState>, HashSet<JobId>) =
        match db::job_accounting_states(pool, &job_ids).await {
            Ok(rows) => (rows, HashSet::new()),
            Err(e) => {
                // A failed read is not evidence the rows are absent. Treating
                // it as "missing" would make resync_job call write_start,
                // which unconditionally resets a possibly-correct terminal
                // record to RUNNING — so every candidate is excluded from
                // resync this pass and picked up again on the next one.
                warn!(error = %e, "reconciliation: failed to query accounting state, skipping this cycle");
                (
                    HashMap::new(),
                    candidates.iter().map(|j| j.job_id).collect(),
                )
            }
        };

    let stale: HashSet<JobId> = jobs_needing_resync(&candidates, &accounting_states, &unknown)
        .into_iter()
        .collect();

    if stale.is_empty() {
        return;
    }
    info!(
        count = stale.len(),
        "reconciliation: resyncing jobs with missing or stale accounting records"
    );

    for job in candidates.iter().filter(|j| stale.contains(&j.job_id)) {
        resync_job(pool, job).await;
    }
}

fn is_accounted_job(job: &Job) -> bool {
    job.start_time.is_some() || job.state.is_finalized()
}

fn needs_metadata_backfill(job: &Job, row: &AccountingRowState) -> bool {
    let expected_gpus = spur_core::job::effective_gpus(&job.spec, job.spec.num_nodes) as u32;
    row.missing_metadata
        || row
            .run_attempt
            .is_some_and(|attempt| attempt != job.run_attempt)
        || (job.start_time.is_some() != row.has_start_time)
        || (expected_gpus > 0 && row.total_gpus == 0)
}

/// What accounting should show for a job's current state. Accounting only
/// ever models `RUNNING` or a finalized state (`record_job_start` always
/// writes `RUNNING`; only `record_job_end` writes a finalized state), so
/// in-memory states it has no representation for — `SUSPENDED`, `COMPLETING`
/// — must map to `RUNNING` for comparison. Otherwise a suspended or
/// completing job would be flagged stale on every single pass for as long as
/// it stays in that state. Uses `is_finalized()`, not `is_terminal()`, so a
/// durably `Preempted` job (which `is_terminal()` excludes but which can get
/// stranded there on a partial-proposal failure) is compared against its own
/// state rather than against `RUNNING`. Finalized jobs use the end-of-run
/// state, as the live end write does; `end_of_run_state` ignores the state it
/// is given, so it must only see finalized jobs.
fn accounting_state(job: &Job) -> JobState {
    if job.state.is_finalized() {
        job.end_of_run_state(job.state)
    } else {
        JobState::Running
    }
}

/// Diff in-memory job state against known accounting state. Pure so it can
/// be unit tested without a database. `unknown` holds jobs whose accounting
/// read failed this pass — distinct from a confirmed-absent row, so they're
/// skipped rather than treated as missing.
fn jobs_needing_resync(
    expected: &[Job],
    accounting: &HashMap<JobId, AccountingRowState>,
    unknown: &HashSet<JobId>,
) -> Vec<JobId> {
    expected
        .iter()
        .filter(|job| !unknown.contains(&job.job_id))
        .filter(|job| match accounting.get(&job.job_id) {
            None => true,
            Some(row) => {
                needs_metadata_backfill(job, row) || row.state != accounting_state(job).display()
            }
        })
        .map(|job| job.job_id)
        .collect()
}

/// Keep metadata repair and finalization atomic so a failed end write cannot
/// leave a finished job appearing to run or charge usage for a partial repair.
async fn resync_job(pool: &PgPool, job: &Job) {
    let is_finalized = job.state.is_finalized();

    let attempt = async {
        let mut tx = pool.begin().await?;
        if !is_finalized && job.start_time.is_some() {
            write_start(&mut tx, job).await?;
        }
        if is_finalized {
            write_end(&mut tx, job).await?;
        }
        tx.commit().await?;
        anyhow::Ok(())
    };

    match tokio::time::timeout(super::notifier::ATTEMPT_TIMEOUT, attempt).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            error!(job_id = job.job_id, error = %e, "reconciliation: failed to resync job accounting record");
        }
        Err(_) => {
            error!(
                job_id = job.job_id,
                "reconciliation: resync timed out, will retry next cycle"
            );
        }
    }
}

async fn write_start(conn: &mut sqlx::PgConnection, job: &Job) -> anyhow::Result<()> {
    let spec = &job.spec;
    let memory_mb = job
        .allocated_resources
        .as_ref()
        .map(|r| r.memory_mb)
        .unwrap_or(0);
    let start_time = job
        .start_time
        .ok_or_else(|| anyhow::anyhow!("job {} has not started", job.job_id))?;
    db::record_job_start(
        conn,
        &db::JobStartRecord {
            job_id: job.job_id,
            run_attempt: Some(job.run_attempt),
            name: spec.name.clone(),
            user: spec.user.clone(),
            account: spec.account.clone().unwrap_or_default(),
            partition: spec.partition.clone().unwrap_or_default(),
            qos: spec.qos.clone().unwrap_or_default(),
            num_nodes: spec.num_nodes,
            num_tasks: spec.num_tasks,
            cpus_per_task: spec.cpus_per_task,
            // spec.num_nodes is already normalised to effective_num_nodes() at
            // submit time (cluster.rs:821), so it equals the allocated count.
            total_gpus: spur_core::job::effective_gpus(spec, spec.num_nodes) as u32,
            memory_mb,
            submit_time: job.submit_time,
            start_time,
            reservation: spec.reservation.clone(),
            nodelist: spur_core::hostlist::compress(&job.allocated_nodes),
            idle_fill: job.idle_fill,
            // Seconds granularity preserves sub-minute limits like --time=0:00:30.
            time_limit_secs: spec.time_limit.map(|d| d.num_seconds()),
        },
    )
    .await
}

async fn write_end(conn: &mut sqlx::PgConnection, job: &Job) -> anyhow::Result<()> {
    let state = accounting_state(job);
    db::record_job_end(
        conn,
        &db::JobEndRecord::for_job(job, state, job.exit_code.unwrap_or(0)),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_core::job::JobSpec;
    use sqlx::Row;

    fn synced_row(state: &str) -> AccountingRowState {
        AccountingRowState {
            state: state.to_string(),
            run_attempt: Some(0),
            missing_metadata: false,
            has_start_time: true,
            total_gpus: 0,
        }
    }

    fn no_unknown() -> HashSet<JobId> {
        HashSet::new()
    }

    #[test]
    fn reconciliation_includes_unstarted_terminal_jobs_but_not_pending_jobs() {
        for state in [JobState::Cancelled, JobState::Deadline, JobState::Failed] {
            let mut job = test_job(1, state);
            job.start_time = None;
            assert!(is_accounted_job(&job));
        }
        let mut pending = test_job(1, JobState::Pending);
        pending.start_time = None;
        assert!(!is_accounted_job(&pending));
    }

    #[test]
    fn an_unstarted_terminal_row_converges_without_a_start_time() {
        let mut job = test_job(1, JobState::Cancelled);
        job.start_time = None;
        let expected = vec![job];
        let mut row = synced_row("CANCELLED");
        row.has_start_time = false;
        row.missing_metadata = true;
        let mut accounting = HashMap::from([(1, row)]);
        assert_eq!(
            jobs_needing_resync(&expected, &accounting, &no_unknown()),
            vec![1]
        );
        accounting.get_mut(&1).unwrap().missing_metadata = false;
        assert!(jobs_needing_resync(&expected, &accounting, &no_unknown()).is_empty());
    }

    #[test]
    fn a_missing_or_stale_start_time_requires_repair() {
        let mut job = test_job(1, JobState::Cancelled);
        let mut row = synced_row("CANCELLED");
        row.has_start_time = false;
        assert!(needs_metadata_backfill(&job, &row));
        job.start_time = None;
        row.has_start_time = true;
        assert!(needs_metadata_backfill(&job, &row));
    }

    #[test]
    fn reconciliation_detects_a_different_run_with_the_same_state() {
        let mut job = test_job(1, JobState::Completed);
        job.run_attempt = 2;
        let mut row = synced_row("COMPLETED");
        row.run_attempt = Some(1);
        assert!(needs_metadata_backfill(&job, &row));
        row.run_attempt = Some(2);
        assert!(!needs_metadata_backfill(&job, &row));
    }

    #[test]
    fn jobs_needing_resync_flags_missing_job() {
        let expected = vec![test_job(1, JobState::Running)];
        let accounting = HashMap::new();

        let stale = jobs_needing_resync(&expected, &accounting, &no_unknown());

        assert_eq!(stale, vec![1]);
    }

    #[test]
    fn jobs_needing_resync_flags_stale_state() {
        let expected = vec![test_job(1, JobState::Completed)];
        let mut accounting = HashMap::new();
        accounting.insert(1, synced_row("RUNNING"));

        let stale = jobs_needing_resync(&expected, &accounting, &no_unknown());

        assert_eq!(stale, vec![1]);
    }

    #[test]
    fn jobs_needing_resync_ignores_job_in_sync() {
        let expected = vec![test_job(1, JobState::Running)];
        let mut accounting = HashMap::new();
        accounting.insert(1, synced_row("RUNNING"));

        let stale = jobs_needing_resync(&expected, &accounting, &no_unknown());

        assert!(stale.is_empty());
    }

    #[test]
    fn jobs_needing_resync_handles_mixed_batch() {
        let expected = vec![
            test_job(1, JobState::Running),
            test_job(2, JobState::Completed),
            test_job(3, JobState::Failed),
        ];
        let mut accounting = HashMap::new();
        accounting.insert(1, synced_row("RUNNING")); // in sync
        accounting.insert(2, synced_row("RUNNING")); // stale
                                                     // job 3 missing entirely

        let mut stale = jobs_needing_resync(&expected, &accounting, &no_unknown());
        stale.sort();

        assert_eq!(stale, vec![2, 3]);
    }

    // A row whose state matches but is missing start metadata (record_job_end
    // created it from scratch because record_job_start's retries exhausted)
    // must still be flagged, or the gap is permanent.
    #[test]
    fn jobs_needing_resync_flags_bare_row_even_when_state_matches() {
        let expected = vec![test_job(1, JobState::Completed)];
        let mut accounting = HashMap::new();
        accounting.insert(
            1,
            AccountingRowState {
                run_attempt: Some(0),
                state: "COMPLETED".to_string(),
                missing_metadata: true,
                has_start_time: false,
                total_gpus: 0,
            },
        );

        let stale = jobs_needing_resync(&expected, &accounting, &no_unknown());

        assert_eq!(stale, vec![1]);
    }

    // A job whose read failed this pass must never be flagged as needing
    // resync, even though it's also absent from `accounting` — a
    // confirmed-absent row and a failed read are not the same thing, and
    // conflating them can clobber a correct finalized record via write_start.
    #[test]
    fn jobs_needing_resync_skips_jobs_with_unknown_read_status() {
        let expected = vec![
            test_job(1, JobState::Completed),
            test_job(2, JobState::Running),
        ];
        let accounting = HashMap::new();
        let mut unknown = HashSet::new();
        unknown.insert(1);

        let stale = jobs_needing_resync(&expected, &accounting, &unknown);

        assert_eq!(stale, vec![2]);
    }

    // Accounting only ever models RUNNING or a finalized state, so states it
    // can't represent must map to RUNNING for comparison — otherwise a
    // suspended/completing job is flagged stale forever.
    #[test]
    fn accounting_state_maps_non_finalized_states_to_running() {
        for state in [JobState::Running, JobState::Suspended, JobState::Completing] {
            assert_eq!(accounting_state(&test_job(1, state)), JobState::Running);
        }
        assert_eq!(
            accounting_state(&test_job(1, JobState::Completed)),
            JobState::Completed
        );
        assert_eq!(
            accounting_state(&test_job(1, JobState::Failed)),
            JobState::Failed
        );
    }

    // is_finalized(), not is_terminal(): a durably Preempted job (which
    // is_terminal() excludes) must compare against its own state, not get
    // mapped to RUNNING — otherwise a resync pass would clobber a correct
    // PREEMPTED record with RUNNING/no-end_time on every single cycle.
    #[test]
    fn accounting_state_uses_is_finalized_for_preempted() {
        assert!(!JobState::Preempted.is_terminal());
        assert!(JobState::Preempted.is_finalized());
        assert_eq!(
            accounting_state(&test_job(1, JobState::Preempted)),
            JobState::Preempted
        );
    }

    fn preempt_cancelled(job_id: JobId, state: JobState) -> Job {
        let mut job = test_job(job_id, state);
        job.cancel_signaled_at = Some(Utc::now());
        job.preempt_mode = Some("Cancel".into());
        job
    }

    #[test]
    fn a_finished_preempt_cancel_is_preempted_in_accounting() {
        assert_eq!(
            accounting_state(&preempt_cancelled(1, JobState::Cancelled)),
            JobState::Preempted
        );
    }

    #[test]
    fn a_preempt_cancel_still_completing_is_running_in_accounting() {
        assert_eq!(
            accounting_state(&preempt_cancelled(1, JobState::Completing)),
            JobState::Running
        );
    }

    #[test]
    fn a_user_cancel_stays_cancelled_in_accounting() {
        let mut job = test_job(1, JobState::Cancelled);
        job.cancel_signaled_at = Some(Utc::now());
        assert_eq!(accounting_state(&job), JobState::Cancelled);
    }

    #[test]
    fn jobs_needing_resync_leaves_a_preempted_row_of_a_preempt_cancel_alone() {
        let job = preempt_cancelled(1, JobState::Cancelled);
        let expected = vec![job];
        let mut accounting = HashMap::new();
        accounting.insert(1, synced_row("PREEMPTED"));

        assert!(jobs_needing_resync(&expected, &accounting, &no_unknown()).is_empty());
    }

    #[test]
    fn jobs_needing_resync_flags_stale_preempted_row_stuck_at_running() {
        // A durably-Preempted job whose accounting row is still RUNNING (the
        // pre-fix behavior, since is_terminal() excluded Preempted from ever
        // getting write_end'd) must be flagged for resync so it converges on
        // PREEMPTED rather than being ignored forever.
        let expected = vec![test_job(1, JobState::Preempted)];
        let mut accounting = HashMap::new();
        accounting.insert(1, synced_row("RUNNING"));

        let stale = jobs_needing_resync(&expected, &accounting, &no_unknown());

        assert_eq!(stale, vec![1]);
    }

    #[test]
    fn jobs_needing_resync_ignores_suspended_job_matching_running_row() {
        let expected = vec![test_job(1, JobState::Suspended)];
        let mut accounting = HashMap::new();
        accounting.insert(1, synced_row("RUNNING"));

        let stale = jobs_needing_resync(&expected, &accounting, &no_unknown());

        assert!(stale.is_empty());
    }

    fn test_job_id(slot: u32) -> u32 {
        const BASE: u32 = 9_500_000;
        BASE + (std::process::id() % 10_000) * 10 + slot
    }

    fn test_job(job_id: JobId, state: JobState) -> Job {
        let spec = JobSpec {
            name: "reconcile-test".into(),
            user: format!("spur_reconcile_user_{}", std::process::id()),
            num_nodes: 1,
            num_tasks: 2,
            cpus_per_task: 3,
            work_dir: "/tmp".into(),
            ..Default::default()
        };
        let mut job = Job::new(job_id, spec);
        let start = Utc::now() - chrono::Duration::hours(1);
        job.start_time = Some(start);
        job.state = state;
        if state.is_finalized() {
            job.end_time = Some(start + chrono::Duration::minutes(5));
            job.exit_code = Some(0);
        }
        job
    }

    async fn test_pool() -> anyhow::Result<PgPool> {
        let url = std::env::var("DATABASE_URL")?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .connect(&url)
            .await?;
        db::migrate(&pool).await?;
        Ok(pool)
    }

    fn bare_end(job_id: JobId, state: &str, end_time: chrono::DateTime<Utc>) -> db::JobEndRecord {
        db::JobEndRecord {
            job_id,
            state: state.to_string(),
            exit_code: 0,
            end_time,
            exit_signal: 0,
            derived_exit_code: 0,
            preempted_by: None,
            preempt_mode: String::new(),
            preempt_qos: String::new(),
            reason: None,
            reason_detail: String::new(),
            ended_by: String::new(),
            ended_by_uid: None,
            metadata: None,
        }
    }

    async fn delete_job(pool: &PgPool, job_id: JobId) {
        let _ = sqlx::query("DELETE FROM jobs WHERE job_id = $1")
            .bind(job_id as i64)
            .execute(pool)
            .await;
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn reconciliation_preserves_usage_across_out_of_order_runs() -> anyhow::Result<()> {
        let pool = test_pool().await?;
        let job_id = 9_800_408;
        let user = "reconcile-run-order";
        for sql in [
            "DELETE FROM jobs WHERE job_id=$1",
            "DELETE FROM job_usage_runs WHERE job_id=$1",
        ] {
            sqlx::query(sql)
                .bind(i64::from(job_id))
                .execute(&pool)
                .await?;
        }
        sqlx::query("DELETE FROM usage WHERE user_name=$1")
            .bind(user)
            .execute(&pool)
            .await?;
        let submit =
            chrono::DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")?.with_timezone(&Utc);
        let mut previous = Job::new(
            job_id,
            JobSpec {
                user: user.into(),
                num_tasks: 2,
                cpus_per_task: 3,
                ..Default::default()
            },
        );
        previous.submit_time = submit;
        previous.start_time = Some(submit + chrono::Duration::minutes(1));
        previous.end_time = Some(submit + chrono::Duration::seconds(70));
        previous.run_attempt = 1;
        previous.state = JobState::Completed;
        let mut cancelled = previous.clone();
        cancelled.state = JobState::Cancelled;
        cancelled.start_time = None;
        cancelled.end_time = Some(submit + chrono::Duration::seconds(71));
        resync_job(&pool, &cancelled).await;
        resync_job(&pool, &previous).await;
        let mut latest = previous.clone();
        latest.run_attempt = 2;
        latest.start_time = Some(submit + chrono::Duration::minutes(2));
        latest.end_time = Some(submit + chrono::Duration::seconds(130));
        resync_job(&pool, &latest).await;
        resync_job(&pool, &cancelled).await;
        resync_job(&pool, &latest).await;
        let row = sqlx::query("SELECT state, start_time, run_attempt FROM jobs WHERE job_id=$1")
            .bind(i64::from(job_id))
            .fetch_one(&pool)
            .await?;
        assert_eq!(row.get::<String, _>("state"), "COMPLETED");
        assert_eq!(
            row.get::<Option<chrono::DateTime<Utc>>, _>("start_time"),
            latest.start_time
        );
        assert_eq!(row.get::<i64, _>("run_attempt"), 2);
        let usage: (i64, i64) = sqlx::query_as(
            "SELECT SUM(cpu_seconds)::BIGINT, SUM(job_count)::BIGINT FROM usage WHERE user_name=$1",
        )
        .bind(user)
        .fetch_one(&pool)
        .await?;
        assert_eq!(usage, (120, 2));
        for sql in [
            "DELETE FROM jobs WHERE job_id=$1",
            "DELETE FROM job_usage_runs WHERE job_id=$1",
        ] {
            sqlx::query(sql)
                .bind(i64::from(job_id))
                .execute(&pool)
                .await?;
        }
        sqlx::query("DELETE FROM usage WHERE user_name=$1")
            .bind(user)
            .execute(&pool)
            .await?;
        Ok(())
    }

    // End to end: simulate record_job_start's retries exhausting (only
    // record_job_end ever lands, creating a bare row), then run the
    // reconciliation resync path and confirm it backfills the row rather
    // than leaving it permanently missing user_name/start_time.
    #[tokio::test]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn resync_job_backfills_bare_row_left_by_a_missed_job_start() -> anyhow::Result<()> {
        let pool = test_pool().await?;
        let job_id = test_job_id(0);
        delete_job(&pool, job_id).await;

        let job = test_job(job_id, JobState::Completed);

        // Simulate the outage: only the end notification ever lands.
        let mut conn = pool.acquire().await?;
        db::record_job_end(
            &mut conn,
            &bare_end(job_id, job.state.display(), job.end_time.unwrap()),
        )
        .await?;
        drop(conn);

        let bare = db::job_accounting_states(&pool, &[job_id])
            .await?
            .remove(&job_id)
            .expect("bare row exists");
        assert!(bare.missing_metadata);
        assert_eq!(bare.state, "COMPLETED");

        // The diff must flag this job even though state already matches.
        let expected = vec![job.clone()];
        let mut accounting = HashMap::new();
        accounting.insert(job_id, bare);
        let stale = jobs_needing_resync(&expected, &accounting, &no_unknown());
        assert_eq!(stale, vec![job_id]);

        resync_job(&pool, &job).await;

        let row = sqlx::query("SELECT user_name, start_time, state FROM jobs WHERE job_id = $1")
            .bind(job_id as i64)
            .fetch_one(&pool)
            .await?;
        let user_name: String = row.get("user_name");
        let start_time: Option<chrono::DateTime<Utc>> = row.get("start_time");
        let state: String = row.get("state");
        assert_eq!(user_name, job.spec.user);
        assert!(start_time.is_some(), "start_time must be backfilled");
        assert_eq!(
            state, "COMPLETED",
            "finalized state must survive the backfill"
        );

        // Usage was skipped originally (start_time was NULL); the backfill
        // must trigger it since end_time was already present.
        let usage_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM usage WHERE user_name = $1 AND job_count > 0")
                .bind(&job.spec.user)
                .fetch_one(&pool)
                .await?;
        assert!(
            usage_count > 0,
            "usage must be backfilled along with the row"
        );

        delete_job(&pool, job_id).await;
        sqlx::query("DELETE FROM usage WHERE user_name = $1")
            .bind(&job.spec.user)
            .execute(&pool)
            .await?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn resync_job_writes_a_preempt_cancel_as_preempted() -> anyhow::Result<()> {
        let pool = test_pool().await?;
        let job_id = test_job_id(7);
        delete_job(&pool, job_id).await;
        let mut job = preempt_cancelled(job_id, JobState::Cancelled);
        // Its own user, so the usage cleanup below cannot race the backfill test's.
        job.spec.user = format!("spur_reconcile_preempt_{}", std::process::id());

        resync_job(&pool, &job).await;

        let state: String = sqlx::query("SELECT state FROM jobs WHERE job_id = $1")
            .bind(job_id as i64)
            .fetch_one(&pool)
            .await?
            .get("state");
        delete_job(&pool, job_id).await;
        sqlx::query("DELETE FROM usage WHERE user_name = $1")
            .bind(&job.spec.user)
            .execute(&pool)
            .await?;
        assert_eq!(state, "PREEMPTED");
        Ok(())
    }

    // End to end: a suspended job's accounting row correctly shows RUNNING
    // (accounting has no SUSPENDED state); a resync pass must not treat that
    // as stale and must not clobber the row.
    #[tokio::test]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn suspended_job_is_not_flagged_stale_against_a_running_row() -> anyhow::Result<()> {
        let pool = test_pool().await?;
        let job_id = test_job_id(1);
        delete_job(&pool, job_id).await;

        let running_job = test_job(job_id, JobState::Running);
        resync_job(&pool, &running_job).await;

        let suspended_job = test_job(job_id, JobState::Suspended);
        let row = db::job_accounting_states(&pool, &[job_id])
            .await?
            .remove(&job_id)
            .expect("row exists");
        let expected = vec![suspended_job];
        let mut accounting = HashMap::new();
        accounting.insert(job_id, row);

        let stale = jobs_needing_resync(&expected, &accounting, &no_unknown());
        assert!(
            stale.is_empty(),
            "a suspended job matching a RUNNING row must not be flagged stale"
        );

        delete_job(&pool, job_id).await;
        Ok(())
    }

    // Regression test for the core atomicity bug: record_job_start
    // unconditionally resets state='RUNNING'/end_time=NULL, so a resync pass
    // that backfills a job already holding a correct, complete terminal
    // record must never leave that record clobbered if anything fails
    // between write_start committing its (would-be-corrupting) write and
    // write_end correcting it. write_start's own INSERT is single-statement
    // atomic on its own — a bad value in it just fails outright without
    // writing anything, old buggy code included — so this drives write_start
    // to completion for real (proving it does flip the row to RUNNING with
    // no end_time, exactly the corrupted shape the bug left behind) and then
    // forces a real SQL failure on the same transaction before commit, the
    // way a failing write_end or a dropped connection would. Only the
    // transaction's rollback — not any additional application-level fixup —
    // is what must protect the pre-existing terminal record here.
    #[tokio::test]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn resync_job_transaction_rolls_back_a_partial_backfill() -> anyhow::Result<()> {
        let pool = test_pool().await?;
        let job_id = test_job_id(2);
        delete_job(&pool, job_id).await;

        // Seed a correct, complete terminal record, as if a prior pass had
        // already recorded it successfully.
        let job = test_job(job_id, JobState::Completed);
        {
            let mut conn = pool.acquire().await?;
            db::record_job_start(
                &mut conn,
                &db::JobStartRecord {
                    job_id,
                    run_attempt: None,
                    name: job.spec.name.clone(),
                    user: job.spec.user.clone(),
                    account: String::new(),
                    partition: String::new(),
                    qos: String::new(),
                    num_nodes: job.spec.num_nodes,
                    num_tasks: job.spec.num_tasks,
                    cpus_per_task: job.spec.cpus_per_task,
                    total_gpus: 0,
                    memory_mb: 0,
                    submit_time: job.submit_time,
                    start_time: job.start_time.unwrap(),
                    reservation: None,
                    nodelist: String::new(),
                    idle_fill: false,
                    time_limit_secs: None,
                },
            )
            .await?;
            db::record_job_end(
                &mut conn,
                &bare_end(job_id, "COMPLETED", job.end_time.unwrap()),
            )
            .await?;
        }

        let mut next_run = job.clone();
        next_run.run_attempt += 1;
        next_run.start_time = Some(job.end_time.unwrap() + chrono::Duration::seconds(1));
        let mut tx = pool.begin().await?;
        write_start(&mut tx, &next_run).await?;

        // Within the uncommitted transaction, the row is now exactly the
        // corrupted shape the pre-fix bug used to leave committed: RUNNING
        // with end_time wiped out.
        let mid_row = sqlx::query("SELECT state, end_time FROM jobs WHERE job_id = $1")
            .bind(job_id as i64)
            .fetch_one(&mut *tx)
            .await?;
        let mid_state: String = mid_row.get("state");
        let mid_end_time: Option<chrono::DateTime<Utc>> = mid_row.get("end_time");
        assert_eq!(mid_state, "RUNNING");
        assert!(mid_end_time.is_none());

        // Simulate write_end failing (or a connection drop) before commit —
        // any real SQL error on the same transaction has the same effect.
        assert!(sqlx::query("SELECT 1/0").execute(&mut *tx).await.is_err());
        drop(tx); // never committed: sqlx issues ROLLBACK

        let row = sqlx::query("SELECT state, end_time FROM jobs WHERE job_id = $1")
            .bind(job_id as i64)
            .fetch_one(&pool)
            .await?;
        let state: String = row.get("state");
        let end_time: Option<chrono::DateTime<Utc>> = row.get("end_time");
        assert_eq!(
            state, "COMPLETED",
            "a rolled-back backfill must never leave the row RUNNING"
        );
        assert!(
            end_time.is_some(),
            "a rolled-back backfill must never wipe out end_time"
        );

        delete_job(&pool, job_id).await;
        Ok(())
    }
}
