// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use sqlx::PgPool;
use tracing::{debug, info, warn};

pub struct FairshareCache {
    factors: RwLock<HashMap<(String, String), f64>>,
}

impl FairshareCache {
    pub fn new() -> Self {
        Self {
            factors: RwLock::new(HashMap::new()),
        }
    }

    pub fn get(&self, user: &str, account: &str) -> f64 {
        let key = (user.to_owned(), account.to_owned());
        match self.factors.read().get(&key) {
            Some(&factor) => factor,
            None => {
                debug!(
                    user,
                    account, "fairshare factor not found, defaulting to neutral"
                );
                1.0
            }
        }
    }

    fn replace(&self, new_factors: HashMap<(String, String), f64>) {
        *self.factors.write() = new_factors;
    }

    /// Set a single user/account's fairshare factor directly, bypassing the
    /// periodic DB refresh. Scheduling tests need non-neutral fairshare
    /// without standing up a PostgreSQL-backed refresh loop.
    #[cfg(test)]
    pub(crate) fn set_for_test(&self, user: &str, account: &str, factor: f64) {
        self.factors
            .write()
            .insert((user.to_owned(), account.to_owned()), factor);
    }

    pub fn spawn_refresh_loop(
        self: &Arc<Self>,
        pool: PgPool,
        halflife_days: u32,
        refresh_interval_secs: u64,
    ) {
        let cache = Arc::clone(self);
        let interval = Duration::from_secs(refresh_interval_secs.max(10));

        tokio::spawn(async move {
            match tokio::time::timeout(
                Duration::from_secs(5),
                crate::accounting::fairshare_factors(&pool, halflife_days),
            )
            .await
            {
                Ok(Ok(factors)) => {
                    info!(count = factors.len(), "fairshare cache initialized");
                    cache.replace(factors);
                }
                Ok(Err(e)) => {
                    warn!(error = %e, "initial fairshare fetch failed, will retry in background");
                }
                Err(_) => {
                    warn!("initial fairshare fetch timed out, will retry in background");
                }
            }

            loop {
                tokio::time::sleep(interval).await;

                match tokio::time::timeout(
                    Duration::from_secs(10),
                    crate::accounting::fairshare_factors(&pool, halflife_days),
                )
                .await
                {
                    Ok(Ok(factors)) => cache.replace(factors),
                    Ok(Err(e)) => {
                        warn!(error = %e, "fairshare refresh failed, retaining stale data")
                    }
                    Err(_) => warn!("fairshare refresh timed out, retaining stale data"),
                }
            }
        });
    }
}
