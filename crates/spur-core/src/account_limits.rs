// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Account/association resource limit enforcement.
//!
//! Mirrors `qos::check_qos_limits` one layer up the hierarchy: limits here
//! come from `AccountLimits` on a user's association with an account,
//! rather than from a `Qos`. Unlike QOS, associations have no separate
//! per-user TRES cap distinct from the per-job one — `max_tres_per_job`
//! bounds a single job and `grp_tres` bounds the account's aggregate usage
//! across all its users.

use crate::accounting::{AccountLimits, TresRecord, TresType};
use crate::job::{effective_gpus, effective_memory_mb, Job, PendingReason};

/// Result of an account/association limit check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountCheckResult {
    /// Job passes all account/association checks.
    Allowed,
    /// Job blocked by an account/association limit.
    Blocked(PendingReason),
}

/// Whether the job's requested wall time exceeds `max_wall` minutes. A cap of 0
/// is "block all" per the limit convention, so it breaches even a job with no
/// explicit time limit.
fn wall_breach(job: &Job, max_wall: u32) -> bool {
    if max_wall == 0 {
        return true;
    }
    job.spec
        .time_limit
        .is_some_and(|w| w.num_minutes() > max_wall as i64)
}

/// The four TRES quantities a single job requests: (cpu, node, mem_mb, gpu).
fn job_tres(job: &Job) -> (u64, u64, u64, u64) {
    let cpus = (job.spec.num_tasks * job.spec.cpus_per_task) as u64;
    let nodes = job.spec.num_nodes as u64;
    let mem = effective_memory_mb(&job.spec, job.spec.num_nodes);
    let gpus = effective_gpus(&job.spec, job.spec.num_nodes);
    (cpus, nodes, mem, gpus)
}

/// Standalone TRES breach for an association: does this job, folded onto
/// `account_running_tres` (empty for the submit-time standalone evaluation),
/// exceed the per-job or account-group cap? Wall is handled separately because
/// the association wall cap denies unconditionally at submission.
fn account_resource_breach(
    job: &Job,
    limits: &AccountLimits,
    account_running_tres: &TresRecord,
) -> Option<PendingReason> {
    let (job_cpu, job_node, job_mem, job_gpu) = job_tres(job);

    if let Some(ref max_tres) = limits.max_tres_per_job {
        if max_tres.get(TresType::Cpu) > 0 && job_cpu > max_tres.get(TresType::Cpu) {
            return Some(PendingReason::AssocMaxCpuPerJobLimit);
        }
        if max_tres.get(TresType::Node) > 0 && job_node > max_tres.get(TresType::Node) {
            return Some(PendingReason::AssocMaxNodePerJobLimit);
        }
        if max_tres.get(TresType::Memory) > 0 && job_mem > max_tres.get(TresType::Memory) {
            return Some(PendingReason::AssocMaxMemPerJob);
        }
        if max_tres.get(TresType::Gpu) > 0 && job_gpu > max_tres.get(TresType::Gpu) {
            return Some(PendingReason::AssocMaxGpuPerJobLimit);
        }
    }

    if let Some(ref grp) = limits.grp_tres {
        if grp.get(TresType::Cpu) > 0
            && account_running_tres.get(TresType::Cpu) + job_cpu > grp.get(TresType::Cpu)
        {
            return Some(PendingReason::AssocGrpCpuLimit);
        }
        if grp.get(TresType::Node) > 0
            && account_running_tres.get(TresType::Node) + job_node > grp.get(TresType::Node)
        {
            return Some(PendingReason::AssocGrpNodeLimit);
        }
        if grp.get(TresType::Memory) > 0
            && account_running_tres.get(TresType::Memory) + job_mem > grp.get(TresType::Memory)
        {
            return Some(PendingReason::AssocGrpMemLimit);
        }
        if grp.get(TresType::Gpu) > 0
            && account_running_tres.get(TresType::Gpu) + job_gpu > grp.get(TresType::Gpu)
        {
            return Some(PendingReason::AssocGrpGpuLimit);
        }
    }

    None
}

/// Check if a job would violate its account/association limits (scheduler path).
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
        if wall_breach(job, max_wall) {
            return AccountCheckResult::Blocked(PendingReason::AssocMaxWallDurationPerJobLimit);
        }
    }

    match account_resource_breach(job, limits, account_running_tres) {
        Some(reason) => AccountCheckResult::Blocked(reason),
        None => AccountCheckResult::Allowed,
    }
}

/// Association submit-count limits (`MaxSubmitJobs`, `GrpSubmitJobs`). These
/// always deny at submission, independent of `DenyOnLimit`. `incoming` is how
/// many jobs this submission adds (array size or 1).
pub fn check_account_submit_limits(
    limits: &AccountLimits,
    user_submitted: u32,
    account_submitted: u32,
    incoming: u32,
) -> Option<PendingReason> {
    if let Some(max) = limits.max_submit_jobs {
        if user_submitted.saturating_add(incoming) > max {
            return Some(PendingReason::AssocMaxSubmitJobLimit);
        }
    }
    if let Some(max) = limits.grp_submit_jobs {
        if account_submitted.saturating_add(incoming) > max {
            return Some(PendingReason::AssocGrpSubmitJobsLimit);
        }
    }
    None
}

