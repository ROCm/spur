// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! QOS enforcement logic.
//!
//! Checks per-QOS limits before allowing a job to be scheduled.

use crate::accounting::{Qos, QosPreemptMode, TresRecord, TresType};
use crate::job::{Job, PendingReason};
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

/// A QOS-level preempt mode override for a running job, or `None` if the
/// QOS carries no override.
///
/// `QosPreemptMode::Off` is both the wire format's "unset" default and a
/// legitimate explicit choice — there is no way to tell them apart — so
/// `Off` is treated as "no override" here, deferring to whatever the
/// caller would otherwise resolve (e.g. partition-level `PreemptMode`).
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

/// Check if a job would violate QOS limits.
///
/// `user_running_*` aggregate the requesting user's load; `qos_running_tres`
/// aggregates all running jobs in the QOS (for the `Grp*` group limits).
pub fn check_qos_limits(
    job: &Job,
    qos: &Qos,
    user_running_count: u32,
    user_submitted_count: u32,
    user_running_tres: &TresRecord,
    qos_running_tres: &TresRecord,
) -> QosCheckResult {
    let limits = &qos.limits;

    // Max jobs per user
    if let Some(max) = limits.max_jobs_per_user {
        if user_running_count >= max {
            return QosCheckResult::Blocked(PendingReason::QoSMaxJobsPerUser);
        }
    }

    // Max submit jobs per user
    if let Some(max) = limits.max_submit_jobs_per_user {
        if user_submitted_count >= max {
            // Slurm distinguishes the submit-job cap (WAIT_QOS_MAX_SUB_JOB,
            // "QOSMaxSubmitJobPerUserLimit") from the running-job cap above.
            return QosCheckResult::Blocked(PendingReason::QosMaxSubmitJobPerUserLimit);
        }
    }

    // Max wall time
    if let Some(max_wall) = limits.max_wall_minutes {
        if let Some(job_wall) = job.spec.time_limit {
            if job_wall.num_minutes() > max_wall as i64 {
                // This is a QOS wall cap, not a partition cap: Slurm reports
                // WAIT_QOS_MAX_WALL_PER_JOB ("QOSMaxWallDurationPerJobLimit").
                return QosCheckResult::Blocked(PendingReason::QosMaxWallDurationPerJobLimit);
            }
        }
    }

    // Max TRES per job
    if let Some(ref max_tres) = limits.max_tres_per_job {
        let job_cpus = (job.spec.num_tasks * job.spec.cpus_per_task) as u64;
        if max_tres.get(TresType::Cpu) > 0 && job_cpus > max_tres.get(TresType::Cpu) {
            return QosCheckResult::Blocked(PendingReason::QosMaxCpuPerJobLimit);
        }

        let job_nodes = job.spec.num_nodes as u64;
        if max_tres.get(TresType::Node) > 0 && job_nodes > max_tres.get(TresType::Node) {
            return QosCheckResult::Blocked(PendingReason::QosMaxNodePerJobLimit);
        }

        if let Some(mem) = job.spec.memory_per_node_mb {
            let total_mem = mem * job.spec.num_nodes as u64;
            if max_tres.get(TresType::Memory) > 0 && total_mem > max_tres.get(TresType::Memory) {
                return QosCheckResult::Blocked(PendingReason::QosMaxMemoryPerJob);
            }
        }
    }

    // Max TRES per user (group limit)
    if let Some(ref max_tres) = limits.max_tres_per_user {
        let job_cpus = (job.spec.num_tasks * job.spec.cpus_per_task) as u64;
        let new_total_cpu = user_running_tres.get(TresType::Cpu) + job_cpus;
        if max_tres.get(TresType::Cpu) > 0 && new_total_cpu > max_tres.get(TresType::Cpu) {
            return QosCheckResult::Blocked(PendingReason::QosMaxCpuPerUserLimit);
        }
    }

    if let Some(ref grp) = limits.grp_tres {
        let job_cpus = (job.spec.num_tasks * job.spec.cpus_per_task) as u64;
        if grp.get(TresType::Cpu) > 0
            && qos_running_tres.get(TresType::Cpu) + job_cpus > grp.get(TresType::Cpu)
        {
            return QosCheckResult::Blocked(PendingReason::QosGrpCpuLimit);
        }

        let job_nodes = job.spec.num_nodes as u64;
        if grp.get(TresType::Node) > 0
            && qos_running_tres.get(TresType::Node) + job_nodes > grp.get(TresType::Node)
        {
            return QosCheckResult::Blocked(PendingReason::QosGrpNodeLimit);
        }

        if let Some(mem) = job.spec.memory_per_node_mb {
            let job_mem = mem * job.spec.num_nodes as u64;
            if grp.get(TresType::Memory) > 0
                && qos_running_tres.get(TresType::Memory) + job_mem > grp.get(TresType::Memory)
            {
                return QosCheckResult::Blocked(PendingReason::QosGrpMemLimit);
            }
        }
    }

    QosCheckResult::Allowed
}

