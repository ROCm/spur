// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! QOS enforcement logic.
//!
//! Checks per-QOS limits before allowing a job to be scheduled.

use crate::accounting::{Qos, QosPreemptMode, TresRecord, TresType};
use crate::job::{effective_gpus, effective_memory_mb, Job, JobSpec, PendingReason};
use crate::partition::PreemptMode;

impl From<QosPreemptMode> for PreemptMode {
    fn from(mode: QosPreemptMode) -> Self {
        match mode {
            QosPreemptMode::Off => PreemptMode::Off,
            QosPreemptMode::Cancel => PreemptMode::Cancel,
            QosPreemptMode::Requeue => PreemptMode::Requeue,
            QosPreemptMode::Suspend => PreemptMode::Suspend,
        }
    }
}

/// A QOS-level preempt mode override, or `None` if unset. `Off` can't be
/// told apart from "unset" on the wire, so it's treated as no override.
pub fn qos_preempt_override(qos: &Qos) -> Option<PreemptMode> {
    match qos.preempt_mode {
        QosPreemptMode::Off => None,
        other => Some(other.into()),
    }
}

/// Result of QOS limit check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QosCheckResult {
    /// Job passes all QOS checks.
    Allowed,
    /// Job blocked by a QOS limit.
    Blocked(PendingReason),
}

/// Whether the job's requested wall time exceeds `max_wall` minutes. A cap of 0
/// is "block all" per the limit convention, so it breaches even a job with no
/// explicit time limit.
fn wall_breach(spec: &JobSpec, max_wall: u32) -> bool {
    if max_wall == 0 {
        return true;
    }
    spec.time_limit
        .is_some_and(|w| w.num_minutes() > max_wall as i64)
}

/// The four TRES quantities a single job requests: (cpu, node, mem_mb, gpu).
fn job_tres(spec: &JobSpec) -> (u64, u64, u64, u64) {
    let cpus = (spec.num_tasks * spec.cpus_per_task) as u64;
    let nodes = spec.num_nodes as u64;
    let mem = effective_memory_mb(spec, spec.num_nodes);
    let gpus = effective_gpus(spec, spec.num_nodes);
    (cpus, nodes, mem, gpus)
}

/// Per-job TRES cap breach. Each dimension is checked only when the cap is set
/// (> 0). `reasons` names the reason for (cpu, node, mem, gpu) so callers can
/// distinguish the per-job vs per-user/grp variants.
fn tres_cap_breach(
    job_cpu: u64,
    job_node: u64,
    job_mem: u64,
    job_gpu: u64,
    cap: &TresRecord,
    reasons: (PendingReason, PendingReason, PendingReason, PendingReason),
) -> Option<PendingReason> {
    if cap.get(TresType::Cpu) > 0 && job_cpu > cap.get(TresType::Cpu) {
        return Some(reasons.0);
    }
    if cap.get(TresType::Node) > 0 && job_node > cap.get(TresType::Node) {
        return Some(reasons.1);
    }
    if cap.get(TresType::Memory) > 0 && job_mem > cap.get(TresType::Memory) {
        return Some(reasons.2);
    }
    if cap.get(TresType::Gpu) > 0 && job_gpu > cap.get(TresType::Gpu) {
        return Some(reasons.3);
    }
    None
}

