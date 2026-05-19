// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Job dependency parsing and checking.
//!
//! Supports: afterok:N, afterany:N, afternotok:N, aftercorr:N, singleton
//! Multiple dependencies separated by commas or colons within a type.

use crate::job::{Job, JobId, JobState};

/// A parsed dependency condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dependency {
    /// Job must complete successfully (exit 0).
    AfterOk(JobId),
    /// Job must complete (any exit code).
    AfterAny(JobId),
    /// Job must fail (non-zero exit).
    AfterNotOk(JobId),
    /// Corresponding array task must complete successfully.
    AfterCorr(JobId),
    /// No other job with same name+user can be running or pending.
    Singleton,
}

/// Parse dependency strings like "afterok:100", "afterany:200,300", "singleton".
pub fn parse_dependencies(specs: &[String]) -> Vec<Dependency> {
    let mut deps = Vec::new();
    for spec in specs {
        for part in spec.split(',') {
            let part = part.trim();
            if part.eq_ignore_ascii_case("singleton") {
                deps.push(Dependency::Singleton);
                continue;
            }
            if let Some((dtype, ids)) = part.split_once(':') {
                for id_str in ids.split(':') {
                    if let Ok(id) = id_str.trim().parse::<JobId>() {
                        match dtype.to_lowercase().as_str() {
                            "afterok" | "after_ok" => deps.push(Dependency::AfterOk(id)),
                            "afterany" | "after_any" | "after" => {
                                deps.push(Dependency::AfterAny(id))
                            }
                            "afternotok" | "after_not_ok" => deps.push(Dependency::AfterNotOk(id)),
                            "aftercorr" | "after_corr" => deps.push(Dependency::AfterCorr(id)),
                            _ => {} // Unknown dependency type, skip
                        }
                    }
                }
            }
        }
    }
    deps
}

