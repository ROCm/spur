// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use spur_core::job::{Job, JobState, PendingReason};

/// Number of [`JobState`] variants (index for `by_state`).
pub const JOB_STATE_COUNT: usize = JobState::COUNT;

/// Aggregated job metrics derived from the controller job map.
///
/// Built by scanning in-memory `Job` records (lazy, on read). Durable state lives
/// in the Raft-backed job map on `ClusterManager`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobMetricsSnapshot {
    /// Total jobs in the controller map (includes terminal jobs).
    pub total: u64,
    /// Count per [`JobState`]; index via [`job_state_index`].
    pub by_state: [u64; JOB_STATE_COUNT],
    /// Pending jobs with `pending_reason == Held`.
    pub held_pending: u64,
    /// Sum of allocated CPUs for jobs in Running or Completing.
    pub running_cpus: u64,
    /// Sum of allocated memory (bytes) for Running or Completing.
    pub running_memory_bytes: u64,
    /// Sum of allocated GPU count for Running or Completing.
    pub running_gpus: u64,
}

/// Map [`JobState`] to a stable index in [`JobMetricsSnapshot::by_state`]
/// (proto wire discriminant via [`JobState::from_proto_i32`]).
pub fn job_state_index(state: JobState) -> usize {
    let wire = state.to_proto_i32();
    debug_assert_eq!(JobState::from_proto_i32(wire), Some(state));
    wire as usize
}

fn counts_toward_running_alloc(state: JobState) -> bool {
    matches!(state, JobState::Running | JobState::Completing)
}

impl JobMetricsSnapshot {
    /// Rebuild metrics by scanning all jobs.
    pub fn collect<'a>(jobs: impl IntoIterator<Item = &'a Job>) -> Self {
        let mut snap = Self::default();
        for job in jobs {
            snap.total += 1;
            snap.by_state[job_state_index(job.state)] += 1;

            if job.state == JobState::Pending && job.pending_reason == PendingReason::Held {
                snap.held_pending += 1;
            }

            if counts_toward_running_alloc(job.state) {
                if let Some(ref alloc) = job.allocated_resources {
                    snap.running_cpus += u64::from(alloc.cpus);
                    snap.running_memory_bytes += alloc.memory_mb.saturating_mul(1024 * 1024);
                    snap.running_gpus += u64::from(alloc.total_gpus());
                }
            }
        }
        snap
    }

    /// Count for a single state.
    pub fn count_state(&self, state: JobState) -> u64 {
        self.by_state[job_state_index(state)]
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use spur_core::job::{Job, JobSpec, JobState, PendingReason};
    use spur_core::resource::{GpuLinkType, GpuResource, ResourceSet};

    use super::*;

    fn job_with_state(id: u32, state: JobState, held: bool) -> Job {
        let mut job = Job::new(id, JobSpec::default());
        job.state = state;
        if held {
            job.pending_reason = PendingReason::Held;
        }
        job
    }

    fn running_with_alloc(id: u32, cpus: u32, memory_mb: u64, gpu_count: u32) -> Job {
        let mut job = Job::new(id, JobSpec::default());
        job.state = JobState::Running;
        job.start_time = Some(Utc::now());
        let mut gpus = Vec::new();
        for i in 0..gpu_count {
            gpus.push(GpuResource {
                device_id: i,
                gpu_type: "mi300x".into(),
                memory_mb: 0,
                peer_gpus: vec![],
                link_type: GpuLinkType::XGMI,
            });
        }
        job.allocated_resources = Some(ResourceSet {
            cpus,
            memory_mb,
            gpus,
            generic: Default::default(),
        });
        job
    }

    #[test]
    fn empty_jobs() {
        let snap = JobMetricsSnapshot::collect([]);
        assert_eq!(snap, JobMetricsSnapshot::default());
    }

    #[test]
    fn counts_by_state_and_held() {
        let jobs = [
            job_with_state(1, JobState::Pending, true),
            job_with_state(2, JobState::Pending, false),
            job_with_state(3, JobState::Running, false),
            job_with_state(4, JobState::Completed, false),
        ];
        let snap = JobMetricsSnapshot::collect(jobs.iter());
        assert_eq!(snap.total, 4);
        assert_eq!(snap.count_state(JobState::Pending), 2);
        assert_eq!(snap.held_pending, 1);
        assert_eq!(snap.count_state(JobState::Running), 1);
        assert_eq!(snap.count_state(JobState::Completed), 1);
    }

    #[test]
    fn running_alloc_sums_running_and_completing() {
        let jobs = [
            running_with_alloc(1, 4, 8192, 2),
            {
                let mut j = running_with_alloc(2, 2, 4096, 1);
                j.state = JobState::Completing;
                j
            },
            job_with_state(3, JobState::Pending, false),
            job_with_state(4, JobState::Pending, false),
        ];
        let snap = JobMetricsSnapshot::collect(jobs.iter());
        assert_eq!(snap.running_cpus, 4 + 2);
        assert_eq!(snap.running_memory_bytes, (8192 + 4096) * 1024 * 1024);
        assert_eq!(snap.running_gpus, 2 + 1);
        assert_eq!(snap.count_state(JobState::Pending), 2);
    }

    #[test]
    fn job_state_index_uses_proto_wire() {
        for &state in &JobState::ALL {
            let wire = state.to_proto_i32();
            assert_eq!(JobState::from_proto_i32(wire), Some(state));
            assert_eq!(job_state_index(state), wire as usize);
        }
    }

    #[test]
    fn pending_job_with_alloc_not_counted() {
        let mut job = running_with_alloc(1, 4, 1024, 1);
        job.state = JobState::Pending;
        let snap = JobMetricsSnapshot::collect([&job]);
        assert_eq!(snap.running_cpus, 0);
        assert_eq!(snap.running_gpus, 0);
    }
}
