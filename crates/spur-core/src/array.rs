// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Job array specification parsing and task expansion.
//!
//! Supports Slurm-compatible array specs:
//! - `0-99` — tasks 0 through 99
//! - `0-99%10` — tasks 0-99 with max 10 running at once
//! - `1,3,5,7` — specific task IDs
//! - `0-10:2` — tasks 0,2,4,6,8,10 (step of 2)
//! - `1-5,10-15` — combination

use crate::job::JobState;
use thiserror::Error;

/// Aggregate the terminal state of an array job from its task states, following
/// Slurm semantics for `scontrol show job <array_job_id>` and dependency
/// resolution against an array parent.
///
/// Returns `None` while any task is still non-terminal (the array as a whole has
/// not finished). Once every task is terminal, returns the aggregate:
/// `Completed` iff all tasks `Completed`, otherwise the "worst" terminal state
/// observed, with failure outranking cancellation/timeout/node-failure.
///
/// `task_states` must contain one entry per array task. An empty slice returns
/// `None` (no tasks known yet — treat as not-finished).
pub fn aggregate_array_state(task_states: &[JobState]) -> Option<JobState> {
    if task_states.is_empty() {
        return None;
    }
    // Still running/pending/etc. — array not finished.
    if task_states.iter().any(|s| !s.is_terminal()) {
        return None;
    }
    if task_states.iter().all(|s| *s == JobState::Completed) {
        return Some(JobState::Completed);
    }
    // Worst-state precedence (Slurm orders failure above the others for the
    // purpose of array exit status). Pick the highest-precedence terminal state.
    let rank = |s: &JobState| match s {
        JobState::Failed => 4,
        JobState::NodeFail => 3,
        JobState::Timeout => 2,
        JobState::Cancelled => 1,
        _ => 0, // Completed (or, defensively, anything else)
    };
    task_states
        .iter()
        .filter(|s| **s != JobState::Completed)
        .max_by_key(|s| rank(s))
        .copied()
}

#[derive(Debug, Clone)]
pub struct ArraySpec {
    /// Individual task IDs.
    pub task_ids: Vec<u32>,
    /// Max concurrent tasks (0 = unlimited).
    pub max_concurrent: u32,
}

#[derive(Debug, Error)]
pub enum ArrayError {
    #[error("invalid array spec: {0}")]
    InvalidSpec(String),
    #[error("array too large: {count} tasks (max {max})")]
    TooLarge { count: usize, max: usize },
}

const MAX_ARRAY_SIZE: usize = 100_000;