/// Check if all dependencies for a job are satisfied.
///
/// Returns `true` if the job is ready to schedule, `false` if blocked.
/// Returns `None` if the dependency can never be satisfied (should cancel the job).
pub fn check_dependencies(
    job: &Job,
    get_job: &dyn Fn(JobId) -> Option<Job>,
    get_jobs_by_name_user: &dyn Fn(&str, &str) -> Vec<Job>,
) -> DependencyResult {
    let deps = parse_dependencies(&job.spec.dependency);
    if deps.is_empty() {
        return DependencyResult::Satisfied;
    }

    for dep in &deps {
        match dep {
            Dependency::AfterOk(dep_id) => {
                match get_job(*dep_id) {
                    Some(dep_job) => match dep_job.state {
                        JobState::Completed => {} // OK, satisfied
                        JobState::Failed
                        | JobState::Cancelled
                        | JobState::Timeout
                        | JobState::NodeFail => {
                            return DependencyResult::Failed; // Dependency failed
                        }
                        _ => return DependencyResult::Waiting, // Still running/pending
                    },
                    None => return DependencyResult::Failed, // Job doesn't exist
                }
            }
            Dependency::AfterAny(dep_id) => {
                match get_job(*dep_id) {
                    Some(dep_job) if dep_job.state.is_terminal() => {} // Any terminal state
                    Some(_) => return DependencyResult::Waiting,
                    None => {} // Job doesn't exist, treat as satisfied
                }
            }
            Dependency::AfterNotOk(dep_id) => {
                match get_job(*dep_id) {
                    Some(dep_job) => match dep_job.state {
                        JobState::Failed
                        | JobState::Cancelled
                        | JobState::Timeout
                        | JobState::NodeFail => {} // Satisfied
                        JobState::Completed => return DependencyResult::Failed,
                        _ => return DependencyResult::Waiting,
                    },
                    None => return DependencyResult::Failed,
                }
            }
            Dependency::AfterCorr(dep_id) => {
                // For array jobs: check the corresponding task
                // For non-array: same as afterok
                match get_job(*dep_id) {
                    Some(dep_job) => match dep_job.state {
                        JobState::Completed => {}
                        s if s.is_terminal() => return DependencyResult::Failed,
                        _ => return DependencyResult::Waiting,
                    },
                    None => return DependencyResult::Failed,
                }
            }
            Dependency::Singleton => {
                let matching = get_jobs_by_name_user(&job.spec.name, &job.spec.user);
                let has_active = matching.iter().any(|j| {
                    j.job_id != job.job_id
                        && (j.state == JobState::Running || j.state == JobState::Pending)
                });
                if has_active {
                    return DependencyResult::Waiting;
                }
            }
        }
    }

    DependencyResult::Satisfied
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencyResult {
    /// All dependencies met, job can be scheduled.
    Satisfied,
    /// Some dependencies not yet resolved, keep waiting.
    Waiting,
    /// A dependency can never be satisfied (e.g., afterok on a failed job).
    Failed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::JobSpec;

    fn make_job(id: JobId, state: JobState) -> Job {
        let mut job = Job::new(
            id,
            JobSpec {
                name: "test".into(),
                user: "alice".into(),
                ..Default::default()
            },
        );
        if state != JobState::Pending {
            let _ = job.transition(JobState::Running);
            if state != JobState::Running {
                let _ = job.transition(state);
            }
        }
        job
    }

    #[test]
    fn test_parse_afterok() {
        let deps = parse_dependencies(&["afterok:100".into()]);
        assert_eq!(deps, vec![Dependency::AfterOk(100)]);
    }

    #[test]
    fn test_parse_multiple() {
        let deps = parse_dependencies(&["afterok:100,afterany:200".into()]);
        assert_eq!(
            deps,
            vec![Dependency::AfterOk(100), Dependency::AfterAny(200)]
        );
    }

    #[test]
    fn test_parse_singleton() {
        let deps = parse_dependencies(&["singleton".into()]);
        assert_eq!(deps, vec![Dependency::Singleton]);
    }

    #[test]
    fn test_afterok_satisfied() {
        let dep_job = make_job(100, JobState::Completed);
        let job = Job::new(
            1,
            JobSpec {
                name: "test".into(),
                user: "alice".into(),
                dependency: vec!["afterok:100".into()],
                ..Default::default()
            },
        );

        let result = check_dependencies(
            &job,
            &|id| {
                if id == 100 {
                    Some(dep_job.clone())
                } else {
                    None
                }
            },
            &|_, _| Vec::new(),
        );

        assert_eq!(result, DependencyResult::Satisfied);
    }

    #[test]
    fn test_afterok_waiting() {
        let dep_job = make_job(100, JobState::Running);
        let job = Job::new(
            1,
            JobSpec {
                name: "test".into(),
                user: "alice".into(),
                dependency: vec!["afterok:100".into()],
                ..Default::default()
            },
        );

        let result = check_dependencies(
            &job,
            &|id| {
                if id == 100 {
                    Some(dep_job.clone())
                } else {
                    None
                }
            },
            &|_, _| Vec::new(),
        );

        assert_eq!(result, DependencyResult::Waiting);
    }

    #[test]
    fn test_afterok_failed() {
        let dep_job = make_job(100, JobState::Failed);
        let job = Job::new(
            1,
            JobSpec {
                name: "test".into(),
                user: "alice".into(),
                dependency: vec!["afterok:100".into()],
                ..Default::default()
            },
        );

        let result = check_dependencies(
            &job,
            &|id| {
                if id == 100 {
                    Some(dep_job.clone())
                } else {
                    None
                }
            },
            &|_, _| Vec::new(),
        );

        assert_eq!(result, DependencyResult::Failed);
    }

    #[test]
    fn test_no_deps_satisfied() {
        let job = Job::new(
            1,
            JobSpec {
                name: "test".into(),
                user: "alice".into(),
                ..Default::default()
            },
        );

        let result = check_dependencies(&job, &|_| None, &|_, _| Vec::new());
        assert_eq!(result, DependencyResult::Satisfied);
    }
}
