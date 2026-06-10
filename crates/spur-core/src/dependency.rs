// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Job dependency parsing and checking.
//!
//! Supports: after:N[+M], afterok:N, afterany:N, afternotok:N, aftercorr:N,
//! singleton. Multiple dependencies separated by commas; multiple targets of
//! the same type separated by colons (e.g. `afterok:100:200`).

use crate::array::aggregate_array_state;
use crate::job::{Job, JobId, JobState};

/// A parsed dependency condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dependency {
    /// Job must begin; child eligible `delay_minutes` after the parent's
    /// start time (regardless of parent outcome). Slurm's `after:N[+M]`.
    After { job_id: JobId, delay_minutes: u32 },
    /// Job must complete successfully (exit 0).
    AfterOk(JobId),
    /// Job must complete (any exit code).
    AfterAny(JobId),
    /// Job must fail (non-zero exit).
    AfterNotOk(JobId),
    /// Corresponding array task must complete successfully (per-task
    /// correspondence: child task[N] releases when parent task[N] succeeds).
    AfterCorr(JobId),
    /// No other job with same name+user can be running or pending.
    Singleton,
}

/// Error returned when a dependency string cannot be parsed or names an
/// unknown dependency type. Surfaced at submit time so users get a clear
/// rejection instead of a silently-deadlocked job.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DependencyParseError {
    #[error("unknown dependency type: '{0}'")]
    UnknownType(String),
    #[error("invalid job id in dependency: '{0}'")]
    InvalidJobId(String),
    #[error("invalid dependency syntax: '{0}'")]
    InvalidSyntax(String),
}

/// Parse dependency strings, rejecting unknown types and malformed ids.
///
/// Accepts forms like `afterok:100`, `afterany:200:300`, `after:100+5`,
/// `aftercorr:42`, `singleton`. Multiple entries may be comma-separated within
/// one string. Returns an error on the first unrecognized type or unparseable
/// id so the caller can reject the submission.
pub fn try_parse_dependencies(specs: &[String]) -> Result<Vec<Dependency>, DependencyParseError> {
    let mut deps = Vec::new();
    for spec in specs {
        for part in spec.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            if part.eq_ignore_ascii_case("singleton") {
                deps.push(Dependency::Singleton);
                continue;
            }
            let (dtype, ids) = part
                .split_once(':')
                .ok_or_else(|| DependencyParseError::InvalidSyntax(part.to_string()))?;
            let dtype = dtype.to_lowercase();

            // Reject unknown types up front, before the id loop — otherwise an
            // id-less unknown type (e.g. `expand:`) would slip through because
            // the loop body never runs.
            let is_after = dtype == "after";
            match dtype.as_str() {
                "after" | "afterok" | "after_ok" | "afterany" | "after_any" | "afternotok"
                | "after_not_ok" | "aftercorr" | "after_corr" => {}
                other => return Err(DependencyParseError::UnknownType(other.to_string())),
            }

            // A known type with no parseable ids (e.g. `afterok:`) is malformed.
            let mut saw_id = false;
            for id_str in ids.split(':') {
                let id_str = id_str.trim();
                if id_str.is_empty() {
                    continue;
                }
                // Only `after` accepts an optional `+M` minute delay suffix;
                // any other type carrying `+M` is invalid syntax.
                let (id_part, delay_minutes) = match id_str.split_once('+') {
                    Some((id, delay)) => {
                        if !is_after {
                            return Err(DependencyParseError::InvalidSyntax(id_str.to_string()));
                        }
                        let delay = delay
                            .trim()
                            .parse::<u32>()
                            .map_err(|_| DependencyParseError::InvalidSyntax(id_str.to_string()))?;
                        (id.trim(), delay)
                    }
                    None => (id_str, 0),
                };
                let id = id_part
                    .parse::<JobId>()
                    .map_err(|_| DependencyParseError::InvalidJobId(id_part.to_string()))?;
                saw_id = true;
                match dtype.as_str() {
                    "after" => deps.push(Dependency::After {
                        job_id: id,
                        delay_minutes,
                    }),
                    "afterok" | "after_ok" => deps.push(Dependency::AfterOk(id)),
                    "afterany" | "after_any" => deps.push(Dependency::AfterAny(id)),
                    "afternotok" | "after_not_ok" => deps.push(Dependency::AfterNotOk(id)),
                    "aftercorr" | "after_corr" => deps.push(Dependency::AfterCorr(id)),
                    _ => unreachable!("dtype validated above"),
                }
            }
            if !saw_id {
                return Err(DependencyParseError::InvalidSyntax(part.to_string()));
            }
        }
    }
    Ok(deps)
}

