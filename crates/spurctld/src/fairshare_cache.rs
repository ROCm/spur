// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use tonic::transport::Channel;
use tracing::{debug, info, warn};

use spur_proto::proto::slurm_accounting_client::SlurmAccountingClient;
use spur_proto::proto::GetFairshareFactorsRequest;

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

    pub fn spawn_refresh_loop(
        self: &Arc<Self>,
        host: String,
        halflife_days: u32,
        refresh_interval_secs: u64,
    ) {
        let cache = Arc::clone(self);
        let interval = Duration::from_secs(refresh_interval_secs.max(10));

        tokio::spawn(async move {
            let uri = if host.starts_with("http://") || host.starts_with("https://") {
                host.clone()
            } else {
                format!("http://{}", host)
            };

            match tokio::time::timeout(Duration::from_secs(5), Self::fetch(&uri, halflife_days))
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
                    Self::fetch(&uri, halflife_days),
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

    async fn fetch(
        uri: &str,
        halflife_days: u32,
    ) -> anyhow::Result<HashMap<(String, String), f64>> {
        let mut client: SlurmAccountingClient<Channel> =
            SlurmAccountingClient::connect(uri.to_owned()).await?;
        let req = GetFairshareFactorsRequest { halflife_days };
        let resp = client.get_fairshare_factors(req).await?;
        let factors = resp
            .into_inner()
            .entries
            .into_iter()
            .map(|e| ((e.user, e.account), e.factor))
            .collect();
        Ok(factors)
    }
}