/// Association `MaxWallDurationPerJob` breach. Slurm denies this
/// unconditionally at submission (it does not depend on `DenyOnLimit`).
pub fn check_account_wall_limit(job: &Job, limits: &AccountLimits) -> Option<PendingReason> {
    match limits.max_wall_minutes {
        Some(max_wall) if wall_breach(job, max_wall) => {
            Some(PendingReason::AssocMaxWallDurationPerJobLimit)
        }
        _ => None,
    }
}

/// Standalone association TRES breach for the submission gate: evaluates the
/// job on its own. Denied at submit only when the governing QOS has
/// `DenyOnLimit`; otherwise the scheduler pends it.
pub fn check_account_standalone_limits(job: &Job, limits: &AccountLimits) -> Option<PendingReason> {
    account_resource_breach(job, limits, &TresRecord::new())
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
    fn max_wall_zero_blocks_time_less_job() {
        let limits = AccountLimits {
            max_wall_minutes: Some(0), // 0 = block all
            ..Default::default()
        };
        let mut job = make_test_job();
        job.spec.time_limit = None;
        assert_eq!(
            check_account_wall_limit(&job, &limits),
            Some(PendingReason::AssocMaxWallDurationPerJobLimit)
        );
    }

    #[test]
    fn positive_max_wall_leaves_time_less_job_alone() {
        let limits = AccountLimits {
            max_wall_minutes: Some(60),
            ..Default::default()
        };
        let mut job = make_test_job();
        job.spec.time_limit = None;
        assert_eq!(check_account_wall_limit(&job, &limits), None);
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
    fn test_blocked_by_max_mem_per_job_with_mem_per_cpu() {
        let mut tres = TresRecord::new();
        tres.set(TresType::Memory, 1024); // max 1 GiB per job
        let limits = AccountLimits {
            max_tres_per_job: Some(tres),
            ..Default::default()
        };
        // 4 tasks * 1 cpu/task * 512 MB/cpu == 2 GiB total, same as the
        // memory_per_node_mb equivalent above.
        let mut job = make_test_job();
        job.spec.num_nodes = 1;
        job.spec.memory_per_cpu_mb = Some(512);
        let result = check_account_limits(&job, &limits, 0, 0, &TresRecord::new());
        assert_eq!(
            result,
            AccountCheckResult::Blocked(PendingReason::AssocMaxMemPerJob)
        );
    }

    #[test]
    fn test_blocked_by_max_gpu_per_job() {
        let mut tres = TresRecord::new();
        tres.set(TresType::Gpu, 2); // max 2 GPUs per job
        let limits = AccountLimits {
            max_tres_per_job: Some(tres),
            ..Default::default()
        };
        let mut job = make_test_job();
        job.spec.num_nodes = 1;
        job.spec.gres = vec!["gpu:4".into()]; // needs 4 GPUs
        let result = check_account_limits(&job, &limits, 0, 0, &TresRecord::new());
        assert_eq!(
            result,
            AccountCheckResult::Blocked(PendingReason::AssocMaxGpuPerJobLimit)
        );
    }

    #[test]
    fn test_blocked_by_grp_gpu() {
        let mut grp = TresRecord::new();
        grp.set(TresType::Gpu, 8); // account-wide cap 8 GPUs
        let limits = AccountLimits {
            grp_tres: Some(grp),
            ..Default::default()
        };
        let mut job = make_test_job();
        job.spec.num_nodes = 1;
        job.spec.gres = vec!["gpu:4".into()];
        let mut running = TresRecord::new();
        running.set(TresType::Gpu, 6); // 6 already running in the account; 6 + 4 > 8
        let result = check_account_limits(&job, &limits, 0, 0, &running);
        assert_eq!(
            result,
            AccountCheckResult::Blocked(PendingReason::AssocGrpGpuLimit)
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

    #[test]
    fn test_blocked_by_grp_mem_with_mem_per_cpu() {
        let mut grp = TresRecord::new();
        grp.set(TresType::Memory, 4096); // account-wide cap 4 GiB
        let limits = AccountLimits {
            grp_tres: Some(grp),
            ..Default::default()
        };
        // 4 tasks * 1 cpu/task * 750 MB/cpu == 3 GiB, same as the
        // memory_per_node_mb equivalent above.
        let mut job = make_test_job();
        job.spec.num_nodes = 1;
        job.spec.memory_per_cpu_mb = Some(750);
        let mut running = TresRecord::new();
        running.set(TresType::Memory, 2000); // already using 2 GiB; 2000 + 3000 > 4096
        let result = check_account_limits(&job, &limits, 0, 0, &running);
        assert_eq!(
            result,
            AccountCheckResult::Blocked(PendingReason::AssocGrpMemLimit)
        );
    }
}
