// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! T56: Job dependency tests.
//!
//! Tests dependency parsing, satisfaction checking, and edge cases.

#[cfg(test)]
mod tests {
    use spur_core::dependency::*;
    use spur_core::job::*;

    fn make_job_state(id: JobId, state: JobState) -> Job {
        let mut job = Job::new(
            id,
            JobSpec {
                name: "dep".into(),
                user: "alice".into(),
                ..Default::default()
            },
        );
        match state {
            JobState::Pending => {}
            // Deadline is only reachable directly from Pending; it must not
            // pass through Running like the other terminal states.
            JobState::Deadline => {
                let _ = job.transition(JobState::Deadline);
            }
            _ => {
                let _ = job.transition(JobState::Running);
                if state != JobState::Running {
                    let _ = job.transition(state);
                }
            }
        }
        job
    }

    // ── T56.1: Parse dependency strings ──────────────────────────

    #[test]
    fn t56_1_parse_afterok() {
        let deps = parse_dependencies(&["afterok:100".into()]);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0], Dependency::AfterOk(100));
    }

    #[test]
    fn t56_2_parse_afterany() {
        let deps = parse_dependencies(&["afterany:200".into()]);
        assert_eq!(deps[0], Dependency::AfterAny(200));
    }

    #[test]
    fn t56_3_parse_afternotok() {
        let deps = parse_dependencies(&["afternotok:300".into()]);
        assert_eq!(deps[0], Dependency::AfterNotOk(300));
    }

    #[test]
    fn t56_4_parse_singleton() {
        let deps = parse_dependencies(&["singleton".into()]);
        assert_eq!(deps[0], Dependency::Singleton);
    }

    #[test]
    fn t56_5_parse_multiple() {
        let deps = parse_dependencies(&["afterok:100,afterany:200".into()]);
        assert_eq!(deps.len(), 2);
    }

    #[test]
    fn t56_6_parse_empty() {
        let deps = parse_dependencies(&[]);
        assert!(deps.is_empty());
    }

    // ── T56.7: afterok satisfied when completed ──────────────────

    #[test]
    fn t56_7_afterok_completed() {
        let dep = make_job_state(100, JobState::Completed);
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
                    Some(dep.clone())
                } else {
                    None
                }
            },
            &|_| Vec::new(),
            &|_, _| Vec::new(),
        );
        assert_eq!(result, DependencyResult::Satisfied);
    }

    // ── T56.8: afterok waiting when running ──────────────────────

    #[test]
    fn t56_8_afterok_running() {
        let dep = make_job_state(100, JobState::Running);
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
                    Some(dep.clone())
                } else {
                    None
                }
            },
            &|_| Vec::new(),
            &|_, _| Vec::new(),
        );
        assert_eq!(result, DependencyResult::Waiting);
    }

    // ── T56.9: afterok failed when dependency failed ─────────────

    #[test]
    fn t56_9_afterok_dep_failed() {
        let dep = make_job_state(100, JobState::Failed);
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
                    Some(dep.clone())
                } else {
                    None
                }
            },
            &|_| Vec::new(),
            &|_, _| Vec::new(),
        );
        assert_eq!(result, DependencyResult::Failed);
    }

    // ── T56.10: afterany satisfied on any terminal ───────────────

    #[test]
    fn t56_10_afterany_failed_is_ok() {
        let dep = make_job_state(100, JobState::Failed);
        let job = Job::new(
            1,
            JobSpec {
                name: "test".into(),
                user: "alice".into(),
                dependency: vec!["afterany:100".into()],
                ..Default::default()
            },
        );
        let result = check_dependencies(
            &job,
            &|id| {
                if id == 100 {
                    Some(dep.clone())
                } else {
                    None
                }
            },
            &|_| Vec::new(),
            &|_, _| Vec::new(),
        );
        assert_eq!(result, DependencyResult::Satisfied);
    }

    // ── T56.11: afternotok satisfied when failed ─────────────────

    #[test]
    fn t56_11_afternotok_failed() {
        let dep = make_job_state(100, JobState::Failed);
        let job = Job::new(
            1,
            JobSpec {
                name: "test".into(),
                user: "alice".into(),
                dependency: vec!["afternotok:100".into()],
                ..Default::default()
            },
        );
        let result = check_dependencies(
            &job,
            &|id| {
                if id == 100 {
                    Some(dep.clone())
                } else {
                    None
                }
            },
            &|_| Vec::new(),
            &|_, _| Vec::new(),
        );
        assert_eq!(result, DependencyResult::Satisfied);
    }

    // ── T56.12: afternotok failed when completed ─────────────────

    #[test]
    fn t56_12_afternotok_completed() {
        let dep = make_job_state(100, JobState::Completed);
        let job = Job::new(
            1,
            JobSpec {
                name: "test".into(),
                user: "alice".into(),
                dependency: vec!["afternotok:100".into()],
                ..Default::default()
            },
        );
        let result = check_dependencies(
            &job,
            &|id| {
                if id == 100 {
                    Some(dep.clone())
                } else {
                    None
                }
            },
            &|_| Vec::new(),
            &|_, _| Vec::new(),
        );
        assert_eq!(result, DependencyResult::Failed);
    }

    // ── T56.13: singleton blocks when same name running ──────────

    #[test]
    fn t56_13_singleton_blocked() {
        let running = make_job_state(50, JobState::Running);
        let mut running_named = running.clone();
        running_named.spec.name = "train".into();
        running_named.spec.user = "alice".into();

        let job = Job::new(
            1,
            JobSpec {
                name: "train".into(),
                user: "alice".into(),
                dependency: vec!["singleton".into()],
                ..Default::default()
            },
        );

        let result = check_dependencies(&job, &|_| None, &|_| Vec::new(), &|name, user| {
            if name == "train" && user == "alice" {
                vec![running_named.clone()]
            } else {
                Vec::new()
            }
        });
        assert_eq!(result, DependencyResult::Waiting);
    }

    // ── T56.14: singleton satisfied when no same name running ────

    #[test]
    fn t56_14_singleton_satisfied() {
        let job = Job::new(
            1,
            JobSpec {
                name: "train".into(),
                user: "alice".into(),
                dependency: vec!["singleton".into()],
                ..Default::default()
            },
        );

        let result = check_dependencies(&job, &|_| None, &|_| Vec::new(), &|_, _| Vec::new());
        assert_eq!(result, DependencyResult::Satisfied);
    }

    // ── T56.15: No dependencies = satisfied ──────────────────────

    #[test]
    fn t56_15_no_deps() {
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

    // ── T56.17: afterok failed when dependency hit deadline ──────

    #[test]
    fn t56_17_afterok_dep_deadline() {
        let dep = make_job_state(100, JobState::Deadline);
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
                    Some(dep.clone())
                } else {
                    None
                }
            },
            &|_| Vec::new(),
            &|_, _| Vec::new(),
        );
        assert_eq!(result, DependencyResult::Failed);
    }

    // ── T56.18: afternotok satisfied when dependency hit deadline ─

    #[test]
    fn t56_18_afternotok_dep_deadline() {
        let dep = make_job_state(100, JobState::Deadline);
        let job = Job::new(
            1,
            JobSpec {
                name: "test".into(),
                user: "alice".into(),
                dependency: vec!["afternotok:100".into()],
                ..Default::default()
            },
        );
        let result = check_dependencies(
            &job,
            &|id| {
                if id == 100 {
                    Some(dep.clone())
                } else {
                    None
                }
            },
            &|_| Vec::new(),
            &|_, _| Vec::new(),
        );
        assert_eq!(result, DependencyResult::Satisfied);
    }

    // ── T56.19: afterany satisfied when dependency hit deadline ───

    #[test]
    fn t56_19_afterany_dep_deadline() {
        let dep = make_job_state(100, JobState::Deadline);
        let job = Job::new(
            1,
            JobSpec {
                name: "test".into(),
                user: "alice".into(),
                dependency: vec!["afterany:100".into()],
                ..Default::default()
            },
        );
        let result = check_dependencies(
            &job,
            &|id| {
                if id == 100 {
                    Some(dep.clone())
                } else {
                    None
                }
            },
            &|_| Vec::new(),
            &|_, _| Vec::new(),
        );
        assert_eq!(result, DependencyResult::Satisfied);
    }

    // ── T56.16: Multiple dependencies all must be met ────────────

    #[test]
    fn t56_16_multiple_deps_one_waiting() {
        let done = make_job_state(100, JobState::Completed);
        let running = make_job_state(200, JobState::Running);
        let job = Job::new(
            1,
            JobSpec {
                name: "test".into(),
                user: "alice".into(),
                dependency: vec!["afterok:100,afterok:200".into()],
                ..Default::default()
            },
        );
        let result = check_dependencies(
            &job,
            &|id| match id {
                100 => Some(done.clone()),
                200 => Some(running.clone()),
                _ => None,
            },
            &|_| Vec::new(),
            &|_, _| Vec::new(),
        );
        assert_eq!(result, DependencyResult::Waiting);
    }

    // ── T56.17+: new dependency types / parsing (issue #259) ──────
    // (Array-parent *resolution* tests live in t28_arrays.rs alongside the
    // other array coverage.)

    #[test]
    fn t56_17_parse_after_with_delay() {
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
    fn t56_18_parse_rejects_unknown_type() {
        // The defect: `expand:N` was silently accepted, deadlocking the job.
        assert!(try_parse_dependencies(&["expand:100".into()]).is_err());
    }

    #[test]
    fn t56_18b_parse_rejects_idless_and_unknown_idless() {
        // Known type with no ids must error, not return Ok([]).
        assert!(try_parse_dependencies(&["afterok:".into()]).is_err());
        // Unknown type with no ids must also error (regression: the id loop
        // never ran, so UnknownType went undetected).
        assert!(try_parse_dependencies(&["expand:".into()]).is_err());
    }

    #[test]
    fn t56_18c_parse_rejects_delay_on_non_after() {
        // `+M` delay suffix is only valid for `after`; other types must reject
        // it instead of silently dropping the delay.
        assert!(try_parse_dependencies(&["afterok:123+5".into()]).is_err());
        // `after` still accepts it.
        assert!(try_parse_dependencies(&["after:123+5".into()]).is_ok());
    }

    #[test]
    fn t56_19_after_unknown_parent_satisfied() {
        let job = Job::new(
            1,
            JobSpec {
                name: "child".into(),
                user: "alice".into(),
                dependency: vec!["after:9999".into()],
                ..Default::default()
            },
        );
        let result = check_dependencies(&job, &|_| None, &|_| Vec::new(), &|_, _| Vec::new());
        assert_eq!(result, DependencyResult::Satisfied);
    }
}
