// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Start and end times as the CLI reports them: a job still waiting stands in
//! the scheduler's projected slot where Slurm would.

use prost_types::Timestamp;
use spur_proto::proto::{JobInfo, JobState};

/// The projection only stands in while the job is still waiting for that slot,
/// so a job that never ran reports nothing rather than a stale plan.
pub fn effective_start(job: &JobInfo) -> Option<&Timestamp> {
    match (job.start_time.as_ref(), job.planned_start_time.as_ref()) {
        (None, Some(planned)) if is_pending(job) => Some(planned),
        (actual, _) => actual,
    }
}

/// Slurm derives an unfinished job's end from its start plus its time limit.
/// An unlimited job, or one with no projected start, has no end to report.
pub fn effective_end(job: &JobInfo) -> Option<Timestamp> {
    if let Some(end) = job.end_time.as_ref() {
        return Some(*end);
    }
    let start = effective_start(job)?;
    let limit = job.time_limit.as_ref()?;
    Some(Timestamp {
        seconds: start.seconds.checked_add(limit.seconds)?,
        nanos: start.nanos,
    })
}

fn is_pending(job: &JobInfo) -> bool {
    job.state == JobState::JobPending as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(seconds: i64) -> Timestamp {
        Timestamp { seconds, nanos: 0 }
    }

    fn pending_job() -> JobInfo {
        JobInfo {
            state: JobState::JobPending as i32,
            ..Default::default()
        }
    }

    fn running_job() -> JobInfo {
        JobInfo {
            state: JobState::JobRunning as i32,
            ..Default::default()
        }
    }

    #[test]
    fn pending_job_reports_the_projected_start() {
        let job = JobInfo {
            planned_start_time: Some(ts(1_000)),
            ..pending_job()
        };
        assert_eq!(effective_start(&job), Some(&ts(1_000)));
    }

    #[test]
    fn pending_job_without_a_projection_reports_none() {
        assert_eq!(effective_start(&pending_job()), None);
    }

    #[test]
    fn a_real_start_wins_over_a_stale_projection() {
        let job = JobInfo {
            start_time: Some(ts(2_000)),
            planned_start_time: Some(ts(1_000)),
            ..running_job()
        };
        assert_eq!(effective_start(&job), Some(&ts(2_000)));
    }

    #[test]
    fn a_projection_on_a_non_pending_job_is_ignored() {
        let job = JobInfo {
            planned_start_time: Some(ts(1_000)),
            ..running_job()
        };
        assert_eq!(effective_start(&job), None);
    }

    #[test]
    fn pending_end_is_the_projected_start_plus_the_limit() {
        let job = JobInfo {
            planned_start_time: Some(ts(1_000)),
            time_limit: Some(prost_types::Duration {
                seconds: 600,
                nanos: 0,
            }),
            ..pending_job()
        };
        assert_eq!(effective_end(&job), Some(ts(1_600)));
    }

    #[test]
    fn running_end_is_the_real_start_plus_the_limit() {
        let job = JobInfo {
            start_time: Some(ts(2_000)),
            time_limit: Some(prost_types::Duration {
                seconds: 600,
                nanos: 0,
            }),
            ..running_job()
        };
        assert_eq!(effective_end(&job), Some(ts(2_600)));
    }

    #[test]
    fn an_unlimited_job_has_no_projected_end() {
        let job = JobInfo {
            planned_start_time: Some(ts(1_000)),
            ..pending_job()
        };
        assert_eq!(effective_end(&job), None);
    }

    #[test]
    fn a_recorded_end_wins_over_the_projection() {
        let job = JobInfo {
            start_time: Some(ts(2_000)),
            end_time: Some(ts(2_100)),
            time_limit: Some(prost_types::Duration {
                seconds: 600,
                nanos: 0,
            }),
            ..running_job()
        };
        assert_eq!(effective_end(&job), Some(ts(2_100)));
    }

    #[test]
    fn an_overflowing_limit_reports_no_end_rather_than_panicking() {
        let job = JobInfo {
            planned_start_time: Some(ts(i64::MAX - 1)),
            time_limit: Some(prost_types::Duration {
                seconds: 600,
                nanos: 0,
            }),
            ..pending_job()
        };
        assert_eq!(effective_end(&job), None);
    }
}
