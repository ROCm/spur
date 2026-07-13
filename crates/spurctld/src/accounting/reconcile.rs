// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use sqlx::PgPool;
use tracing::{error, info, warn};

use spur_core::job::{Job, JobId};

use crate::cluster::ClusterManager;
use crate::raft::RaftHandle;

use super::db;

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
            if !raft.is_leader() {
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
        .map(|j| (j.job_id, j.state.display().to_owned()))
        .collect();

    let mut accounting_states = HashMap::new();
    for job in &candidates {
        match db::job_accounting_state(pool, job.job_id as i32).await {
            Ok(Some(state)) => {
                accounting_states.insert(job.job_id, state);
            }
            Ok(None) => {}
            Err(e) => {
                warn!(job_id = job.job_id, error = %e, "reconciliation: failed to query accounting state");
            }
        }
    }

    let stale = jobs_needing_resync(&expected, &accounting_states);
    if stale.is_empty() {
        return;
    }
    info!(
        count = stale.len(),
        "reconciliation: resyncing jobs with missing or stale accounting records"
    );

    for job in candidates.iter().filter(|j| stale.contains(&j.job_id)) {
        let row_missing = !accounting_states.contains_key(&job.job_id);
        resync_job(pool, job, row_missing).await;
    }
}

/// Diff in-memory job state against known accounting state. Pure so it can
/// be unit tested without a database.
fn jobs_needing_resync(
    expected: &[(JobId, String)],
    accounting: &HashMap<JobId, String>,
) -> Vec<JobId> {
    expected
        .iter()
        .filter(|(id, state)| accounting.get(id) != Some(state))
        .map(|(id, _)| *id)
        .collect()
}

async fn resync_job(pool: &PgPool, job: &Job, row_missing: bool) {
    // record_job_start unconditionally sets state='RUNNING', so only call it
    // when the row doesn't exist yet or the job hasn't reached a terminal
    // state — otherwise it would clobber a correct terminal state.
    if row_missing || !job.state.is_terminal() {
        if let Err(e) = write_start(pool, job).await {
            error!(job_id = job.job_id, error = %e, "reconciliation: failed to resync job start");
            return;
        }
    }

    if job.state.is_terminal() {
        if let Err(e) = write_end(pool, job).await {
            error!(job_id = job.job_id, error = %e, "reconciliation: failed to resync job end");
        }
    }
}

async fn write_start(pool: &PgPool, job: &Job) -> anyhow::Result<()> {
    let spec = &job.spec;
    let memory_mb = job
        .allocated_resources
        .as_ref()
        .map(|r| r.memory_mb)
        .unwrap_or(0);
    let start_time = job.start_time.unwrap_or(job.submit_time);
    db::record_job_start(
        pool,
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

async fn write_end(pool: &PgPool, job: &Job) -> anyhow::Result<()> {
    let end_time = job.end_time.unwrap_or_else(Utc::now);
    db::record_job_end(
        pool,
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

    #[test]
    fn jobs_needing_resync_flags_missing_job() {
        let expected = vec![(1, "RUNNING".to_string())];
        let accounting = HashMap::new();

        let stale = jobs_needing_resync(&expected, &accounting);

        assert_eq!(stale, vec![1]);
    }

    #[test]
    fn jobs_needing_resync_flags_stale_state() {
        let expected = vec![(1, "COMPLETED".to_string())];
        let mut accounting = HashMap::new();
        accounting.insert(1, "RUNNING".to_string());

        let stale = jobs_needing_resync(&expected, &accounting);

        assert_eq!(stale, vec![1]);
    }

    #[test]
    fn jobs_needing_resync_ignores_job_in_sync() {
        let expected = vec![(1, "RUNNING".to_string())];
        let mut accounting = HashMap::new();
        accounting.insert(1, "RUNNING".to_string());

        let stale = jobs_needing_resync(&expected, &accounting);

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
        accounting.insert(1, "RUNNING".to_string()); // in sync
        accounting.insert(2, "RUNNING".to_string()); // stale
                                                     // job 3 missing entirely

        let mut stale = jobs_needing_resync(&expected, &accounting);
        stale.sort();

        assert_eq!(stale, vec![2, 3]);
    }
}