/// Parse an array spec string like "0-99%10".
pub fn parse_array_spec(spec: &str) -> Result<ArraySpec, ArrayError> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(ArrayError::InvalidSpec("empty spec".into()));
    }

    // Split off %N concurrent limit
    let (range_part, max_concurrent) = if let Some((ranges, limit)) = spec.rsplit_once('%') {
        let limit: u32 = limit
            .parse()
            .map_err(|_| ArrayError::InvalidSpec(format!("invalid limit: {}", limit)))?;
        (ranges, limit)
    } else {
        (spec, 0)
    };

    let mut task_ids = Vec::new();

    for part in range_part.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }

        // Check for step: "0-10:2"
        let (range_str, step) = if let Some((r, s)) = part.split_once(':') {
            let step: u32 = s
                .parse()
                .map_err(|_| ArrayError::InvalidSpec(format!("invalid step: {}", s)))?;
            if step == 0 {
                return Err(ArrayError::InvalidSpec("step cannot be 0".into()));
            }
            (r, step)
        } else {
            (part, 1)
        };

        if let Some((start_s, end_s)) = range_str.split_once('-') {
            let start: u32 = start_s
                .parse()
                .map_err(|_| ArrayError::InvalidSpec(format!("invalid start: {}", start_s)))?;
            let end: u32 = end_s
                .parse()
                .map_err(|_| ArrayError::InvalidSpec(format!("invalid end: {}", end_s)))?;
            if start > end {
                return Err(ArrayError::InvalidSpec(format!("{} > {}", start, end)));
            }
            let mut i = start;
            while i <= end {
                task_ids.push(i);
                i += step;
            }
        } else {
            let id: u32 = range_str
                .parse()
                .map_err(|_| ArrayError::InvalidSpec(format!("invalid id: {}", range_str)))?;
            task_ids.push(id);
        }
    }

    if task_ids.len() > MAX_ARRAY_SIZE {
        return Err(ArrayError::TooLarge {
            count: task_ids.len(),
            max: MAX_ARRAY_SIZE,
        });
    }

    task_ids.sort();
    task_ids.dedup();

    Ok(ArraySpec {
        task_ids,
        max_concurrent,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_range() {
        let spec = parse_array_spec("0-9").unwrap();
        assert_eq!(spec.task_ids, (0..=9).collect::<Vec<_>>());
        assert_eq!(spec.max_concurrent, 0);
    }

    #[test]
    fn test_range_with_limit() {
        let spec = parse_array_spec("0-99%10").unwrap();
        assert_eq!(spec.task_ids.len(), 100);
        assert_eq!(spec.max_concurrent, 10);
    }

    #[test]
    fn test_specific_ids() {
        let spec = parse_array_spec("1,3,5,7").unwrap();
        assert_eq!(spec.task_ids, vec![1, 3, 5, 7]);
    }

    #[test]
    fn test_step() {
        let spec = parse_array_spec("0-10:2").unwrap();
        assert_eq!(spec.task_ids, vec![0, 2, 4, 6, 8, 10]);
    }

    #[test]
    fn test_mixed() {
        let spec = parse_array_spec("1-5,10-15").unwrap();
        assert_eq!(spec.task_ids, vec![1, 2, 3, 4, 5, 10, 11, 12, 13, 14, 15]);
    }

    #[test]
    fn test_single() {
        let spec = parse_array_spec("42").unwrap();
        assert_eq!(spec.task_ids, vec![42]);
    }

    #[test]
    fn test_empty_fails() {
        assert!(parse_array_spec("").is_err());
    }

    #[test]
    fn test_reversed_range_fails() {
        assert!(parse_array_spec("10-5").is_err());
    }

    #[test]
    fn test_zero_step_fails() {
        assert!(parse_array_spec("0-10:0").is_err());
    }

    // ── aggregate_array_state ────────────────────────────────────

    #[test]
    fn test_aggregate_empty_is_none() {
        assert_eq!(aggregate_array_state(&[]), None);
    }

    #[test]
    fn test_aggregate_unfinished_is_none() {
        // Any non-terminal task means the array is not finished.
        assert_eq!(
            aggregate_array_state(&[JobState::Completed, JobState::Running]),
            None
        );
        assert_eq!(
            aggregate_array_state(&[JobState::Pending, JobState::Completed]),
            None
        );
    }

    #[test]
    fn test_aggregate_all_completed() {
        assert_eq!(
            aggregate_array_state(&[
                JobState::Completed,
                JobState::Completed,
                JobState::Completed
            ]),
            Some(JobState::Completed)
        );
    }

    #[test]
    fn test_aggregate_mixed_failure_wins() {
        // Failure outranks completion.
        assert_eq!(
            aggregate_array_state(&[JobState::Completed, JobState::Failed]),
            Some(JobState::Failed)
        );
    }

    #[test]
    fn test_aggregate_precedence_failed_over_others() {
        // Failed > NodeFail > Timeout > Cancelled.
        assert_eq!(
            aggregate_array_state(&[
                JobState::Cancelled,
                JobState::Timeout,
                JobState::NodeFail,
                JobState::Failed,
            ]),
            Some(JobState::Failed)
        );
        assert_eq!(
            aggregate_array_state(&[JobState::Cancelled, JobState::Timeout, JobState::NodeFail]),
            Some(JobState::NodeFail)
        );
        assert_eq!(
            aggregate_array_state(&[JobState::Cancelled, JobState::Timeout]),
            Some(JobState::Timeout)
        );
        assert_eq!(
            aggregate_array_state(&[JobState::Cancelled, JobState::Cancelled]),
            Some(JobState::Cancelled)
        );
    }
}
