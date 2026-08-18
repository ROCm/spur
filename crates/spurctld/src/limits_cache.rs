// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Controller-side cache of QoS definitions loaded from the accounting database.
//!
//! Mirrors `fairshare_cache`: an `RwLock<HashMap>` refreshed on a background
//! loop that retains stale data on error. The scheduler's `qos_block_with` reads
//! this cache so the dormant `QOS*` pending-reasons fire against real limits.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use sqlx::PgPool;
use tracing::{info, warn};

use spur_core::accounting::{Qos, QosLimits, QosPreemptMode, TresRecord};

struct Snapshot {
    qos: HashMap<String, Qos>,
    loaded: bool,
}

pub struct QosCache {
    snapshot: RwLock<Snapshot>,
}

impl QosCache {
    pub fn new() -> Self {
        Self {
            snapshot: RwLock::new(Snapshot {
                qos: HashMap::new(),
                loaded: false,
            }),
        }
    }

    pub fn get(&self, name: &str) -> Option<Qos> {
        self.snapshot.read().qos.get(name).cloned()
    }

    /// True after at least one successful fetch from the accounting database.
    pub fn is_loaded(&self) -> bool {
        self.snapshot.read().loaded
    }

    fn replace(&self, new_qos: HashMap<String, Qos>) {
        let mut snap = self.snapshot.write();
        snap.qos = new_qos;
        snap.loaded = true;
    }

