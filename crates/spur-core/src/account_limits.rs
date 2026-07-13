// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Account/association resource limit enforcement.
//!
//! Mirrors `qos::check_qos_limits` one layer up the hierarchy: limits here
//! come from `AccountLimits` on a user's `Association` with an account,
//! rather than from a `Qos`. Unlike QOS, associations have no separate
//! per-user TRES cap distinct from the per-job one — `max_tres_per_job`
//! bounds a single job and `grp_tres` bounds the account's aggregate usage
//! across all its users.

use crate::accounting::{AccountLimits, TresRecord, TresType};
use crate::job::{Job, PendingReason};

/// Result of an account/association limit check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountCheckResult {
    /// Job passes all account/association checks.
    Allowed,
    /// Job blocked by an account/association limit.
    Blocked(PendingReason),
}

/// Check if a job would violate its account/association limits.
///
/// `user_running_count`/`user_submitted_count` are the requesting user's
/// running/(pending+running) job count under this account; `account_running_tres`
/// aggregates all running jobs across every user in the account (for `grp_tres`).
pub fn check_account_limits(
    job: &Job,
    limits: &AccountLimits,
    user_running_count: u32,
    user_submitted_count: u32,
    account_running_tres: &TresRecord,
) -> AccountCheckResult {
    if let Some(max) = limits.max_running_jobs {
        if user_running_count >= max {
            return AccountCheckResult::Blocked(PendingReason::AssocMaxJobsLimit);
        }
    }

    if let Some(max) = limits.max_submit_jobs {
        if user_submitted_count >= max {
            return AccountCheckResult::Blocked(PendingReason::AssocMaxSubmitJobLimit);
        }
    }

    if let Some(max_wall) = limits.max_wall_minutes {
        if let Some(job_wall) = job.spec.time_limit {
            if job_wall.num_minutes() > max_wall as i64 {
                return AccountCheckResult::Blocked(PendingReason::AssocMaxWallDurationPerJobLimit);
            }
        }
    }

    if let Some(ref max_tres) = limits.max_tres_per_job {
        let job_cpus = (job.spec.num_tasks * job.spec.cpus_per_task) as u64;
        if max_tres.get(TresType::Cpu) > 0 && job_cpus > max_tres.get(TresType::Cpu) {
            return AccountCheckResult::Blocked(PendingReason::AssocMaxCpuPerJobLimit);
        }

        let job_nodes = job.spec.num_nodes as u64;
        if max_tres.get(TresType::Node) > 0 && job_nodes > max_tres.get(TresType::Node) {
            return AccountCheckResult::Blocked(PendingReason::AssocMaxNodePerJobLimit);
        }

        if let Some(mem) = job.spec.memory_per_node_mb {
            let total_mem = mem * job.spec.num_nodes as u64;
            if max_tres.get(TresType::Memory) > 0 && total_mem > max_tres.get(TresType::Memory) {
                return AccountCheckResult::Blocked(PendingReason::AssocMaxMemPerJob);
            }
        }
    }

    if let Some(ref grp) = limits.grp_tres {
        let job_cpus = (job.spec.num_tasks * job.spec.cpus_per_task) as u64;
        if grp.get(TresType::Cpu) > 0
            && account_running_tres.get(TresType::Cpu) + job_cpus > grp.get(TresType::Cpu)
        {
            return AccountCheckResult::Blocked(PendingReason::AssocGrpCpuLimit);
        }

        let job_nodes = job.spec.num_nodes as u64;
        if grp.get(TresType::Node) > 0
            && account_running_tres.get(TresType::Node) + job_nodes > grp.get(TresType::Node)
        {
            return AccountCheckResult::Blocked(PendingReason::AssocGrpNodeLimit);
        }

        if let Some(mem) = job.spec.memory_per_node_mb {
            let job_mem = mem * job.spec.num_nodes as u64;
            if grp.get(TresType::Memory) > 0
                && account_running_tres.get(TresType::Memory) + job_mem > grp.get(TresType::Memory)
            {
                return AccountCheckResult::Blocked(PendingReason::AssocGrpMemLimit);
            }
        }
    }

    AccountCheckResult::Allowed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::JobSpec;

    fn make_test_job() -> Job {
        Job::new(
            1,
            JobSpec {
                name: "test".into(),
                user: "alice".into(),
                account: Some("research".into()),
                num_tasks: 4,
                cpus_per_task: 1,
                time_limit: Some(chrono::Duration::hours(2)),
                ..Default::default()
            },
        )
    }

    #[test]
    fn test_allowed_when_no_limits() {
        let job = make_test_job();
        let result =
            check_account_limits(&job, &AccountLimits::default(), 0, 0, &TresRecord::new());
        assert_eq!(result, AccountCheckResult::Allowed);
    }

    #[test]
    fn test_blocked_by_max_running_jobs() {
        let limits = AccountLimits {
            max_running_jobs: Some(5),
            ..Default::default()
        };
        let job = make_test_job();
        let result = check_account_limits(&job, &limits, 5, 5, &TresRecord::new());
        assert_eq!(
            result,
            AccountCheckResult::Blocked(PendingReason::AssocMaxJobsLimit)
        );
    }

    #[test]
    fn test_allowed_under_max_running_jobs() {
        let limits = AccountLimits {
            max_running_jobs: Some(5),
            ..Default::default()
        };
        let job = make_test_job();
        let result = check_account_limits(&job, &limits, 3, 3, &TresRecord::new());
        assert_eq!(result, AccountCheckResult::Allowed);
    }

    #[test]
    fn test_blocked_by_max_submit_jobs() {
        let limits = AccountLimits {
            max_submit_jobs: Some(3),
            ..Default::default()
        };
        let job = make_test_job();
        let result = check_account_limits(&job, &limits, 0, 3, &TresRecord::new());
        assert_eq!(
            result,
            AccountCheckResult::Blocked(PendingReason::AssocMaxSubmitJobLimit)
        );
    }

    #[test]
    fn test_blocked_by_max_wall() {
        let limits = AccountLimits {
            max_wall_minutes: Some(60), // 1 hour max
            ..Default::default()
        };
        let job = make_test_job(); // 2 hour job
        let result = check_account_limits(&job, &limits, 0, 0, &TresRecord::new());
        assert_eq!(
            result,
            AccountCheckResult::Blocked(PendingReason::AssocMaxWallDurationPerJobLimit)
        );
    }

    #[test]
    fn test_blocked_by_max_cpu_per_job() {
        let mut tres = TresRecord::new();
        tres.set(TresType::Cpu, 2); // max 2 CPUs per job
        let limits = AccountLimits {
            max_tres_per_job: Some(tres),
            ..Default::default()
        };
        let job = make_test_job(); // needs 4 CPUs
        let result = check_account_limits(&job, &limits, 0, 0, &TresRecord::new());
        assert_eq!(
            result,
            AccountCheckResult::Blocked(PendingReason::AssocMaxCpuPerJobLimit)
        );
    }

    #[test]
    fn test_blocked_by_max_node_per_job() {
        let mut tres = TresRecord::new();
        tres.set(TresType::Node, 1); // max 1 node per job
        let limits = AccountLimits {
            max_tres_per_job: Some(tres),
            ..Default::default()
        };
        let mut job = make_test_job();
        job.spec.num_nodes = 4;
        let result = check_account_limits(&job, &limits, 0, 0, &TresRecord::new());
        assert_eq!(
            result,
            AccountCheckResult::Blocked(PendingReason::AssocMaxNodePerJobLimit)
        );
    }

    #[test]
    fn test_blocked_by_max_mem_per_job() {
        let mut tres = TresRecord::new();
        tres.set(TresType::Memory, 1024); // max 1 GiB per job
        let limits = AccountLimits {
            max_tres_per_job: Some(tres),
            ..Default::default()
        };
        let mut job = make_test_job();
        job.spec.num_nodes = 1;
        job.spec.memory_per_node_mb = Some(2048); // 2 GiB
        let result = check_account_limits(&job, &limits, 0, 0, &TresRecord::new());
        assert_eq!(
            result,
            AccountCheckResult::Blocked(PendingReason::AssocMaxMemPerJob)
        );
    }

    #[test]
    fn test_blocked_by_grp_cpu() {
        let mut grp = TresRecord::new();
        grp.set(TresType::Cpu, 8); // account-wide cap 8
        let limits = AccountLimits {
            grp_tres: Some(grp),
            ..Default::default()
        };
        let job = make_test_job(); // needs 4 CPUs
        let mut running = TresRecord::new();
        running.set(TresType::Cpu, 6); // 6 already running in the account; 6 + 4 > 8
        let result = check_account_limits(&job, &limits, 0, 0, &running);
        assert_eq!(
            result,
            AccountCheckResult::Blocked(PendingReason::AssocGrpCpuLimit)
        );
    }

    #[test]
    fn test_blocked_by_grp_node() {
        let mut grp = TresRecord::new();
        grp.set(TresType::Node, 4); // account-wide cap 4 nodes
        let limits = AccountLimits {
            grp_tres: Some(grp),
            ..Default::default()
        };
        let mut job = make_test_job();
        job.spec.num_nodes = 3;
        let mut running = TresRecord::new();
        running.set(TresType::Node, 2); // 2 nodes already running; 2 + 3 > 4
        let result = check_account_limits(&job, &limits, 0, 0, &running);
        assert_eq!(
            result,
            AccountCheckResult::Blocked(PendingReason::AssocGrpNodeLimit)
        );
    }

    #[test]
    fn test_blocked_by_grp_mem() {
        let mut grp = TresRecord::new();
        grp.set(TresType::Memory, 4096); // account-wide cap 4 GiB
        let limits = AccountLimits {
            grp_tres: Some(grp),
            ..Default::default()
        };
        let mut job = make_test_job();
        job.spec.num_nodes = 1;
        job.spec.memory_per_node_mb = Some(3000); // job needs 3 GiB
        let mut running = TresRecord::new();
        running.set(TresType::Memory, 2000); // already using 2 GiB; 2000 + 3000 > 4096
        let result = check_account_limits(&job, &limits, 0, 0, &running);
        assert_eq!(
            result,
            AccountCheckResult::Blocked(PendingReason::AssocGrpMemLimit)
        );
    }
}