/// Add a QOS's priority delta on top of an already-computed priority.
///
/// Callers should pass the fully weighted priority (fairshare/age/partition
/// tier already applied) as `base_priority`, not a job's raw submitted
/// priority: the QOS delta is a flat addition, so applying it before those
/// multiplicative factors would let them amplify or dilute it in proportion
/// to values that have nothing to do with QOS.
pub fn qos_adjusted_priority(base_priority: u32, qos: &Qos) -> u32 {
    let adjusted = base_priority as i64 + qos.priority as i64;
    adjusted.clamp(1, u32::MAX as i64) as u32
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
        let result = check_qos_limits(&job, &qos, 0, 0, &TresRecord::new(), &TresRecord::new());
        assert_eq!(result, QosCheckResult::Allowed);
    }

    #[test]
    fn test_blocked_by_max_jobs() {
        let qos = make_qos(Some(5), None);
        let job = make_test_job();
        let result = check_qos_limits(&job, &qos, 5, 5, &TresRecord::new(), &TresRecord::new());
        assert_eq!(
            result,
            QosCheckResult::Blocked(PendingReason::QoSMaxJobsPerUser)
        );
    }

    #[test]
    fn test_allowed_under_max_jobs() {
        let qos = make_qos(Some(5), None);
        let job = make_test_job();
        let result = check_qos_limits(&job, &qos, 3, 3, &TresRecord::new(), &TresRecord::new());
        assert_eq!(result, QosCheckResult::Allowed);
    }

    #[test]
    fn test_blocked_by_max_wall() {
        let qos = make_qos(None, Some(60)); // 1 hour max
        let job = make_test_job(); // 2 hour job
        let result = check_qos_limits(&job, &qos, 0, 0, &TresRecord::new(), &TresRecord::new());
        assert_eq!(
            result,
            QosCheckResult::Blocked(PendingReason::QosMaxWallDurationPerJobLimit)
        );
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
        let result = check_qos_limits(&job, &qos, 0, 0, &TresRecord::new(), &TresRecord::new());
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
        let result = check_qos_limits(&job, &qos, 0, 0, &TresRecord::new(), &TresRecord::new());
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
        let result = check_qos_limits(&job, &qos, 0, 0, &running, &TresRecord::new());
        assert_eq!(
            result,
            QosCheckResult::Blocked(PendingReason::QosMaxCpuPerUserLimit)
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
        let result = check_qos_limits(&job, &qos, 0, 3, &TresRecord::new(), &TresRecord::new());
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
        let result = check_qos_limits(&job, &qos, 0, 0, &TresRecord::new(), &TresRecord::new());
        assert_eq!(
            result,
            QosCheckResult::Blocked(PendingReason::QosMaxNodePerJobLimit)
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
        let result = check_qos_limits(&job, &qos, 0, 0, &TresRecord::new(), &qos_running);
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
        let result = check_qos_limits(&job, &qos, 0, 0, &TresRecord::new(), &qos_running);
        assert_eq!(
            result,
            QosCheckResult::Blocked(PendingReason::QosGrpNodeLimit)
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
        let result = check_qos_limits(&job, &qos, 0, 0, &TresRecord::new(), &qos_running);
        assert_eq!(
            result,
            QosCheckResult::Blocked(PendingReason::QosGrpMemLimit)
        );
    }

    #[test]
    fn test_qos_priority_adjustment() {
        let qos = Qos {
            priority: 500,
            ..Default::default()
        };
        assert_eq!(qos_adjusted_priority(1000, &qos), 1500);

        let qos_neg = Qos {
            priority: -200,
            ..Default::default()
        };
        assert_eq!(qos_adjusted_priority(1000, &qos_neg), 800);
    }

    #[test]
    fn test_qos_priority_floor() {
        let qos = Qos {
            priority: -2000,
            ..Default::default()
        };
        assert_eq!(qos_adjusted_priority(1000, &qos), 1); // Floor at 1
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
