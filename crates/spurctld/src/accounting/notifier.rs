// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::future::Future;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use tracing::error;

use spur_core::job::{JobId, JobState};

use super::db::JobStartRecord;
use super::txn::{TxnOutcome, TxnRecord};

const RETRY_ATTEMPTS: u32 = 3;
const RETRY_BACKOFF: Duration = Duration::from_millis(200);
// Bounds how long a single attempt can pin one of the pool's 8 connections.
// Without this, a hung (not fully down) Postgres connection lets 3 retries
// each hold a connection indefinitely, rather than failing fast and freeing it.
// Shared with reconcile.rs, which has the same hung-connection exposure on
// its resync writes.
pub(super) const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);

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

/// Counts spawned-but-unfinished writes so shutdown can wait for them. The
/// runtime would otherwise drop them, losing the rows with no log line.
#[derive(Default)]
struct InFlight {
    count: std::sync::atomic::AtomicUsize,
    idle: tokio::sync::Notify,
}

impl InFlight {
    fn enter(&self) {
        self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    fn leave(&self) {
        if self.count.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) == 1 {
            self.idle.notify_waiters();
        }
    }

    fn is_idle(&self) -> bool {
        self.count.load(std::sync::atomic::Ordering::SeqCst) == 0
    }
}

pub struct DrainHandle(std::sync::Arc<InFlight>);

impl DrainHandle {
    /// Wait for spawned writes to finish, up to `limit`. False means some were
    /// still pending, so their rows are lost.
    pub async fn wait(self, limit: Duration) -> bool {
        // Registered before the check so a write finishing in between still
        // wakes this, rather than waiting out the whole timeout.
        let waiter = self.0.idle.notified();
        if self.0.is_idle() {
            return true;
        }
        tokio::time::timeout(limit, waiter).await.is_ok()
    }
}

/// Releases on drop, so a write that panics still frees the drain.
struct InFlightGuard(std::sync::Arc<InFlight>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.leave();
    }
}

pub struct AccountingNotifier {
    pool: PgPool,
    inflight: std::sync::Arc<InFlight>,
}

impl AccountingNotifier {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            inflight: std::sync::Arc::new(InFlight::default()),
        }
    }

    /// Detached so the caller can release its lock on the notifier before
    /// awaiting the drain.
    pub fn drain_handle(&self) -> DrainHandle {
        DrainHandle(self.inflight.clone())
    }

    pub fn notify_job_start(&self, record: JobStartRecord) {
        let pool = self.pool.clone();
        let job_id = record.job_id;
        let inflight = self.inflight.clone();
        inflight.enter();
        tokio::spawn(async move {
            let _guard = InFlightGuard(inflight);
            let write = || async {
                let mut conn = pool.acquire().await?;
                super::db::record_job_start(&mut conn, &record).await
            };
            if let Err(e) = retry_with_backoff(write, RETRY_ATTEMPTS, RETRY_BACKOFF).await {
                error!(job_id, error = %e, "failed to record job start in accounting after retries");
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub fn notify_job_end(
        &self,
        job_id: JobId,
        state: JobState,
        exit_code: i32,
        end_time: DateTime<Utc>,
        exit_signal: i32,
        derived_exit_code: i32,
        preempted_by: Option<JobId>,
        preempt_mode: Option<String>,
        preempt_qos: Option<String>,
    ) {
        let pool = self.pool.clone();
        let state_str = state.display().to_owned();
        let inflight = self.inflight.clone();
        inflight.enter();
        tokio::spawn(async move {
            let _guard = InFlightGuard(inflight);
            let write = || async {
                let mut conn = pool.acquire().await?;
                super::db::record_job_end(
                    &mut conn,
                    job_id,
                    &state_str,
                    exit_code,
                    end_time,
                    exit_signal,
                    derived_exit_code,
                    preempted_by,
                    preempt_mode.as_deref().unwrap_or(""),
                    preempt_qos.as_deref().unwrap_or(""),
                )
                .await
            };
            if let Err(e) = retry_with_backoff(write, RETRY_ATTEMPTS, RETRY_BACKOFF).await {
                error!(job_id, error = %e, "failed to record job end in accounting after retries");
            }
        });
    }

    /// Best-effort async write of an audit record. Only committed (`Success`)
    /// rows retry; `Denied`/`Error` rows use a single attempt so a flood of
    /// (cheaply-triggered, possibly unauthenticated) failed attempts cannot pin
    /// the connection pool against real accounting writes.
    pub fn notify_txn(&self, record: TxnRecord) {
        let pool = self.pool.clone();
        let attempts = if record.outcome == TxnOutcome::Success {
            RETRY_ATTEMPTS
        } else {
            1
        };
        let inflight = self.inflight.clone();
        inflight.enter();
        tokio::spawn(async move {
            let _guard = InFlightGuard(inflight);
            let write = || async {
                let mut conn = pool.acquire().await?;
                super::db::record_txn(&mut conn, &record).await
            };
            match retry_with_backoff(write, attempts, RETRY_BACKOFF).await {
                Ok(()) => spur_core::audit_metrics::inc_rows_written(),
                Err(e) => {
                    // The log line alone is not alertable.
                    spur_core::audit_metrics::inc_rows_dropped();
                    error!(
                        actor = %record.actor,
                        action = record.action.as_str(),
                        entity = %record.entity_name,
                        attempts,
                        error = %e,
                        "failed to record txn in accounting"
                    );
                }
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

    #[tokio::test]
    async fn drain_returns_at_once_when_nothing_is_pending() {
        let tracker = Arc::new(InFlight::default());
        assert!(DrainHandle(tracker).wait(Duration::from_secs(30)).await);
    }

    /// The point of the drain: a write already spawned must finish before the
    /// process exits, or its row is lost with no log line.
    #[tokio::test]
    async fn drain_waits_for_a_write_that_is_still_running() {
        let tracker = Arc::new(InFlight::default());
        tracker.enter();

        let finished = Arc::new(AtomicU32::new(0));
        let writer = {
            let (tracker, finished) = (tracker.clone(), finished.clone());
            tokio::spawn(async move {
                let _guard = InFlightGuard(tracker);
                tokio::time::sleep(Duration::from_millis(50)).await;
                finished.fetch_add(1, Ordering::SeqCst);
            })
        };

        assert!(
            DrainHandle(tracker).wait(Duration::from_secs(30)).await,
            "drain must report the queue emptied"
        );
        assert_eq!(
            finished.load(Ordering::SeqCst),
            1,
            "drain returned before the write completed"
        );
        writer.await.expect("writer task");
    }

    /// A stuck write must not hold shutdown open indefinitely; the caller is
    /// told rows were abandoned instead.
    #[tokio::test]
    async fn drain_gives_up_on_a_stuck_write() {
        let tracker = Arc::new(InFlight::default());
        tracker.enter();

        assert!(!DrainHandle(tracker).wait(Duration::from_millis(20)).await);
    }
}
