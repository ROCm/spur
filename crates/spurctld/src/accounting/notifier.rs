// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::future::Future;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use tracing::error;

use spur_core::job::{JobId, JobState};

const RETRY_ATTEMPTS: u32 = 3;
const RETRY_BACKOFF: Duration = Duration::from_millis(200);
// Bounds how long a single attempt can pin one of the pool's 8 connections.
// Without this, a hung (not fully down) Postgres connection lets 3 retries
// each hold a connection indefinitely, rather than failing fast and freeing it.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);

/// Retry a fallible async operation up to `attempts` times, doubling `backoff`
/// between tries. Each attempt is bounded by `ATTEMPT_TIMEOUT`. Returns the
/// last error (or a timeout error) if all attempts fail.
async fn retry_with_backoff<F, Fut>(
    mut f: F,
    attempts: u32,
    mut backoff: Duration,
) -> anyhow::Result<()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    let mut attempt = 1;
    loop {
        match tokio::time::timeout(ATTEMPT_TIMEOUT, f()).await {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(e)) if attempt >= attempts => return Err(e),
            Err(_) if attempt >= attempts => {
                return Err(anyhow::anyhow!(
                    "operation timed out after {:?}",
                    ATTEMPT_TIMEOUT
                ));
            }
            Ok(Err(_)) | Err(_) => {
                tokio::time::sleep(backoff).await;
                backoff *= 2;
                attempt += 1;
            }
        }
    }
}

pub struct JobStartRecord {
    pub job_id: JobId,
    pub name: String,
    pub user: String,
    pub account: String,
    pub partition: String,
    pub num_nodes: u32,
    pub num_tasks: u32,
    pub cpus_per_task: u32,
    pub memory_mb: u64,
    pub submit_time: DateTime<Utc>,
    pub start_time: DateTime<Utc>,
    pub reservation: Option<String>,
}

pub struct AccountingNotifier {
    pool: PgPool,
}

impl AccountingNotifier {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn notify_job_start(&self, record: JobStartRecord) {
        let pool = self.pool.clone();
        let job_id = record.job_id;
        let name = record.name;
        let user = record.user;
        let account = record.account;
        let partition = record.partition;
        let num_nodes = record.num_nodes as i32;
        let num_tasks = record.num_tasks as i32;
        let cpus_per_task = record.cpus_per_task as i32;
        let memory_mb = record.memory_mb as i64;
        let submit_time = record.submit_time;
        let start_time = record.start_time;
        let reservation = record.reservation.unwrap_or_default();
        tokio::spawn(async move {
            let write = || {
                super::db::record_job_start(
                    &pool,
                    job_id as i32,
                    &name,
                    &user,
                    &account,
                    &partition,
                    num_nodes,
                    num_tasks,
                    cpus_per_task,
                    memory_mb,
                    submit_time,
                    start_time,
                    &reservation,
                )
            };
            if let Err(e) = retry_with_backoff(write, RETRY_ATTEMPTS, RETRY_BACKOFF).await {
                error!(job_id, error = %e, "failed to record job start in accounting after retries");
            }
        });
    }

    pub fn notify_job_end(
        &self,
        job_id: JobId,
        state: JobState,
        exit_code: i32,
        end_time: DateTime<Utc>,
        exit_signal: i32,
        derived_exit_code: i32,
    ) {
        let pool = self.pool.clone();
        let state_str = state.display().to_owned();
        tokio::spawn(async move {
            let write = || {
                super::db::record_job_end(
                    &pool,
                    job_id as i32,
                    &state_str,
                    exit_code,
                    end_time,
                    exit_signal,
                    derived_exit_code,
                )
            };
            if let Err(e) = retry_with_backoff(write, RETRY_ATTEMPTS, RETRY_BACKOFF).await {
                error!(job_id, error = %e, "failed to record job end in accounting after retries");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn retry_with_backoff_gives_up_after_all_attempts_fail() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_clone = calls.clone();
        let f = move || {
            calls_clone.fetch_add(1, Ordering::SeqCst);
            async { Err(anyhow::anyhow!("boom")) }
        };

        let result = retry_with_backoff(f, 3, Duration::from_millis(1)).await;

        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retry_with_backoff_stops_on_first_success() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_clone = calls.clone();
        let f = move || {
            let n = calls_clone.fetch_add(1, Ordering::SeqCst) + 1;
            async move {
                if n < 2 {
                    Err(anyhow::anyhow!("transient"))
                } else {
                    Ok(())
                }
            }
        };

        let result = retry_with_backoff(f, 3, Duration::from_millis(1)).await;

        assert!(result.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