/// Standalone TRES/wall breach: does this job, evaluated on its own (no other
/// load), exceed any QOS resource cap? This is what Slurm's `DenyOnLimit`
/// converts into a submission denial, and what the scheduler also re-checks
/// with real aggregates folded in. `running_*` fold in existing load; pass
/// empty records for the standalone (submit-time) evaluation.
///
/// `grp_node_charge` is the node count charged against `grp_tres` specifically
/// — distinct from `job_node` below because a caller that knows the job could
/// pack onto nodes the QOS already occupies may pass a smaller number than
/// `spec.num_nodes`. Per-job/per-user node caps always use the job's real
/// requested node count, since they bound a single job's/user's own footprint
/// rather than group-wide reuse.
fn qos_resource_breach(
    spec: &JobSpec,
    qos: &Qos,
    user_running_tres: &TresRecord,
    qos_running_tres: &TresRecord,
    grp_node_charge: u64,
) -> Option<PendingReason> {
    let limits = &qos.limits;
    let (job_cpu, job_node, job_mem, job_gpu) = job_tres(spec);

    if let Some(max_wall) = limits.max_wall_minutes {
        if wall_breach(spec, max_wall) {
            return Some(PendingReason::QosMaxWallDurationPerJobLimit);
        }
    }

    if let Some(ref max_tres) = limits.max_tres_per_job {
        if let Some(reason) = tres_cap_breach(
            job_cpu,
            job_node,
            job_mem,
            job_gpu,
            max_tres,
            (
                PendingReason::QosMaxCpuPerJobLimit,
                PendingReason::QosMaxNodePerJobLimit,
                PendingReason::QosMaxMemoryPerJob,
                PendingReason::QosMaxGpuPerJobLimit,
            ),
        ) {
            return Some(reason);
        }
    }

    if let Some(ref max_tres) = limits.max_tres_per_user {
        if let Some(reason) = tres_cap_breach(
            user_running_tres.get(TresType::Cpu) + job_cpu,
            user_running_tres.get(TresType::Node) + job_node,
            user_running_tres.get(TresType::Memory) + job_mem,
            user_running_tres.get(TresType::Gpu) + job_gpu,
            max_tres,
            (
                PendingReason::QosMaxCpuPerUserLimit,
                PendingReason::QosMaxNodePerUserLimit,
                PendingReason::QosMaxMemoryPerUser,
                PendingReason::QosMaxGpuPerUserLimit,
            ),
        ) {
            return Some(reason);
        }
    }

    if let Some(ref grp) = limits.grp_tres {
        if let Some(reason) = tres_cap_breach(
            qos_running_tres.get(TresType::Cpu) + job_cpu,
            qos_running_tres.get(TresType::Node) + grp_node_charge,
            qos_running_tres.get(TresType::Memory) + job_mem,
            qos_running_tres.get(TresType::Gpu) + job_gpu,
            grp,
            (
                PendingReason::QosGrpCpuLimit,
                PendingReason::QosGrpNodeLimit,
                PendingReason::QosGrpMemLimit,
                PendingReason::QosGrpGpuLimit,
            ),
        ) {
            return Some(reason);
        }
    }

    None
}

/// Check if a job would violate QOS limits (scheduler path).
///
/// `user_running_*` aggregate the requesting user's load; `qos_running_tres`
/// aggregates all running jobs in the QOS (for the `Grp*` group limits).
///
/// `consumed_wall_minutes` is the QOS's wall-clock consumption over the
/// configured window. `None` means the figure is unavailable (accounting
/// disabled or unreachable) and leaves `grp_wall_minutes` unapplied.
pub fn check_qos_limits(
    job: &Job,
    qos: &Qos,
    user_running_count: u32,
    user_submitted_count: u32,
    user_running_tres: &TresRecord,
    qos_running_tres: &TresRecord,
    consumed_wall_minutes: Option<u64>,
) -> QosCheckResult {
    check_qos_limits_with_grp_node_charge(
        job,
        qos,
        user_running_count,
        user_submitted_count,
        user_running_tres,
        qos_running_tres,
        consumed_wall_minutes,
        job.spec.num_nodes as u64,
    )
}

/// Like `check_qos_limits`, but the node count charged against `grp_tres` is
/// `grp_node_charge` rather than `job.spec.num_nodes` — see `qos_resource_breach`.
#[allow(clippy::too_many_arguments)]
pub fn check_qos_limits_with_grp_node_charge(
    job: &Job,
    qos: &Qos,
    user_running_count: u32,
    user_submitted_count: u32,
    user_running_tres: &TresRecord,
    qos_running_tres: &TresRecord,
    consumed_wall_minutes: Option<u64>,
    grp_node_charge: u64,
) -> QosCheckResult {
    let limits = &qos.limits;

    // Max jobs per user (running count).
    if let Some(max) = limits.max_jobs_per_user {
        if user_running_count >= max {
            return QosCheckResult::Blocked(PendingReason::QoSMaxJobsPerUser);
        }
    }

    // Max submit jobs per user. Slurm distinguishes the submit-job cap
    // (WAIT_QOS_MAX_SUB_JOB, "QOSMaxSubmitJobPerUserLimit") from the
    // running-job cap above.
    if let Some(max) = limits.max_submit_jobs_per_user {
        if user_submitted_count >= max {
            return QosCheckResult::Blocked(PendingReason::QosMaxSubmitJobPerUserLimit);
        }
    }

    // Group wall-clock budget. Unlike the TRES group limits, this does not
    // project the candidate job: Slurm blocks once the budget is reached, so a
    // job that still fits inside it must be admitted.
    if let (Some(cap), Some(consumed)) = (limits.grp_wall_minutes, consumed_wall_minutes) {
        if consumed >= cap as u64 {
            return QosCheckResult::Blocked(PendingReason::QosGrpWallLimit);
        }
    }

    match qos_resource_breach(
        &job.spec,
        qos,
        user_running_tres,
        qos_running_tres,
        grp_node_charge,
    ) {
        Some(reason) => QosCheckResult::Blocked(reason),
        None => QosCheckResult::Allowed,
    }
}

