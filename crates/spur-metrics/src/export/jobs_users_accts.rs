// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Per-user and per-account job gauge registration for `/metrics/jobs-users-accts`.
//!
//! Opt-in (`metrics.high_cardinality`): each user/account is its own time series.

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use spur_core::job::JobState;
use std::sync::atomic::AtomicU64;

use crate::export::encode_registered;
use crate::export::jobs::job_state_metric_suffix;
use crate::job::JobMetricsSnapshot;
use crate::user_acct::UserAcctMetricsSnapshot;

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct UserLabel {
    username: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct AccountLabel {
    account: String,
}

type LabeledGauge<L> = Family<L, Gauge<u64, AtomicU64>>;

fn register_entity_gauges<L, F>(
    registry: &mut Registry,
    prefix: &str,
    entities: &[(String, JobMetricsSnapshot)],
    label_for: F,
) where
    L: Clone + std::hash::Hash + Eq + EncodeLabelSet + 'static + std::fmt::Debug + Send + Sync,
    F: Fn(&str) -> L,
{
    let total = LabeledGauge::<L>::default();
    let cpus_alloc = LabeledGauge::<L>::default();
    let memory_alloc_bytes = LabeledGauge::<L>::default();
    let gpus_alloc = LabeledGauge::<L>::default();
    let mut state_families: Vec<(JobState, LabeledGauge<L>)> = JobState::ALL
        .iter()
        .map(|&s| (s, Family::default()))
        .collect();

    for (name, snap) in entities {
        let label = label_for(name);
        total.get_or_create(&label).set(snap.total);
        cpus_alloc.get_or_create(&label).set(snap.running_cpus);
        memory_alloc_bytes
            .get_or_create(&label)
            .set(snap.running_memory_bytes);
        gpus_alloc.get_or_create(&label).set(snap.running_gpus);
        for (state, family) in &state_families {
            family.get_or_create(&label).set(snap.count_state(*state));
        }
    }

    registry.register(format!("spur_{prefix}_jobs"), "Total number of jobs", total);
    registry.register(
        format!("spur_{prefix}_jobs_cpus_alloc"),
        "Total CPUs allocated to jobs in Running or Completing state",
        cpus_alloc,
    );
    registry.register(
        format!("spur_{prefix}_jobs_memory_alloc_bytes"),
        "Total memory in bytes allocated to jobs in Running or Completing state",
        memory_alloc_bytes,
    );
    registry.register(
        format!("spur_{prefix}_jobs_gpus_alloc"),
        "Total GPUs allocated to jobs in Running or Completing state",
        gpus_alloc,
    );
    for (state, family) in state_families.drain(..) {
        let name = format!("spur_{prefix}_jobs_{}", job_state_metric_suffix(state));
        let help = format!("Number of jobs in {} state", state.display());
        registry.register(name, help, family);
    }
}

/// Register per-user and per-account job gauges into `registry` from `snap`.
pub fn register_jobs_users_accts(registry: &mut Registry, snap: &UserAcctMetricsSnapshot) {
    register_entity_gauges(registry, "user", &snap.by_user, |username| UserLabel {
        username: username.to_string(),
    });
    register_entity_gauges(registry, "account", &snap.by_account, |account| {
        AccountLabel {
            account: account.to_string(),
        }
    });
}

/// Encode per-user/per-account job metrics as OpenMetrics 1.0 text.
pub fn encode_jobs_users_accts_metrics(snap: &UserAcctMetricsSnapshot) -> String {
    encode_registered(|registry| register_jobs_users_accts(registry, snap))
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_core::job::{Job, JobSpec};

    #[test]
    fn empty_snapshot_has_eof_only() {
        let body = encode_jobs_users_accts_metrics(&UserAcctMetricsSnapshot::default());
        assert_eq!(body, "# EOF\n");
    }

    #[test]
    fn export_contains_per_user_and_per_account_families() {
        let spec = JobSpec {
            user: "alice".into(),
            account: Some("teamA".into()),
            ..Default::default()
        };
        let mut job = Job::new(1, spec);
        job.state = JobState::Running;

        let snap = UserAcctMetricsSnapshot::collect([&job]);
        let body = encode_jobs_users_accts_metrics(&snap);

        assert!(body.contains("spur_user_jobs{username=\"alice\"} 1\n"));
        assert!(body.contains("spur_user_jobs_running{username=\"alice\"} 1\n"));
        assert!(body.contains("spur_account_jobs{account=\"teamA\"} 1\n"));
        assert!(body.contains("spur_account_jobs_running{account=\"teamA\"} 1\n"));
    }
}
