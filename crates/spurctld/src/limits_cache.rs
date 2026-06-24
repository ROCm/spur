// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Controller-side cache of QoS definitions loaded from the accounting daemon.
//!
//! Mirrors `fairshare_cache`: an `RwLock<HashMap>` refreshed on a background
//! loop that retains stale data on error. The scheduler's `qos_block_for` reads
//! this cache so the dormant `QOS*` pending-reasons fire against real limits.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use tonic::transport::Channel;
use tracing::{info, warn};

use spur_core::accounting::{Qos, QosLimits, QosPreemptMode, TresRecord};
use spur_proto::proto::slurm_accounting_client::SlurmAccountingClient;
use spur_proto::proto::{ListQosRequest, QosInfo};

pub struct QosCache {
    qos: RwLock<HashMap<String, Qos>>,
}

impl QosCache {
    pub fn new() -> Self {
        Self {
            qos: RwLock::new(HashMap::new()),
        }
    }

    pub fn get(&self, name: &str) -> Option<Qos> {
        self.qos.read().get(name).cloned()
    }

    fn replace(&self, new_qos: HashMap<String, Qos>) {
        *self.qos.write() = new_qos;
    }

    /// Test-only seam: populates the cache without the accounting daemon.
    #[cfg(test)]
    pub(crate) fn insert(&self, qos: Qos) {
        self.qos.write().insert(qos.name.clone(), qos);
    }

    pub fn spawn_refresh_loop(self: &Arc<Self>, host: String, refresh_interval_secs: u64) {
        let cache = Arc::clone(self);
        let interval = Duration::from_secs(refresh_interval_secs.max(10));

        tokio::spawn(async move {
            let uri = if host.starts_with("http://") || host.starts_with("https://") {
                host.clone()
            } else {
                format!("http://{}", host)
            };

            match tokio::time::timeout(Duration::from_secs(5), Self::fetch(&uri)).await {
                Ok(Ok(qos)) => {
                    info!(count = qos.len(), "qos cache initialized");
                    cache.replace(qos);
                }
                Ok(Err(e)) => {
                    warn!(error = %e, "initial qos fetch failed, will retry in background");
                }
                Err(_) => {
                    warn!("initial qos fetch timed out, will retry in background");
                }
            }

            loop {
                tokio::time::sleep(interval).await;

                match tokio::time::timeout(Duration::from_secs(10), Self::fetch(&uri)).await {
                    Ok(Ok(qos)) => cache.replace(qos),
                    Ok(Err(e)) => warn!(error = %e, "qos refresh failed, retaining stale data"),
                    Err(_) => warn!("qos refresh timed out, retaining stale data"),
                }
            }
        });
    }

    async fn fetch(uri: &str) -> anyhow::Result<HashMap<String, Qos>> {
        let mut client: SlurmAccountingClient<Channel> =
            SlurmAccountingClient::connect(uri.to_owned()).await?;
        let resp = client.list_qos(ListQosRequest {}).await?;
        let qos = resp
            .into_inner()
            .qos_list
            .into_iter()
            .map(|info| (info.name.clone(), qos_from_proto(info)))
            .collect();
        Ok(qos)
    }
}