    /// Test-only seam: populates the cache without a database.
    #[cfg(test)]
    pub(crate) fn insert(&self, qos: Qos) {
        let mut snap = self.snapshot.write();
        snap.qos.insert(qos.name.clone(), qos);
        snap.loaded = true;
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

/// Per-QOS wall-clock consumption for `GrpWall`, derived from job history in the
/// accounting database. Reads return `None` until a first successful load, which
/// is what leaves the limit unapplied when accounting is disabled or unreachable.
///
/// A failed refresh after that keeps the last figure rather than reverting to
/// `None`: consumption over a multi-day window barely moves between refreshes, so
/// a slightly stale figure is a far better basis than dropping the budget
/// entirely and letting spend run unbounded through a database blip.
pub struct GrpWallCache {
    snapshot: RwLock<Option<HashMap<String, u64>>>,
}

impl GrpWallCache {
    pub fn new() -> Self {
        Self {
            snapshot: RwLock::new(None),
        }
    }

    /// Minutes consumed by `qos_name` in the window. A loaded cache reports zero
    /// for a QOS with no job history; an unloaded one reports `None`.
    pub fn consumed_minutes(&self, qos_name: &str) -> Option<u64> {
        let snap = self.snapshot.read();
        let consumed = snap.as_ref()?;
        Some(consumed.get(qos_name).copied().unwrap_or(0))
    }

    fn replace(&self, consumed: HashMap<String, u64>) {
        *self.snapshot.write() = Some(consumed);
    }

    /// Test-only seam: populates consumption without a database.
    #[cfg(test)]
    pub(crate) fn seed(&self, consumed: HashMap<String, u64>) {
        self.replace(consumed);
    }

    pub fn spawn_refresh_loop(
        self: &Arc<Self>,
        pool: PgPool,
        refresh_interval_secs: u64,
        window_days: u32,
    ) {
        let cache = Arc::clone(self);
        let interval = Duration::from_secs(refresh_interval_secs.max(10));

        tokio::spawn(async move {
            loop {
                match tokio::time::timeout(
                    Duration::from_secs(10),
                    crate::accounting::db::consumed_wall_minutes_by_qos(&pool, window_days),
                )
                .await
                {
                    Ok(Ok(consumed)) => cache.replace(consumed),
                    Ok(Err(e)) => {
                        warn!(error = %e, "grpwall usage refresh failed, retaining stale data")
                    }
                    Err(_) => warn!("grpwall usage refresh timed out, retaining stale data"),
                }

                tokio::time::sleep(interval).await;
            }
        });
    }
}

impl Default for GrpWallCache {
    fn default() -> Self {
        Self::new()
    }
}

fn qos_from_record(r: crate::accounting::db::QosRecord) -> Qos {
    use spur_core::accounting::limit_from_db;
    // Values are validated by `create_qos` before being stored, so a parse
    // failure here means the DB row predates that check or was edited
    // out-of-band; treat it as unset rather than poisoning the whole refresh.
    let opt_tres = |s: Option<String>| {
        s.filter(|s| !s.is_empty()).and_then(|s| {
            TresRecord::parse(&s)
                .inspect_err(|e| warn!(tres = %s, error = %e, "dropping unparseable stored TRES"))
                .ok()
        })
    };

    Qos {
        name: r.name,
        description: r.description,
        priority: r.priority,
        preempt_mode: r.preempt_mode.parse::<QosPreemptMode>().unwrap_or_default(),
        preempt: r
            .preempt
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        limits: QosLimits {
            max_jobs_per_user: limit_from_db(r.max_jobs_per_user),
            max_submit_jobs_per_user: limit_from_db(r.max_submit_per_user),
            max_submit_jobs_per_account: limit_from_db(r.max_submit_per_account),
            grp_submit_jobs: limit_from_db(r.grp_submit_jobs),
            max_tres_per_job: opt_tres(r.max_tres_per_job),
            max_tres_per_user: opt_tres(r.max_tres_per_user),
            grp_tres: opt_tres(r.grp_tres),
            max_wall_minutes: limit_from_db(r.max_wall_min),
            grp_wall_minutes: limit_from_db(r.grp_wall_min),
            preempt_exempt_time: r.preempt_exempt_time.map(|v| v as u32),
        },
        usage_factor: r.usage_factor,
        deny_on_limit: parse_deny_on_limit(&r.flags),
    }
}

/// Parse the QOS `flags` column (comma-separated) for the `DenyOnLimit` flag.
/// Unknown flags are ignored on read (writes reject them via
/// `canonicalize_qos_flags`); this tolerates rows imported from Slurm dumps that
/// carry flags Spur does not model yet.
fn parse_deny_on_limit(flags: &str) -> bool {
    flags
        .split(',')
        .any(|f| f.trim().eq_ignore_ascii_case("denyonlimit"))
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
            ..Default::default()
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
        let result = check_qos_limits(
            &job,
            &qos,
            0,
            2,
            &TresRecord::new(),
            &TresRecord::new(),
            None,
        );
        assert_eq!(
            result,
            QosCheckResult::Blocked(PendingReason::QosMaxSubmitJobPerUserLimit)
        );
    }

    #[test]
    fn test_cached_qos_fires_cpu_per_user_reason() {
        let cache = QosCache::new();
        let mut qos = make_qos("cpucap");
        qos.limits.max_tres_per_user = Some(TresRecord::parse("cpu=8").unwrap());
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
        let result = check_qos_limits(&job, &qos, 0, 0, &running, &TresRecord::new(), None);
        assert_eq!(
            result,
            QosCheckResult::Blocked(PendingReason::QosMaxCpuPerUserLimit)
        );
    }

    #[test]
    fn test_qos_from_record_parses_limits() {
        let record = crate::accounting::db::QosRecord {
            name: "high".into(),
            description: "High priority QoS".into(),
            priority: 100,
            preempt_mode: "cancel".into(),
            preempt: String::new(),
            usage_factor: 2.0,
            max_jobs_per_user: Some(10),
            max_wall_min: Some(60),
            max_tres_per_job: Some("cpu=32,mem=131072".into()),
            max_submit_per_user: Some(50),
            max_submit_per_account: Some(40),
            grp_submit_jobs: Some(30),
            max_tres_per_user: Some("cpu=64".into()),
            grp_tres: Some("gpu=8".into()),
            grp_wall_min: Some(120),
            preempt_exempt_time: None,
            flags: "DenyOnLimit".into(),
        };

        let qos = qos_from_record(record);

        assert_eq!(qos.name, "high");
        assert!(qos.deny_on_limit);
        assert_eq!(qos.limits.max_submit_jobs_per_account, Some(40));
        assert_eq!(qos.limits.grp_submit_jobs, Some(30));
        assert_eq!(qos.priority, 100);
        assert_eq!(qos.preempt_mode, QosPreemptMode::Cancel);
        assert_eq!(qos.usage_factor, 2.0);
        assert_eq!(qos.limits.max_jobs_per_user, Some(10));
        assert_eq!(qos.limits.max_wall_minutes, Some(60));
        assert_eq!(qos.limits.grp_wall_minutes, Some(120));
        assert_eq!(qos.limits.max_submit_jobs_per_user, Some(50));
        assert!(qos.limits.max_tres_per_job.is_some());
        assert_eq!(
            qos.limits
                .max_tres_per_job
                .as_ref()
                .unwrap()
                .get(TresType::Cpu),
            32
        );
        assert!(qos.limits.max_tres_per_user.is_some());
        assert_eq!(
            qos.limits
                .max_tres_per_user
                .as_ref()
                .unwrap()
                .get(TresType::Cpu),
            64
        );
        assert!(qos.limits.grp_tres.is_some());
    }

    #[test]
    fn test_qos_from_record_zero_is_literal_negative_and_null_are_unset() {
        // Post sentinel-flip: a stored 0 is a real "block all" value; NULL and a
        // stray negative are "no limit" (unset).
        let record = crate::accounting::db::QosRecord {
            name: "minimal".into(),
            description: String::new(),
            priority: 0,
            preempt_mode: "off".into(),
            preempt: String::new(),
            usage_factor: 1.0,
            max_jobs_per_user: Some(0),
            max_wall_min: None,
            max_tres_per_job: Some(String::new()),
            max_submit_per_user: Some(-1),
            max_submit_per_account: None,
            grp_submit_jobs: Some(0),
            max_tres_per_user: None,
            grp_tres: None,
            grp_wall_min: None,
            preempt_exempt_time: None,
            flags: String::new(),
        };

        let qos = qos_from_record(record);

        assert_eq!(qos.limits.max_jobs_per_user, Some(0));
        assert_eq!(qos.limits.max_wall_minutes, None);
        assert!(qos.limits.max_tres_per_job.is_none());
        assert_eq!(qos.limits.max_submit_jobs_per_user, None);
        assert_eq!(qos.limits.grp_submit_jobs, Some(0));
        assert!(qos.limits.max_tres_per_user.is_none());
        assert!(qos.limits.grp_tres.is_none());
        assert_eq!(qos.limits.grp_wall_minutes, None);
        assert!(!qos.deny_on_limit);
    }
}
