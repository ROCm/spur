// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use sqlx::PgPool;
use tracing::{error, info, warn};

use spur_core::job::{Job, JobId, JobState};

use crate::cluster::ClusterManager;
use crate::raft::RaftHandle;

use super::db::{self, AccountingRowState};

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

/// Only jobs that have actually started accrue an accounting record
/// (`notify_job_start` fires from `start_job`), so jobs still pending are
/// not candidates.
async fn run_once(pool: &PgPool, cluster: &ClusterManager) {
    let candidates: Vec<Job> = cluster
        .get_jobs(&[], None, None, None, &[])
        .into_iter()
        .filter(|j| j.start_time.is_some())
        .collect();
    if candidates.is_empty() {
        return;
    }

    let expected: Vec<(JobId, String)> = candidates
        .iter()
        .map(|j| (j.job_id, accounting_expected_state(j.state)))
        .collect();

    let job_ids: Vec<i32> = candidates.iter().map(|j| j.job_id as i32).collect();
    let (accounting_states, unknown): (HashMap<JobId, AccountingRowState>, HashSet<JobId>) =
        match db::job_accounting_states(pool, &job_ids).await {
            Ok(rows) => (
                rows.into_iter()
                    .map(|(id, row)| (id as JobId, row))
                    .collect(),
                HashSet::new(),
            ),
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

    let stale = jobs_needing_resync(&expected, &accounting_states, &unknown);
    if stale.is_empty() {
        return;
    }
    info!(
        count = stale.len(),
        "reconciliation: resyncing jobs with missing or stale accounting records"
    );

    for job in candidates.iter().filter(|j| stale.contains(&j.job_id)) {
        let row = accounting_states.get(&job.job_id);
        let row_missing = row.is_none();
        let needs_start_backfill = row.map(|r| r.needs_start_backfill).unwrap_or(false);
        resync_job(pool, job, row_missing, needs_start_backfill).await;
    }
}

/// What accounting should show for a job's current state. Accounting only
/// ever models `RUNNING` or a terminal state (`record_job_start` always
/// writes `RUNNING`; only `record_job_end` writes a terminal state), so
/// in-memory states it has no representation for — `SUSPENDED`, `COMPLETING`
/// — must map to `RUNNING` for comparison. Otherwise a suspended or
/// completing job would be flagged stale on every single pass for as long as
/// it stays in that state.
fn accounting_expected_state(state: JobState) -> String {
    if state.is_terminal() {
        state.display().to_owned()
    } else {
        JobState::Running.display().to_owned()
    }
}

/// Diff in-memory job state against known accounting state. Pure so it can
/// be unit tested without a database. `unknown` holds jobs whose accounting
/// read failed this pass — distinct from a confirmed-absent row, so they're
/// skipped rather than treated as missing.
fn jobs_needing_resync(
    expected: &[(JobId, String)],
    accounting: &HashMap<JobId, AccountingRowState>,
    unknown: &HashSet<JobId>,
) -> Vec<JobId> {
    expected
        .iter()
        .filter(|(id, _)| !unknown.contains(id))
        .filter(|(id, state)| match accounting.get(id) {
            None => true,
            Some(row) => row.needs_start_backfill || &row.state != state,
        })
        .map(|(id, _)| *id)
        .collect()
}

/// Resync one job's accounting record. `write_start` and `write_end` (when
/// both are needed) run inside a single transaction so a failure partway
/// through — including `write_start` itself failing — rolls back cleanly
/// instead of leaving the row in an intermediate state: `record_job_start`
/// unconditionally sets `state='RUNNING'` with a wiped end time and exit
/// code, and previously that reset was committed on its own statement, with
/// `write_end` relied on to fix it back up immediately after. Any failure
/// between the two — or a concurrent reader landing in the gap — could
/// observe or permanently persist a correct terminal record clobbered back
/// to `RUNNING`. The whole attempt is bounded by `ATTEMPT_TIMEOUT` so a
/// wedged connection can't stall the reconciliation loop; a timed-out job is
/// simply retried on the next pass.
async fn resync_job(pool: &PgPool, job: &Job, row_missing: bool, needs_start_backfill: bool) {
    // record_job_start unconditionally sets state='RUNNING', so only call it
    // when the row doesn't exist yet, is missing start metadata a proper
    // record_job_start would have populated (see AccountingRowState), or the
    // job hasn't reached a terminal state — otherwise it would clobber a
    // correct terminal state. The terminal case is corrected in the same
    // transaction by write_end below.
    let needs_start = row_missing || needs_start_backfill || !job.state.is_terminal();
    let is_terminal = job.state.is_terminal();

    let attempt = async {
        let mut tx = pool.begin().await?;
        if needs_start {
            write_start(&mut tx, job).await?;
        }
        if is_terminal {
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
    let start_time = job.start_time.unwrap_or(job.submit_time);
    db::record_job_start(
        conn,
        job.job_id as i32,
        &spec.name,
        &spec.user,
        spec.account.as_deref().unwrap_or_default(),
        spec.partition.as_deref().unwrap_or_default(),
        spec.num_nodes as i32,
        spec.num_tasks as i32,
        spec.cpus_per_task as i32,
        memory_mb as i64,
        job.submit_time,
        start_time,
        spec.reservation.as_deref().unwrap_or_default(),
    )
    .await
}

async fn write_end(conn: &mut sqlx::PgConnection, job: &Job) -> anyhow::Result<()> {
    let end_time = job.end_time.unwrap_or_else(Utc::now);
    db::record_job_end(
        conn,
        job.job_id as i32,
        job.state.display(),
        job.exit_code.unwrap_or(0),
        end_time,
        job.exit_signal,
        job.derived_exit_code,
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
            needs_start_backfill: false,
        }
    }

    fn no_unknown() -> HashSet<JobId> {
        HashSet::new()
    }

    #[test]
    fn jobs_needing_resync_flags_missing_job() {
        let expected = vec![(1, "RUNNING".to_string())];
        let accounting = HashMap::new();

        let stale = jobs_needing_resync(&expected, &accounting, &no_unknown());

        assert_eq!(stale, vec![1]);
    }

    #[test]
    fn jobs_needing_resync_flags_stale_state() {
        let expected = vec![(1, "COMPLETED".to_string())];
        let mut accounting = HashMap::new();
        accounting.insert(1, synced_row("RUNNING"));

        let stale = jobs_needing_resync(&expected, &accounting, &no_unknown());

        assert_eq!(stale, vec![1]);
    }

    #[test]
    fn jobs_needing_resync_ignores_job_in_sync() {
        let expected = vec![(1, "RUNNING".to_string())];
        let mut accounting = HashMap::new();
        accounting.insert(1, synced_row("RUNNING"));

        let stale = jobs_needing_resync(&expected, &accounting, &no_unknown());

        assert!(stale.is_empty());
    }

    #[test]
    fn jobs_needing_resync_handles_mixed_batch() {
        let expected = vec![
            (1, "RUNNING".to_string()),
            (2, "COMPLETED".to_string()),
            (3, "FAILED".to_string()),
        ];
        let mut accounting = HashMap::new();
        accounting.insert(1, synced_row("RUNNING")); // in sync
        accounting.insert(2, synced_row("RUNNING")); // stale
                                                     // job 3 missing entirely

        let mut stale = jobs_needing_resync(&expected, &accounting, &no_unknown());
        stale.sort();

        assert_eq!(stale, vec![2, 3]);
    }

    // Finding 1: a row whose state matches but is missing start metadata
    // (record_job_end created it from scratch because record_job_start's
    // retries exhausted) must still be flagged, or the gap is permanent.
    #[test]
    fn jobs_needing_resync_flags_bare_row_even_when_state_matches() {
        let expected = vec![(1, "COMPLETED".to_string())];
        let mut accounting = HashMap::new();
        accounting.insert(
            1,
            AccountingRowState {
                state: "COMPLETED".to_string(),
                needs_start_backfill: true,
            },
        );

        let stale = jobs_needing_resync(&expected, &accounting, &no_unknown());

        assert_eq!(stale, vec![1]);
    }

    // Finding 2: a job whose read failed this pass must never be flagged as
    // needing resync, even though it's also absent from `accounting` — a
    // confirmed-absent row and a failed read are not the same thing, and
    // conflating them can clobber a correct terminal record via write_start.
    #[test]
    fn jobs_needing_resync_skips_jobs_with_unknown_read_status() {
        let expected = vec![(1, "COMPLETED".to_string()), (2, "RUNNING".to_string())];
        let accounting = HashMap::new();
        let mut unknown = HashSet::new();
        unknown.insert(1);

        let stale = jobs_needing_resync(&expected, &accounting, &unknown);

        assert_eq!(stale, vec![2]);
    }

    // Finding 3: accounting only ever models RUNNING or a terminal state, so
    // states it can't represent must map to RUNNING for comparison —
    // otherwise a suspended/completing job is flagged stale forever.
    #[test]
    fn accounting_expected_state_maps_non_terminal_states_to_running() {
        assert_eq!(accounting_expected_state(JobState::Running), "RUNNING");
        assert_eq!(accounting_expected_state(JobState::Suspended), "RUNNING");
        assert_eq!(accounting_expected_state(JobState::Completing), "RUNNING");
        assert_eq!(accounting_expected_state(JobState::Completed), "COMPLETED");
        assert_eq!(accounting_expected_state(JobState::Failed), "FAILED");
    }

    #[test]
    fn jobs_needing_resync_ignores_suspended_job_matching_running_row() {
        let expected = vec![(1, accounting_expected_state(JobState::Suspended))];
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
        if state.is_terminal() {
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

    async fn delete_job(pool: &PgPool, job_id: i32) {
        let _ = sqlx::query("DELETE FROM jobs WHERE job_id = $1")
            .bind(job_id)
            .execute(pool)
            .await;
    }

    // Finding 1, end to end: simulate record_job_start's retries exhausting
    // (only record_job_end ever lands, creating a bare row), then run the
    // reconciliation resync path and confirm it backfills the row rather
    // than leaving it permanently missing user_name/start_time.
    #[tokio::test]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn resync_job_backfills_bare_row_left_by_a_missed_job_start() -> anyhow::Result<()> {
        let pool = test_pool().await?;
        let job_id = test_job_id(0);
        delete_job(&pool, job_id as i32).await;

        let job = test_job(job_id, JobState::Completed);

        // Simulate the outage: only the end notification ever lands.
        let mut conn = pool.acquire().await?;
        db::record_job_end(
            &mut conn,
            job_id as i32,
            job.state.display(),
            0,
            job.end_time.unwrap(),
            0,
            0,
        )
        .await?;
        drop(conn);

        let bare = db::job_accounting_states(&pool, &[job_id as i32])
            .await?
            .remove(&(job_id as i32))
            .expect("bare row exists");
        assert!(bare.needs_start_backfill);
        assert_eq!(bare.state, "COMPLETED");

        // The diff must flag this job even though state already matches.
        let expected = vec![(job_id, accounting_expected_state(job.state))];
        let mut accounting = HashMap::new();
        accounting.insert(job_id, bare);
        let stale = jobs_needing_resync(&expected, &accounting, &no_unknown());
        assert_eq!(stale, vec![job_id]);

        resync_job(&pool, &job, false, true).await;

        let row = sqlx::query("SELECT user_name, start_time, state FROM jobs WHERE job_id = $1")
            .bind(job_id as i32)
            .fetch_one(&pool)
            .await?;
        let user_name: String = row.get("user_name");
        let start_time: Option<chrono::DateTime<Utc>> = row.get("start_time");
        let state: String = row.get("state");
        assert_eq!(user_name, job.spec.user);
        assert!(start_time.is_some(), "start_time must be backfilled");
        assert_eq!(
            state, "COMPLETED",
            "terminal state must survive the backfill"
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

        delete_job(&pool, job_id as i32).await;
        sqlx::query("DELETE FROM usage WHERE user_name = $1")
            .bind(&job.spec.user)
            .execute(&pool)
            .await?;
        Ok(())
    }

    // Finding 3, end to end: a suspended job's accounting row correctly shows
    // RUNNING (accounting has no SUSPENDED state); a resync pass must not
    // treat that as stale and must not clobber the row.
    #[tokio::test]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn suspended_job_is_not_flagged_stale_against_a_running_row() -> anyhow::Result<()> {
        let pool = test_pool().await?;
        let job_id = test_job_id(1);
        delete_job(&pool, job_id as i32).await;

        let running_job = test_job(job_id, JobState::Running);
        resync_job(&pool, &running_job, true, false).await;

        let suspended_job = test_job(job_id, JobState::Suspended);
        let row = db::job_accounting_states(&pool, &[job_id as i32])
            .await?
            .remove(&(job_id as i32))
            .expect("row exists");
        let expected = vec![(job_id, accounting_expected_state(suspended_job.state))];
        let mut accounting = HashMap::new();
        accounting.insert(job_id, row);

        let stale = jobs_needing_resync(&expected, &accounting, &no_unknown());
        assert!(
            stale.is_empty(),
            "a suspended job matching a RUNNING row must not be flagged stale"
        );

        delete_job(&pool, job_id as i32).await;
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
        delete_job(&pool, job_id as i32).await;

        // Seed a correct, complete terminal record, as if a prior pass had
        // already recorded it successfully.
        let job = test_job(job_id, JobState::Completed);
        {
            let mut conn = pool.acquire().await?;
            db::record_job_start(
                &mut conn,
                job_id as i32,
                &job.spec.name,
                &job.spec.user,
                "",
                "",
                job.spec.num_nodes as i32,
                job.spec.num_tasks as i32,
                job.spec.cpus_per_task as i32,
                0,
                job.submit_time,
                job.start_time.unwrap(),
                "",
            )
            .await?;
            db::record_job_end(
                &mut conn,
                job_id as i32,
                "COMPLETED",
                0,
                job.end_time.unwrap(),
                0,
                0,
            )
            .await?;
        }

        // Run write_start (the real function resync_job calls) inside a
        // transaction of our own, the same way resync_job's fix does.
        let mut tx = pool.begin().await?;
        write_start(&mut tx, &job).await?;

        // Within the uncommitted transaction, the row is now exactly the
        // corrupted shape the pre-fix bug used to leave committed: RUNNING
        // with end_time wiped out.
        let mid_row = sqlx::query("SELECT state, end_time FROM jobs WHERE job_id = $1")
            .bind(job_id as i32)
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
            .bind(job_id as i32)
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

        delete_job(&pool, job_id as i32).await;
        Ok(())
    }
}