impl Default for QosCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Zero/empty limit fields mean "no limit" — the accounting daemon encodes absent limits this way.
pub fn qos_from_proto(info: QosInfo) -> Qos {
    let opt_u32 = |v: u32| if v == 0 { None } else { Some(v) };
    let opt_tres = |s: &str| {
        if s.is_empty() {
            None
        } else {
            Some(TresRecord::parse(s))
        }
    };

    Qos {
        name: info.name,
        description: info.description,
        priority: info.priority,
        preempt_mode: info
            .preempt_mode
            .parse::<QosPreemptMode>()
            .unwrap_or_default(),
        limits: QosLimits {
            max_jobs_per_user: opt_u32(info.max_jobs_per_user),
            max_submit_jobs_per_user: opt_u32(info.max_submit_jobs_per_user),
            max_tres_per_job: opt_tres(&info.max_tres_per_job),
            max_tres_per_user: opt_tres(&info.max_tres_per_user),
            grp_tres: opt_tres(&info.grp_tres),
            max_wall_minutes: opt_u32(info.max_wall_minutes),
            grp_wall_minutes: None,
        },
        usage_factor: info.usage_factor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_core::accounting::TresType;
    use spur_core::job::{Job, JobSpec, PendingReason};
    use spur_core::qos::{check_qos_limits, QosCheckResult};

    fn proto(name: &str) -> QosInfo {
        QosInfo {
            name: name.into(),
            description: String::new(),
            priority: 0,
            preempt_mode: "off".into(),
            usage_factor: 1.0,
            max_jobs_per_user: 0,
            max_wall_minutes: 0,
            max_tres_per_job: String::new(),
            max_submit_jobs_per_user: 0,
            max_tres_per_user: String::new(),
            grp_tres: String::new(),
        }
    }

    #[test]
    fn test_proto_to_qos_parses_all_five_limits() {
        let mut p = proto("limited");
        p.max_jobs_per_user = 4;
        p.max_submit_jobs_per_user = 8;
        p.max_wall_minutes = 60;
        p.max_tres_per_job = "cpu=2".into();
        p.max_tres_per_user = "cpu=16".into();

        let qos = qos_from_proto(p);
        assert_eq!(qos.limits.max_jobs_per_user, Some(4));
        assert_eq!(qos.limits.max_submit_jobs_per_user, Some(8));
        assert_eq!(qos.limits.max_wall_minutes, Some(60));
        assert_eq!(
            qos.limits
                .max_tres_per_job
                .as_ref()
                .map(|t| t.get(TresType::Cpu)),
            Some(2)
        );
        assert_eq!(
            qos.limits
                .max_tres_per_user
                .as_ref()
                .map(|t| t.get(TresType::Cpu)),
            Some(16)
        );
    }

    #[test]
    fn test_proto_to_qos_zero_means_no_limit() {
        let qos = qos_from_proto(proto("empty"));
        assert!(qos.limits.max_jobs_per_user.is_none());
        assert!(qos.limits.max_submit_jobs_per_user.is_none());
        assert!(qos.limits.max_tres_per_job.is_none());
        assert!(qos.limits.max_tres_per_user.is_none());
        assert!(qos.limits.max_wall_minutes.is_none());
    }

    #[test]
    fn test_cache_get_returns_converted_qos() {
        let cache = QosCache::new();
        let mut p = proto("normal");
        p.max_submit_jobs_per_user = 3;
        cache.replace(HashMap::from([("normal".to_string(), qos_from_proto(p))]));

        assert!(cache.get("missing").is_none());
        let got = cache.get("normal").expect("present");
        assert_eq!(got.limits.max_submit_jobs_per_user, Some(3));
    }

    // A cache-sourced limited Qos drives check_qos_limits to the specific reason.
    #[test]
    fn test_cached_qos_fires_submit_limit_reason() {
        let cache = QosCache::new();
        let mut p = proto("strict");
        p.max_submit_jobs_per_user = 2;
        cache.replace(HashMap::from([("strict".to_string(), qos_from_proto(p))]));

        let qos = cache.get("strict").expect("present");
        let job = Job::new(
            1,
            JobSpec {
                name: "j".into(),
                user: "alice".into(),
                num_tasks: 1,
                cpus_per_task: 1,
                qos: Some("strict".into()),
                ..Default::default()
            },
        );
        let result = check_qos_limits(&job, &qos, 0, 2, &TresRecord::new(), &TresRecord::new());
        assert_eq!(
            result,
            QosCheckResult::Blocked(PendingReason::QosMaxSubmitJobPerUserLimit)
        );
    }

    #[test]
    fn test_cached_qos_fires_cpu_per_user_reason() {
        let cache = QosCache::new();
        let mut p = proto("cpucap");
        p.max_tres_per_user = "cpu=8".into();
        cache.replace(HashMap::from([("cpucap".to_string(), qos_from_proto(p))]));

        let qos = cache.get("cpucap").expect("present");
        let job = Job::new(
            2,
            JobSpec {
                name: "j".into(),
                user: "bob".into(),
                num_tasks: 4,
                cpus_per_task: 1,
                qos: Some("cpucap".into()),
                ..Default::default()
            },
        );
        let mut running = TresRecord::new();
        running.set(TresType::Cpu, 6);
        let result = check_qos_limits(&job, &qos, 0, 0, &running, &TresRecord::new());
        assert_eq!(
            result,
            QosCheckResult::Blocked(PendingReason::QosMaxCpuPerUserLimit)
        );
    }
}