/// Lenient parse used on the resolution path, where the spec was already
/// validated at submit time. Unknown/malformed entries are dropped rather than
/// erroring so a single bad entry can't wedge the scheduler loop.
pub fn parse_dependencies(specs: &[String]) -> Vec<Dependency> {
    try_parse_dependencies(specs).unwrap_or_else(|_| {
        // Fall back to best-effort: parse each entry independently, skipping
        // any that fail. This preserves behavior for callers that pre-validated.
        let mut deps = Vec::new();
        for spec in specs {
            for part in spec.split(',') {
                if let Ok(mut ok) = try_parse_dependencies(&[part.to_string()]) {
                    deps.append(&mut ok);
                }
            }
        }
        deps
    })
}

/// Resolve a dependency target to its effective state, transparently handling
/// array parents.
///
/// A dependency target id `P` may refer to:
/// - a scalar job (returned directly by `get_job`), or
/// - an array *parent* id whose own job record never exists (Spur stores only
///   the per-task jobs). In that case we aggregate the task states.
///
/// Returns `None` when the target is unknown (neither a scalar job nor an array
/// with any tasks). Otherwise returns `Some(state)`: the live scalar state, or
/// the array's aggregate. An array that exists but has not finished aggregating
/// is reported as the non-terminal sentinel `JobState::Running`, so callers
/// treat it as still-waiting.
fn resolve_target_state(
    dep_id: JobId,
    get_job: &dyn Fn(JobId) -> Option<Job>,
    get_array_tasks: &dyn Fn(JobId) -> Vec<Job>,
) -> Option<JobState> {
    if let Some(job) = get_job(dep_id) {
        return Some(job.state);
    }
    let tasks = get_array_tasks(dep_id);
    if tasks.is_empty() {
        return None; // Genuinely unknown id.
    }
    // Array parent: aggregate. `None` here means "not finished yet" — we encode
    // that as a non-terminal sentinel (Running) so callers treat it as Waiting.
    Some(
        aggregate_array_state(&tasks.iter().map(|t| t.state).collect::<Vec<_>>())
            .unwrap_or(JobState::Running),
    )
}