/// QOS submit-count limits (`MaxSubmitJobsPerUser`, `MaxSubmitJobsPerAccount`,
/// `GrpSubmitJobs`). These always deny at submission (Slurm's
/// `acct_policy_validate`), independent of `DenyOnLimit`, because admitting the
/// job would itself increment the counted quantity. `incoming` is how many jobs
/// this submission adds (array size or 1).
pub fn check_qos_submit_limits(
    qos: &Qos,
    user_submitted: u32,
    account_submitted: u32,
    qos_submitted: u32,
    incoming: u32,
) -> Option<PendingReason> {
    let limits = &qos.limits;
    if let Some(max) = limits.max_submit_jobs_per_user {
        if user_submitted.saturating_add(incoming) > max {
            return Some(PendingReason::QosMaxSubmitJobPerUserLimit);
        }
    }
    if let Some(max) = limits.max_submit_jobs_per_account {
        if account_submitted.saturating_add(incoming) > max {
            return Some(PendingReason::QosMaxSubmitJobPerAccountLimit);
        }
    }
    if let Some(max) = limits.grp_submit_jobs {
        if qos_submitted.saturating_add(incoming) > max {
            return Some(PendingReason::QosGrpSubmitJobsLimit);
        }
    }
    None
}

/// Standalone QOS resource/wall breach for the submission gate: evaluates the
/// job on its own (no other load). Denied at submit only when the QOS has
/// `DenyOnLimit`; otherwise the scheduler pends it.
pub fn check_qos_standalone_limits(spec: &JobSpec, qos: &Qos) -> Option<PendingReason> {
    qos_resource_breach(
        spec,
        qos,
        &TresRecord::new(),
        &TresRecord::new(),
        spec.num_nodes as u64,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounting::QosLimits;
    use crate::job::JobSpec;

    fn make_qos(max_jobs: Option<u32>, max_wall: Option<u32>) -> Qos {
        Qos {
            name: "test".into(),
            limits: QosLimits {
                max_jobs_per_user: max_jobs,
                max_wall_minutes: max_wall,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn make_test_job() -> Job {
        Job::new(
            1,
            JobSpec {
                name: "test".into(),
                user: "alice".into(),
                num_tasks: 4,
                cpus_per_task: 1,
                time_limit: Some(chrono::Duration::hours(2)),
                ..Default::default()
            },
        )
    }

    #[test]
    fn test_allowed_when_no_limits() {
        let qos = make_qos(None, None);
        let job = make_test_job();
        let result = check_qos_limits(
            &job,
            &qos,
            0,
            0,
            &TresRecord::new(),
            &TresRecord::new(),
            None,
        );
        assert_eq!(result, QosCheckResult::Allowed);
    }

    #[test]
    fn test_blocked_by_max_jobs() {
        let qos = make_qos(Some(5), None);
        let job = make_test_job();
        let result = check_qos_limits(
            &job,
            &qos,
            5,
            5,
            &TresRecord::new(),
            &TresRecord::new(),
            None,
        );
        assert_eq!(
            result,
            QosCheckResult::Blocked(PendingReason::QoSMaxJobsPerUser)
        );
    }

    #[test]
    fn test_allowed_under_max_jobs() {
        let qos = make_qos(Some(5), None);
        let job = make_test_job();
        let result = check_qos_limits(
            &job,
            &qos,
            3,
            3,
            &TresRecord::new(),
            &TresRecord::new(),
            None,
        );
        assert_eq!(result, QosCheckResult::Allowed);
    }

    #[test]
    fn test_blocked_by_max_wall() {
        let qos = make_qos(None, Some(60)); // 1 hour max
        let job = make_test_job(); // 2 hour job
        let result = check_qos_limits(
            &job,
            &qos,
            0,
            0,
            &TresRecord::new(),
            &TresRecord::new(),
            None,
        );
        assert_eq!(
            result,
            QosCheckResult::Blocked(PendingReason::QosMaxWallDurationPerJobLimit)
        );
    }

    #[test]
    fn max_wall_zero_blocks_time_less_job() {
        let qos = make_qos(None, Some(0)); // 0 = block all
        let mut job = make_test_job();
        job.spec.time_limit = None;
        assert_eq!(
            check_qos_standalone_limits(&job.spec, &qos),
            Some(PendingReason::QosMaxWallDurationPerJobLimit)
        );
    }

    #[test]
    fn positive_max_wall_leaves_time_less_job_alone() {
        let qos = make_qos(None, Some(60));
        let mut job = make_test_job();
        job.spec.time_limit = None;
        assert_eq!(check_qos_standalone_limits(&job.spec, &qos), None);
    }

    #[test]
    fn test_blocked_by_max_tres_per_job() {
        let mut tres = TresRecord::new();
        tres.set(TresType::Cpu, 2); // Max 2 CPUs per job
        let qos = Qos {
            name: "restricted".into(),
            limits: QosLimits {
                max_tres_per_job: Some(tres),
                ..Default::default()
            },
            ..Default::default()
        };
        let job = make_test_job(); // 4 CPUs
        let result = check_qos_limits(
            &job,
            &qos,
            0,
            0,
            &TresRecord::new(),
            &TresRecord::new(),
            None,
        );
        assert_eq!(
            result,
            QosCheckResult::Blocked(PendingReason::QosMaxCpuPerJobLimit)
        );
    }

    #[test]
    fn test_blocked_by_max_mem_per_job() {
        let mut tres = TresRecord::new();
        tres.set(TresType::Memory, 1024); // Max 1 GiB per job
        let qos = Qos {
            name: "restricted".into(),
            limits: QosLimits {
                max_tres_per_job: Some(tres),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut job = make_test_job();
        job.spec.num_nodes = 1;
        job.spec.memory_per_node_mb = Some(2048); // 2 GiB
        let result = check_qos_limits(
            &job,
            &qos,
            0,
            0,
            &TresRecord::new(),
            &TresRecord::new(),
            None,
        );
        assert_eq!(
            result,
            QosCheckResult::Blocked(PendingReason::QosMaxMemoryPerJob)
        );
    }

    #[test]
    fn test_blocked_by_max_mem_per_job_with_mem_per_cpu() {
        let mut tres = TresRecord::new();
        tres.set(TresType::Memory, 1024); // Max 1 GiB per job
        let qos = Qos {
            name: "restricted".into(),
            limits: QosLimits {
                max_tres_per_job: Some(tres),
                ..Default::default()
            },
            ..Default::default()
        };
        // 4 tasks * 1 cpu/task * 512 MB/cpu == 2 GiB total, same as the
        // memory_per_node_mb equivalent above.
        let mut job = make_test_job();
        job.spec.num_nodes = 1;
        job.spec.memory_per_cpu_mb = Some(512);
        let result = check_qos_limits(
            &job,
            &qos,
            0,
            0,
            &TresRecord::new(),
            &TresRecord::new(),
            None,
        );
        assert_eq!(
            result,
            QosCheckResult::Blocked(PendingReason::QosMaxMemoryPerJob)
        );
    }

    #[test]
    fn test_blocked_by_max_cpu_per_user() {
        let mut tres = TresRecord::new();
        tres.set(TresType::Cpu, 8); // Max 8 CPUs across the user's running jobs
        let qos = Qos {
            name: "restricted".into(),
            limits: QosLimits {
                max_tres_per_user: Some(tres),
                ..Default::default()
            },
            ..Default::default()
        };
        let job = make_test_job(); // needs 4 CPUs
        let mut running = TresRecord::new();
        running.set(TresType::Cpu, 6); // already using 6; 6 + 4 > 8
        let result = check_qos_limits(&job, &qos, 0, 0, &running, &TresRecord::new(), None);
        assert_eq!(
            result,
            QosCheckResult::Blocked(PendingReason::QosMaxCpuPerUserLimit)
        );
    }

    #[test]
    fn test_blocked_by_max_node_per_user() {
        let mut tres = TresRecord::new();
        tres.set(TresType::Node, 4); // Max 4 nodes across the user's running jobs
        let qos = Qos {
            name: "restricted".into(),
            limits: QosLimits {
                max_tres_per_user: Some(tres),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut job = make_test_job();
        job.spec.num_nodes = 3; // needs 3 nodes
        let mut running = TresRecord::new();
        running.set(TresType::Node, 2); // already using 2; 2 + 3 > 4
        let result = check_qos_limits(&job, &qos, 0, 0, &running, &TresRecord::new(), None);
        assert_eq!(
            result,
            QosCheckResult::Blocked(PendingReason::QosMaxNodePerUserLimit)
        );
    }

    #[test]
    fn test_blocked_by_max_memory_per_user() {
        let mut tres = TresRecord::new();
        tres.set(TresType::Memory, 4096); // Max 4 GiB across the user's running jobs
        let qos = Qos {
            name: "restricted".into(),
            limits: QosLimits {
                max_tres_per_user: Some(tres),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut job = make_test_job();
        job.spec.num_nodes = 1;
        job.spec.memory_per_node_mb = Some(3000); // job needs 3 GiB
        let mut running = TresRecord::new();
        running.set(TresType::Memory, 2000); // already using 2 GiB; 2000 + 3000 > 4096
        let result = check_qos_limits(&job, &qos, 0, 0, &running, &TresRecord::new(), None);
        assert_eq!(
            result,
            QosCheckResult::Blocked(PendingReason::QosMaxMemoryPerUser)
        );
    }

    #[test]
    fn test_blocked_by_max_memory_per_user_with_mem_per_cpu() {
        let mut tres = TresRecord::new();
        tres.set(TresType::Memory, 4096); // Max 4 GiB across the user's running jobs
        let qos = Qos {
            name: "restricted".into(),
            limits: QosLimits {
                max_tres_per_user: Some(tres),
                ..Default::default()
            },
            ..Default::default()
        };
        // 4 tasks * 1 cpu/task * 750 MB/cpu == 3 GiB, same as the
        // memory_per_node_mb equivalent above.
        let mut job = make_test_job();
        job.spec.num_nodes = 1;
        job.spec.memory_per_cpu_mb = Some(750);
        let mut running = TresRecord::new();
        running.set(TresType::Memory, 2000); // already using 2 GiB; 2000 + 3000 > 4096
        let result = check_qos_limits(&job, &qos, 0, 0, &running, &TresRecord::new(), None);
        assert_eq!(
            result,
            QosCheckResult::Blocked(PendingReason::QosMaxMemoryPerUser)
        );
    }

    #[test]
    fn test_blocked_by_max_submit_jobs_per_user() {
        let qos = Qos {
            name: "restricted".into(),
            limits: QosLimits {
                max_submit_jobs_per_user: Some(3),
                ..Default::default()
            },
            ..Default::default()
        };
        let job = make_test_job();
        let result = check_qos_limits(
            &job,
            &qos,
            0,
            3,
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
    fn test_blocked_by_max_node_per_job() {
        let mut tres = TresRecord::new();
        tres.set(TresType::Node, 1); // max 1 node per job
        let qos = Qos {
            name: "restricted".into(),
            limits: QosLimits {
                max_tres_per_job: Some(tres),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut job = make_test_job();
        job.spec.num_nodes = 4;
        let result = check_qos_limits(
            &job,
            &qos,
            0,
            0,
            &TresRecord::new(),
            &TresRecord::new(),
            None,
        );
        assert_eq!(
            result,
            QosCheckResult::Blocked(PendingReason::QosMaxNodePerJobLimit)
        );
    }

    #[test]
    fn test_blocked_by_max_gpu_per_job() {
        let mut tres = TresRecord::new();
        tres.set(TresType::Gpu, 2); // max 2 GPUs per job
        let qos = Qos {
            name: "restricted".into(),
            limits: QosLimits {
                max_tres_per_job: Some(tres),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut job = make_test_job();
        job.spec.num_nodes = 1;
        job.spec.gres = vec!["gpu:4".into()]; // needs 4 GPUs
        let result = check_qos_limits(
            &job,
            &qos,
            0,
            0,
            &TresRecord::new(),
            &TresRecord::new(),
            None,
        );
        assert_eq!(
            result,
            QosCheckResult::Blocked(PendingReason::QosMaxGpuPerJobLimit)
        );
    }

    #[test]
    fn test_max_gpu_per_job_counts_all_nodes() {
        let mut tres = TresRecord::new();
        tres.set(TresType::Gpu, 4); // max 4 GPUs per job
        let qos = Qos {
            name: "restricted".into(),
            limits: QosLimits {
                max_tres_per_job: Some(tres),
                ..Default::default()
            },
            ..Default::default()
        };
        // gres is per-node, so 2 nodes * gpu:3 = 6 GPUs total > 4.
        let mut job = make_test_job();
        job.spec.num_nodes = 2;
        job.spec.gres = vec!["gpu:3".into()];
        let result = check_qos_limits(
            &job,
            &qos,
            0,
            0,
            &TresRecord::new(),
            &TresRecord::new(),
            None,
        );
        assert_eq!(
            result,
            QosCheckResult::Blocked(PendingReason::QosMaxGpuPerJobLimit)
        );
    }

    #[test]
    fn test_blocked_by_max_gpu_per_user() {
        let mut tres = TresRecord::new();
        tres.set(TresType::Gpu, 8); // max 8 GPUs across the user's running jobs
        let qos = Qos {
            name: "restricted".into(),
            limits: QosLimits {
                max_tres_per_user: Some(tres),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut job = make_test_job();
        job.spec.num_nodes = 1;
        job.spec.gres = vec!["gpu:4".into()]; // job needs 4
        let mut running = TresRecord::new();
        running.set(TresType::Gpu, 6); // already using 6; 6 + 4 > 8
        let result = check_qos_limits(&job, &qos, 0, 0, &running, &TresRecord::new(), None);
        assert_eq!(
            result,
            QosCheckResult::Blocked(PendingReason::QosMaxGpuPerUserLimit)
        );
    }

    #[test]
    fn test_gpu_typed_gres_counts_toward_limit() {
        let mut tres = TresRecord::new();
        tres.set(TresType::Gpu, 2);
        let qos = Qos {
            name: "restricted".into(),
            limits: QosLimits {
                max_tres_per_job: Some(tres),
                ..Default::default()
            },
            ..Default::default()
        };
        // Typed request "gpu:mi300x:4" must still be counted as 4 GPUs.
        let mut job = make_test_job();
        job.spec.num_nodes = 1;
        job.spec.gres = vec!["gpu:mi300x:4".into()];
        let result = check_qos_limits(
            &job,
            &qos,
            0,
            0,
            &TresRecord::new(),
            &TresRecord::new(),
            None,
        );
        assert_eq!(
            result,
            QosCheckResult::Blocked(PendingReason::QosMaxGpuPerJobLimit)
        );
    }

    #[test]
    fn test_gpu_limit_ignores_non_gpu_gres() {
        let mut tres = TresRecord::new();
        tres.set(TresType::Gpu, 1); // 1 GPU cap
        let qos = Qos {
            name: "restricted".into(),
            limits: QosLimits {
                max_tres_per_job: Some(tres),
                ..Default::default()
            },
            ..Default::default()
        };
        // A non-gpu gres request must not be counted against the GPU cap.
        let mut job = make_test_job();
        job.spec.num_nodes = 1;
        job.spec.gres = vec!["bandwidth:lustre:100".into()];
        let result = check_qos_limits(
            &job,
            &qos,
            0,
            0,
            &TresRecord::new(),
            &TresRecord::new(),
            None,
        );
        assert_eq!(result, QosCheckResult::Allowed);
    }

    #[test]
    fn test_gpu_exactly_at_cap_is_allowed() {
        // The cap uses strict `>`, so a job requesting exactly the limit
        // (per-job) with the user already at the boundary (per-user) passes.
        let mut per_job = TresRecord::new();
        per_job.set(TresType::Gpu, 4);
        let mut per_user = TresRecord::new();
        per_user.set(TresType::Gpu, 8);
        let qos = Qos {
            name: "boundary".into(),
            limits: QosLimits {
                max_tres_per_job: Some(per_job),
                max_tres_per_user: Some(per_user),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut job = make_test_job();
        job.spec.num_nodes = 1;
        job.spec.gres = vec!["gpu:4".into()]; // exactly the per-job cap
        let mut running = TresRecord::new();
        running.set(TresType::Gpu, 4); // 4 running + 4 new == 8, the per-user cap
        let result = check_qos_limits(&job, &qos, 0, 0, &running, &TresRecord::new(), None);
        assert_eq!(result, QosCheckResult::Allowed);
    }

    #[test]
    fn test_blocked_by_grp_gpu() {
        let mut grp = TresRecord::new();
        grp.set(TresType::Gpu, 8); // QOS-wide cap 8 GPUs
        let qos = Qos {
            name: "grp".into(),
            limits: QosLimits {
                grp_tres: Some(grp),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut job = make_test_job();
        job.spec.num_nodes = 1;
        job.spec.gres = vec!["gpu:4".into()];
        let mut qos_running = TresRecord::new();
        qos_running.set(TresType::Gpu, 6); // 6 already in the QOS; 6 + 4 > 8
        let result = check_qos_limits(&job, &qos, 0, 0, &TresRecord::new(), &qos_running, None);
        assert_eq!(
            result,
            QosCheckResult::Blocked(PendingReason::QosGrpGpuLimit)
        );
    }

    #[test]
    fn test_blocked_by_grp_cpu() {
        let mut grp = TresRecord::new();
        grp.set(TresType::Cpu, 8); // QOS-wide cap 8
        let qos = Qos {
            name: "grp".into(),
            limits: QosLimits {
                grp_tres: Some(grp),
                ..Default::default()
            },
            ..Default::default()
        };
        let job = make_test_job(); // needs 4 CPUs
        let mut qos_running = TresRecord::new();
        qos_running.set(TresType::Cpu, 6); // 6 already in the QOS; 6 + 4 > 8
        let result = check_qos_limits(&job, &qos, 0, 0, &TresRecord::new(), &qos_running, None);
        assert_eq!(
            result,
            QosCheckResult::Blocked(PendingReason::QosGrpCpuLimit)
        );
    }

    #[test]
    fn test_blocked_by_grp_node() {
        let mut grp = TresRecord::new();
        grp.set(TresType::Node, 4); // QOS-wide cap 4 nodes
        let qos = Qos {
            name: "grp".into(),
            limits: QosLimits {
                grp_tres: Some(grp),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut job = make_test_job();
        job.spec.num_nodes = 3;
        let mut qos_running = TresRecord::new();
        qos_running.set(TresType::Node, 2); // 2 nodes already running; 2 + 3 > 4
        let result = check_qos_limits(&job, &qos, 0, 0, &TresRecord::new(), &qos_running, None);
        assert_eq!(
            result,
            QosCheckResult::Blocked(PendingReason::QosGrpNodeLimit)
        );
    }

    #[test]
    fn test_grp_node_charge_overrides_only_grp_branch() {
        let mut grp = TresRecord::new();
        grp.set(TresType::Node, 4);
        let qos = Qos {
            name: "grp".into(),
            limits: QosLimits {
                grp_tres: Some(grp),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut job = make_test_job();
        job.spec.num_nodes = 3;
        let mut qos_running = TresRecord::new();
        qos_running.set(TresType::Node, 2); // raw request (2+3>4) would block

        // A caller that knows the job can pack onto already-occupied nodes
        // charges only 1 new node: 2 + 1 = 4 <= 4, so it's allowed.
        let result = check_qos_limits_with_grp_node_charge(
            &job,
            &qos,
            0,
            0,
            &TresRecord::new(),
            &qos_running,
            None,
            1,
        );
        assert_eq!(result, QosCheckResult::Allowed);
    }

    #[test]
    fn test_grp_node_charge_does_not_affect_max_tres_per_job() {
        let mut max_tres = TresRecord::new();
        max_tres.set(TresType::Node, 2);
        let qos = Qos {
            name: "capped".into(),
            limits: QosLimits {
                max_tres_per_job: Some(max_tres),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut job = make_test_job();
        job.spec.num_nodes = 3; // breaches the per-job cap of 2 regardless of packing

        // Even a grp_node_charge of 0 must not rescue the per-job cap: it only
        // overrides the grp_tres branch, not max_tres_per_job.
        let result = check_qos_limits_with_grp_node_charge(
            &job,
            &qos,
            0,
            0,
            &TresRecord::new(),
            &TresRecord::new(),
            None,
            0,
        );
        assert_eq!(
            result,
            QosCheckResult::Blocked(PendingReason::QosMaxNodePerJobLimit)
        );
    }

    #[test]
    fn test_blocked_by_grp_mem() {
        let mut grp = TresRecord::new();
        grp.set(TresType::Memory, 4096); // QOS-wide cap 4 GiB
        let qos = Qos {
            name: "grp".into(),
            limits: QosLimits {
                grp_tres: Some(grp),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut job = make_test_job();
        job.spec.num_nodes = 1;
        job.spec.memory_per_node_mb = Some(3000); // job needs 3 GiB
        let mut qos_running = TresRecord::new();
        qos_running.set(TresType::Memory, 2000); // 2 GiB already running; 2000 + 3000 > 4096
        let result = check_qos_limits(&job, &qos, 0, 0, &TresRecord::new(), &qos_running, None);
        assert_eq!(
            result,
            QosCheckResult::Blocked(PendingReason::QosGrpMemLimit)
        );
    }

    #[test]
    fn test_blocked_by_grp_mem_with_mem_per_cpu() {
        let mut grp = TresRecord::new();
        grp.set(TresType::Memory, 4096); // QOS-wide cap 4 GiB
        let qos = Qos {
            name: "grp".into(),
            limits: QosLimits {
                grp_tres: Some(grp),
                ..Default::default()
            },
            ..Default::default()
        };
        // 4 tasks * 1 cpu/task * 750 MB/cpu == 3 GiB, same as the
        // memory_per_node_mb equivalent above.
        let mut job = make_test_job();
        job.spec.num_nodes = 1;
        job.spec.memory_per_cpu_mb = Some(750);
        let mut qos_running = TresRecord::new();
        qos_running.set(TresType::Memory, 2000); // 2 GiB already running; 2000 + 3000 > 4096
        let result = check_qos_limits(&job, &qos, 0, 0, &TresRecord::new(), &qos_running, None);
        assert_eq!(
            result,
            QosCheckResult::Blocked(PendingReason::QosGrpMemLimit)
        );
    }

    fn grp_wall_qos(cap: u32) -> Qos {
        Qos {
            name: "budget".into(),
            limits: QosLimits {
                grp_wall_minutes: Some(cap),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn grp_wall_blocks_once_the_budget_is_reached() {
        let qos = grp_wall_qos(600);
        let job = make_test_job();
        let result = check_qos_limits(
            &job,
            &qos,
            0,
            0,
            &TresRecord::new(),
            &TresRecord::new(),
            Some(600),
        );
        assert_eq!(
            result,
            QosCheckResult::Blocked(PendingReason::QosGrpWallLimit)
        );
    }

    #[test]
    fn grp_wall_blocks_when_the_budget_is_overspent() {
        let qos = grp_wall_qos(600);
        let job = make_test_job();
        let result = check_qos_limits(
            &job,
            &qos,
            0,
            0,
            &TresRecord::new(),
            &TresRecord::new(),
            Some(4_000),
        );
        assert_eq!(
            result,
            QosCheckResult::Blocked(PendingReason::QosGrpWallLimit)
        );
    }

    #[test]
    fn grp_wall_admits_a_job_that_still_fits_the_budget() {
        // The candidate job's own time limit is deliberately not projected: 599
        // consumed against a 600 cap admits, even though this 2h job will overshoot.
        let qos = grp_wall_qos(600);
        let job = make_test_job();
        let result = check_qos_limits(
            &job,
            &qos,
            0,
            0,
            &TresRecord::new(),
            &TresRecord::new(),
            Some(599),
        );
        assert_eq!(result, QosCheckResult::Allowed);
    }

    #[test]
    fn grp_wall_is_not_applied_when_consumption_is_unknown() {
        // `None` is what an unreachable or disabled accounting database yields.
        // Scheduling must continue rather than halt cluster-wide.
        let qos = grp_wall_qos(1);
        let job = make_test_job();
        let result = check_qos_limits(
            &job,
            &qos,
            0,
            0,
            &TresRecord::new(),
            &TresRecord::new(),
            None,
        );
        assert_eq!(result, QosCheckResult::Allowed);
    }

    #[test]
    fn consumption_alone_blocks_nothing_without_a_budget() {
        let qos = make_qos(None, None);
        let job = make_test_job();
        let result = check_qos_limits(
            &job,
            &qos,
            0,
            0,
            &TresRecord::new(),
            &TresRecord::new(),
            Some(u64::MAX),
        );
        assert_eq!(result, QosCheckResult::Allowed);
    }

    #[test]
    fn test_qos_preempt_override_off_is_none() {
        let qos = Qos {
            preempt_mode: QosPreemptMode::Off,
            ..Default::default()
        };
        assert_eq!(qos_preempt_override(&qos), None);
    }

    #[test]
    fn test_qos_preempt_override_maps_variants() {
        let requeue = Qos {
            preempt_mode: QosPreemptMode::Requeue,
            ..Default::default()
        };
        assert_eq!(qos_preempt_override(&requeue), Some(PreemptMode::Requeue));

        let cancel = Qos {
            preempt_mode: QosPreemptMode::Cancel,
            ..Default::default()
        };
        assert_eq!(qos_preempt_override(&cancel), Some(PreemptMode::Cancel));

        let suspend = Qos {
            preempt_mode: QosPreemptMode::Suspend,
            ..Default::default()
        };
        assert_eq!(qos_preempt_override(&suspend), Some(PreemptMode::Suspend));
    }
}
