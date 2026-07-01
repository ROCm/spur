// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Controller-side cache of QoS definitions loaded from the accounting database.
//!
//! Mirrors `fairshare_cache`: an `RwLock<HashMap>` refreshed on a background
//! loop that retains stale data on error. The scheduler's `qos_block_for` reads
//! this cache so the dormant `QOS*` pending-reasons fire against real limits.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use sqlx::PgPool;
use tracing::{info, warn};

use spur_core::accounting::{Qos, QosLimits, QosPreemptMode, TresRecord};

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

    /// Test-only seam: populates the cache without a database.
    #[cfg(test)]
    pub(crate) fn insert(&self, qos: Qos) {
        self.qos.write().insert(qos.name.clone(), qos);
    }

    pub fn spawn_refresh_loop(self: &Arc<Self>, pool: PgPool, refresh_interval_secs: u64) {
        let cache = Arc::clone(self);
        let interval = Duration::from_secs(refresh_interval_secs.max(10));

        tokio::spawn(async move {
            match tokio::time::timeout(Duration::from_secs(5), Self::fetch(&pool)).await {
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

                match tokio::time::timeout(Duration::from_secs(10), Self::fetch(&pool)).await {
                    Ok(Ok(qos)) => cache.replace(qos),
                    Ok(Err(e)) => warn!(error = %e, "qos refresh failed, retaining stale data"),
                    Err(_) => warn!("qos refresh timed out, retaining stale data"),
                }
            }
        });
    }

    async fn fetch(pool: &PgPool) -> anyhow::Result<HashMap<String, Qos>> {
        let records = crate::accounting::db::list_qos(pool).await?;
        let qos = records
            .into_iter()
            .map(|r| (r.name.clone(), qos_from_record(r)))
            .collect();
        Ok(qos)
    }
}

impl Default for QosCache {
    fn default() -> Self {
        Self::new()
    }
}

fn qos_from_record(r: crate::accounting::db::QosRecord) -> Qos {
    let opt_u32 = |v: Option<i32>| v.filter(|&x| x > 0).map(|x| x as u32);
    let opt_tres = |s: Option<String>| s.filter(|s| !s.is_empty()).map(|s| TresRecord::parse(&s));

    Qos {
        name: r.name,
        description: r.description,
        priority: r.priority,
        preempt_mode: r.preempt_mode.parse::<QosPreemptMode>().unwrap_or_default(),
        limits: QosLimits {
            max_jobs_per_user: opt_u32(r.max_jobs_per_user),
            max_submit_jobs_per_user: opt_u32(r.max_submit_per_user),
            max_tres_per_job: opt_tres(r.max_tres_per_job),
            max_tres_per_user: opt_tres(r.max_tres_per_user),
            grp_tres: opt_tres(r.grp_tres),
            max_wall_minutes: opt_u32(r.max_wall_min),
            grp_wall_minutes: None,
        },
        usage_factor: r.usage_factor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_core::accounting::TresType;
    use spur_core::job::{Job, JobSpec, PendingReason};
    use spur_core::qos::{check_qos_limits, QosCheckResult};

    fn make_qos(name: &str) -> Qos {
        Qos {
            name: name.into(),
            description: String::new(),
            priority: 0,
            preempt_mode: QosPreemptMode::default(),
            limits: QosLimits::default(),
            usage_factor: 1.0,
        }
    }

    #[test]
    fn test_cache_get_returns_converted_qos() {
        let cache = QosCache::new();
        let mut qos = make_qos("normal");
        qos.limits.max_submit_jobs_per_user = Some(3);
        cache.replace(HashMap::from([("normal".to_string(), qos)]));

        assert!(cache.get("missing").is_none());
        let got = cache.get("normal").expect("present");
        assert_eq!(got.limits.max_submit_jobs_per_user, Some(3));
    }

    #[test]
    fn test_cached_qos_fires_submit_limit_reason() {
        let cache = QosCache::new();
        let mut qos = make_qos("strict");
        qos.limits.max_submit_jobs_per_user = Some(2);
        cache.replace(HashMap::from([("strict".to_string(), qos)]));

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
        let mut qos = make_qos("cpucap");
        qos.limits.max_tres_per_user = Some(TresRecord::parse("cpu=8"));
        cache.replace(HashMap::from([("cpucap".to_string(), qos)]));

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