/// Check if all dependencies for a job are satisfied.
///
/// `get_array_tasks(id)` returns all task jobs whose `array_job_id == id`
/// (empty for non-array ids). This lets the resolver aggregate over array
/// parents and implement `aftercorr` per-task correspondence.
pub fn check_dependencies(
    job: &Job,
    get_job: &dyn Fn(JobId) -> Option<Job>,
    get_array_tasks: &dyn Fn(JobId) -> Vec<Job>,
    get_jobs_by_name_user: &dyn Fn(&str, &str) -> Vec<Job>,
) -> DependencyResult {
    let deps = parse_dependencies(&job.spec.dependency);
    if deps.is_empty() {
        return DependencyResult::Satisfied;
    }

    for dep in &deps {
        match dep {
            Dependency::After {
                job_id: dep_id,
                delay_minutes,
            } => {
                // Find the parent's start time. For an array parent, use the
                // earliest task start. Eligible `delay_minutes` after start,
                // regardless of outcome.
                let start = get_job(*dep_id).and_then(|j| j.start_time).or_else(|| {
                    get_array_tasks(*dep_id)
                        .iter()
                        .filter_map(|t| t.start_time)
                        .min()
                });
                match start {
                    Some(start) => {
                        let eligible = start + chrono::Duration::minutes(*delay_minutes as i64);
                        if chrono::Utc::now() < eligible {
                            return DependencyResult::Waiting;
                        }
                    }
                    None => {
                        // No recorded start time. Either the parent never
                        // existed, or it reached a terminal state without ever
                        // running (e.g. cancelled/failed while Pending —
                        // start_time is only set on JobStart). In both cases the
                        // parent will never start, so `after` can no longer
                        // block: Slurm releases `after` dependents once the
                        // parent leaves the pending/eligible set. Only a
                        // known-but-still-non-terminal parent keeps us waiting.
                        match resolve_target_state(*dep_id, get_job, get_array_tasks) {
                            Some(s) if !s.is_terminal() => return DependencyResult::Waiting,
                            _ => {} // Terminal, or unknown — satisfied.
                        }
                    }
                }
            }
            Dependency::AfterOk(dep_id) => {
                match resolve_target_state(*dep_id, get_job, get_array_tasks) {
                    Some(JobState::Completed) => {} // OK, satisfied
                    Some(JobState::Failed)
                    | Some(JobState::Cancelled)
                    | Some(JobState::Timeout)
                    | Some(JobState::NodeFail)
                    | Some(JobState::Deadline) => {
                        return DependencyResult::Failed; // Dependency failed
                    }
                    Some(_) => return DependencyResult::Waiting, // Still running/pending
                    None => return DependencyResult::Failed,     // Target doesn't exist
                }
            }
            Dependency::AfterAny(dep_id) => {
                match resolve_target_state(*dep_id, get_job, get_array_tasks) {
                    Some(s) if s.is_terminal() => {} // Any terminal state
                    Some(_) => return DependencyResult::Waiting,
                    None => {} // Target doesn't exist, treat as satisfied
                }
            }
            Dependency::AfterNotOk(dep_id) => {
                match resolve_target_state(*dep_id, get_job, get_array_tasks) {
                    Some(JobState::Failed)
                    | Some(JobState::Cancelled)
                    | Some(JobState::Timeout)
                    | Some(JobState::NodeFail)
                    | Some(JobState::Deadline) => {} // Satisfied
                    Some(JobState::Completed) => return DependencyResult::Failed,
                    Some(_) => return DependencyResult::Waiting,
                    None => return DependencyResult::Failed,
                }
            }
            Dependency::AfterCorr(dep_id) => {
                // Per-task correspondence: this child task[N] releases when the
                // parent's task[N] completes successfully. For a non-array child
                // (or scalar parent) this degrades to afterok.
                match job.spec.array_task_id {
                    Some(task_n) => {
                        let tasks = get_array_tasks(*dep_id);
                        if tasks.is_empty() {
                            // Parent may be scalar — fall back to afterok.
                            match resolve_target_state(*dep_id, get_job, get_array_tasks) {
                                Some(JobState::Completed) => {}
                                Some(s) if s.is_terminal() => return DependencyResult::Failed,
                                Some(_) => return DependencyResult::Waiting,
                                None => return DependencyResult::Failed,
                            }
                        } else {
                            match tasks.iter().find(|t| t.spec.array_task_id == Some(task_n)) {
                                Some(t) => match t.state {
                                    JobState::Completed => {}
                                    s if s.is_terminal() => return DependencyResult::Failed,
                                    _ => return DependencyResult::Waiting,
                                },
                                // No corresponding parent task[N] exists — can
                                // never correspond.
                                None => return DependencyResult::Failed,
                            }
                        }
                    }
                    None => {
                        // Scalar child: aftercorr behaves like afterok.
                        match resolve_target_state(*dep_id, get_job, get_array_tasks) {
                            Some(JobState::Completed) => {}
                            Some(s) if s.is_terminal() => return DependencyResult::Failed,
                            Some(_) => return DependencyResult::Waiting,
                            None => return DependencyResult::Failed,
                        }
                    }
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
            &|_| Vec::new(),
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
            &|_| Vec::new(),
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
            &|_| Vec::new(),
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

        let result = check_dependencies(&job, &|_| None, &|_| Vec::new(), &|_, _| Vec::new());
        assert_eq!(result, DependencyResult::Satisfied);
    }

    fn make_array_task(parent: JobId, task: u32, state: JobState) -> Job {
        let mut job = Job::new(
            parent + 1 + task as JobId,
            JobSpec {
                name: "arr".into(),
                user: "alice".into(),
                array_job_id: Some(parent),
                array_task_id: Some(task),
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
    fn test_afterok_array_parent_all_completed() {
        let tasks = vec![
            make_array_task(100, 0, JobState::Completed),
            make_array_task(100, 1, JobState::Completed),
            make_array_task(100, 2, JobState::Completed),
        ];
        let job = Job::new(
            1,
            JobSpec {
                name: "c".into(),
                user: "alice".into(),
                dependency: vec!["afterok:100".into()],
                ..Default::default()
            },
        );
        let result = check_dependencies(
            &job,
            &|_| None,
            &|id| if id == 100 { tasks.clone() } else { Vec::new() },
            &|_, _| Vec::new(),
        );
        assert_eq!(result, DependencyResult::Satisfied);
    }

    #[test]
    fn test_afterok_array_parent_one_running_waits() {
        let tasks = vec![
            make_array_task(100, 0, JobState::Completed),
            make_array_task(100, 1, JobState::Running),
        ];
        let job = Job::new(
            1,
            JobSpec {
                name: "c".into(),
                user: "alice".into(),
                dependency: vec!["afterok:100".into()],
                ..Default::default()
            },
        );
        let result = check_dependencies(
            &job,
            &|_| None,
            &|id| if id == 100 { tasks.clone() } else { Vec::new() },
            &|_, _| Vec::new(),
        );
        assert_eq!(result, DependencyResult::Waiting);
    }

    #[test]
    fn test_afterok_array_parent_one_failed_fails() {
        let tasks = vec![
            make_array_task(100, 0, JobState::Completed),
            make_array_task(100, 1, JobState::Failed),
        ];
        let job = Job::new(
            1,
            JobSpec {
                name: "c".into(),
                user: "alice".into(),
                dependency: vec!["afterok:100".into()],
                ..Default::default()
            },
        );
        let result = check_dependencies(
            &job,
            &|_| None,
            &|id| if id == 100 { tasks.clone() } else { Vec::new() },
            &|_, _| Vec::new(),
        );
        assert_eq!(result, DependencyResult::Failed);
    }

    #[test]
    fn test_aftercorr_per_task_correspondence() {
        // Parent task 1 failed, tasks 0 and 2 completed.
        let tasks = vec![
            make_array_task(100, 0, JobState::Completed),
            make_array_task(100, 1, JobState::Failed),
            make_array_task(100, 2, JobState::Completed),
        ];
        let get_tasks = |id: JobId| if id == 100 { tasks.clone() } else { Vec::new() };

        // Child task 0 → corresponds to parent task 0 (Completed) → Satisfied.
        let mut child0 = Job::new(
            10,
            JobSpec {
                name: "c".into(),
                user: "alice".into(),
                dependency: vec!["aftercorr:100".into()],
                array_job_id: Some(9),
                array_task_id: Some(0),
                ..Default::default()
            },
        );
        child0.spec.array_task_id = Some(0);
        assert_eq!(
            check_dependencies(&child0, &|_| None, &get_tasks, &|_, _| Vec::new()),
            DependencyResult::Satisfied
        );

        // Child task 1 → parent task 1 (Failed) → Failed.
        let mut child1 = child0.clone();
        child1.spec.array_task_id = Some(1);
        assert_eq!(
            check_dependencies(&child1, &|_| None, &get_tasks, &|_, _| Vec::new()),
            DependencyResult::Failed
        );

        // Child task 2 → parent task 2 (Completed) → Satisfied.
        let mut child2 = child0.clone();
        child2.spec.array_task_id = Some(2);
        assert_eq!(
            check_dependencies(&child2, &|_| None, &get_tasks, &|_, _| Vec::new()),
            DependencyResult::Satisfied
        );
    }

    #[test]
    fn test_parse_after_with_delay() {
        let deps = try_parse_dependencies(&["after:100+5".into()]).unwrap();
        assert_eq!(
            deps,
            vec![Dependency::After {
                job_id: 100,
                delay_minutes: 5
            }]
        );
    }

    #[test]
    fn test_parse_after_without_delay() {
        let deps = try_parse_dependencies(&["after:100".into()]).unwrap();
        assert_eq!(
            deps,
            vec![Dependency::After {
                job_id: 100,
                delay_minutes: 0
            }]
        );
    }

    #[test]
    fn test_parse_unknown_type_rejected() {
        let err = try_parse_dependencies(&["expand:100".into()]).unwrap_err();
        assert_eq!(err, DependencyParseError::UnknownType("expand".into()));
    }

    #[test]
    fn test_parse_invalid_id_rejected() {
        let err = try_parse_dependencies(&["afterok:abc".into()]).unwrap_err();
        assert!(matches!(err, DependencyParseError::InvalidJobId(_)));
    }

    #[test]
    fn test_after_time_gate_waits_then_satisfies() {
        // Parent started 10 minutes ago; child has after:100+5 → eligible.
        let mut parent = Job::new(
            100,
            JobSpec {
                name: "p".into(),
                user: "alice".into(),
                ..Default::default()
            },
        );
        parent.start_time = Some(chrono::Utc::now() - chrono::Duration::minutes(10));
        let job = Job::new(
            1,
            JobSpec {
                name: "c".into(),
                user: "alice".into(),
                dependency: vec!["after:100+5".into()],
                ..Default::default()
            },
        );
        let result = check_dependencies(
            &job,
            &|id| {
                if id == 100 {
                    Some(parent.clone())
                } else {
                    None
                }
            },
            &|_| Vec::new(),
            &|_, _| Vec::new(),
        );
        assert_eq!(result, DependencyResult::Satisfied);

        // Parent started just now; after:100+5 → not yet eligible.
        let mut parent_now = parent.clone();
        parent_now.start_time = Some(chrono::Utc::now());
        let result2 = check_dependencies(
            &job,
            &|id| {
                if id == 100 {
                    Some(parent_now.clone())
                } else {
                    None
                }
            },
            &|_| Vec::new(),
            &|_, _| Vec::new(),
        );
        assert_eq!(result2, DependencyResult::Waiting);
    }

    #[test]
    fn test_after_parent_cancelled_while_pending_is_satisfied() {
        // Parent reached a terminal state without ever starting (cancelled while
        // Pending → start_time is None). `after` must NOT deadlock; it should
        // release since the parent can never start.
        let parent = make_job(100, JobState::Cancelled);
        assert!(parent.start_time.is_none());
        let job = Job::new(
            1,
            JobSpec {
                name: "c".into(),
                user: "alice".into(),
                dependency: vec!["after:100".into()],
                ..Default::default()
            },
        );
        let result = check_dependencies(
            &job,
            &|id| {
                if id == 100 {
                    Some(parent.clone())
                } else {
                    None
                }
            },
            &|_| Vec::new(),
            &|_, _| Vec::new(),
        );
        assert_eq!(result, DependencyResult::Satisfied);
    }

    #[test]
    fn test_after_parent_pending_not_started_waits() {
        // Parent exists, still Pending, not started → after must wait.
        let parent = make_job(100, JobState::Pending);
        let job = Job::new(
            1,
            JobSpec {
                name: "c".into(),
                user: "alice".into(),
                dependency: vec!["after:100".into()],
                ..Default::default()
            },
        );
        let result = check_dependencies(
            &job,
            &|id| {
                if id == 100 {
                    Some(parent.clone())
                } else {
                    None
                }
            },
            &|_| Vec::new(),
            &|_, _| Vec::new(),
        );
        assert_eq!(result, DependencyResult::Waiting);
    }

    #[test]
    fn test_after_unknown_parent_is_satisfied() {
        // Unknown parent id — Slurm treats `after` as satisfiable by absence.
        let job = Job::new(
            1,
            JobSpec {
                name: "c".into(),
                user: "alice".into(),
                dependency: vec!["after:9999".into()],
                ..Default::default()
            },
        );
        let result = check_dependencies(&job, &|_| None, &|_| Vec::new(), &|_, _| Vec::new());
        assert_eq!(result, DependencyResult::Satisfied);
    }

    fn child_with_dep(dep: &str) -> Job {
        Job::new(
            1,
            JobSpec {
                name: "c".into(),
                user: "alice".into(),
                dependency: vec![dep.into()],
                ..Default::default()
            },
        )
    }

    #[test]
    fn test_afterany_array_parent_terminal_satisfied() {
        // afterany over an array: satisfied once all tasks are terminal,
        // regardless of success/failure.
        let tasks = vec![
            make_array_task(100, 0, JobState::Completed),
            make_array_task(100, 1, JobState::Failed),
        ];
        let job = child_with_dep("afterany:100");
        let result = check_dependencies(
            &job,
            &|_| None,
            &|id| if id == 100 { tasks.clone() } else { Vec::new() },
            &|_, _| Vec::new(),
        );
        assert_eq!(result, DependencyResult::Satisfied);
    }

    #[test]
    fn test_afterany_array_parent_unfinished_waits() {
        let tasks = vec![
            make_array_task(100, 0, JobState::Completed),
            make_array_task(100, 1, JobState::Running),
        ];
        let job = child_with_dep("afterany:100");
        let result = check_dependencies(
            &job,
            &|_| None,
            &|id| if id == 100 { tasks.clone() } else { Vec::new() },
            &|_, _| Vec::new(),
        );
        assert_eq!(result, DependencyResult::Waiting);
    }

    #[test]
    fn test_afternotok_array_parent_any_failure_satisfied() {
        // afternotok over an array: the aggregate is Failed (one task failed),
        // so the "must not succeed" condition is satisfied.
        let tasks = vec![
            make_array_task(100, 0, JobState::Completed),
            make_array_task(100, 1, JobState::Failed),
        ];
        let job = child_with_dep("afternotok:100");
        let result = check_dependencies(
            &job,
            &|_| None,
            &|id| if id == 100 { tasks.clone() } else { Vec::new() },
            &|_, _| Vec::new(),
        );
        assert_eq!(result, DependencyResult::Satisfied);
    }

    #[test]
    fn test_afternotok_array_parent_all_ok_fails() {
        // All tasks completed → aggregate Completed → afternotok can never hold.
        let tasks = vec![
            make_array_task(100, 0, JobState::Completed),
            make_array_task(100, 1, JobState::Completed),
        ];
        let job = child_with_dep("afternotok:100");
        let result = check_dependencies(
            &job,
            &|_| None,
            &|id| if id == 100 { tasks.clone() } else { Vec::new() },
            &|_, _| Vec::new(),
        );
        assert_eq!(result, DependencyResult::Failed);
    }

    #[test]
    fn test_aftercorr_scalar_parent_degrades_to_afterok() {
        // Child is NOT an array task; parent is a scalar job. aftercorr behaves
        // like afterok.
        let parent_ok = make_job(100, JobState::Completed);
        let job = child_with_dep("aftercorr:100");
        assert_eq!(
            check_dependencies(
                &job,
                &|id| if id == 100 {
                    Some(parent_ok.clone())
                } else {
                    None
                },
                &|_| Vec::new(),
                &|_, _| Vec::new(),
            ),
            DependencyResult::Satisfied
        );

        let parent_fail = make_job(100, JobState::Failed);
        assert_eq!(
            check_dependencies(
                &job,
                &|id| if id == 100 {
                    Some(parent_fail.clone())
                } else {
                    None
                },
                &|_| Vec::new(),
                &|_, _| Vec::new(),
            ),
            DependencyResult::Failed
        );
    }

    #[test]
    fn test_aftercorr_array_child_missing_parent_task_fails() {
        // Child task 5 but parent array only has tasks 0..2 — no corresponding
        // parent task, so it can never correspond.
        let tasks = vec![
            make_array_task(100, 0, JobState::Completed),
            make_array_task(100, 1, JobState::Completed),
        ];
        let mut child = child_with_dep("aftercorr:100");
        child.spec.array_job_id = Some(9);
        child.spec.array_task_id = Some(5);
        let result = check_dependencies(
            &child,
            &|_| None,
            &|id| if id == 100 { tasks.clone() } else { Vec::new() },
            &|_, _| Vec::new(),
        );
        assert_eq!(result, DependencyResult::Failed);
    }

    #[test]
    fn test_aftercorr_array_child_parent_task_still_running_waits() {
        let tasks = vec![
            make_array_task(100, 0, JobState::Completed),
            make_array_task(100, 1, JobState::Running),
        ];
        let mut child = child_with_dep("aftercorr:100");
        child.spec.array_task_id = Some(1);
        let result = check_dependencies(
            &child,
            &|_| None,
            &|id| if id == 100 { tasks.clone() } else { Vec::new() },
            &|_, _| Vec::new(),
        );
        assert_eq!(result, DependencyResult::Waiting);
    }
}
