// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;

use spur_core::job::Job;

use crate::job::JobMetricsSnapshot;

/// Per-user and per-account job metrics, opt-in via `metrics.high_cardinality`
/// since each distinct user/account becomes its own time series.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserAcctMetricsSnapshot {
    pub by_user: Vec<(String, JobMetricsSnapshot)>,
    pub by_account: Vec<(String, JobMetricsSnapshot)>,
}

impl UserAcctMetricsSnapshot {
    /// Group jobs by `spec.user` and by `spec.account` (jobs without an
    /// account are excluded from the account breakdown, not double-counted
    /// under an empty key).
    pub fn collect<'a>(jobs: impl IntoIterator<Item = &'a Job> + Clone) -> Self {
        let mut by_user: BTreeMap<&str, Vec<&Job>> = BTreeMap::new();
        let mut by_account: BTreeMap<&str, Vec<&Job>> = BTreeMap::new();

        for job in jobs.clone() {
            by_user.entry(&job.spec.user).or_default().push(job);
            if let Some(account) = job.spec.account.as_deref().filter(|a| !a.is_empty()) {
                by_account.entry(account).or_default().push(job);
            }
        }

        Self {
            by_user: by_user
                .into_iter()
                .map(|(user, jobs)| (user.to_string(), JobMetricsSnapshot::collect(jobs)))
                .collect(),
            by_account: by_account
                .into_iter()
                .map(|(account, jobs)| (account.to_string(), JobMetricsSnapshot::collect(jobs)))
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_core::job::{JobSpec, JobState};

    fn job_for(id: u32, user: &str, account: Option<&str>, state: JobState) -> Job {
        let spec = JobSpec {
            user: user.into(),
            account: account.map(String::from),
            ..Default::default()
        };
        let mut job = Job::new(id, spec);
        job.state = state;
        job
    }

    #[test]
    fn groups_by_user_and_account() {
        let jobs = [
            job_for(1, "alice", Some("teamA"), JobState::Running),
            job_for(2, "alice", Some("teamA"), JobState::Pending),
            job_for(3, "bob", None, JobState::Running),
        ];
        let snap = UserAcctMetricsSnapshot::collect(jobs.iter());

        assert_eq!(snap.by_user.len(), 2);
        let alice = &snap.by_user.iter().find(|(u, _)| u == "alice").unwrap().1;
        assert_eq!(alice.total, 2);
        assert_eq!(alice.count_state(JobState::Running), 1);
        assert_eq!(alice.count_state(JobState::Pending), 1);

        assert_eq!(snap.by_account.len(), 1);
        let team_a = &snap.by_account[0];
        assert_eq!(team_a.0, "teamA");
        assert_eq!(team_a.1.total, 2);
    }

    #[test]
    fn jobs_without_account_excluded_from_account_breakdown() {
        let jobs = [job_for(1, "bob", None, JobState::Running)];
        let snap = UserAcctMetricsSnapshot::collect(jobs.iter());
        assert_eq!(snap.by_user.len(), 1);
        assert!(snap.by_account.is_empty());
    }

    #[test]
    fn empty_jobs() {
        let snap = UserAcctMetricsSnapshot::collect([]);
        assert_eq!(snap, UserAcctMetricsSnapshot::default());
    }
}
