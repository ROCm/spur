//! T28: Job arrays.
//!
//! Corresponds to Slurm's test28.x series.
//! Tests array spec parsing, array task ID assignment, path resolution.

#[cfg(test)]
mod tests {
    use spur_core::job::*;

    // ── T28.1: Array spec storage ────────────────────────────────

    #[test]
    fn t28_1_array_spec_stored() {
        let job = Job::new(
            1,
            JobSpec {
                name: "array".into(),
                user: "test".into(),
                array_spec: Some("0-99%10".into()),
                ..Default::default()
            },
        );
        assert_eq!(job.spec.array_spec, Some("0-99%10".into()));
    }

    #[test]
    fn t28_2_array_spec_none_for_regular_job() {
        let job = Job::new(
            1,
            JobSpec {
                name: "regular".into(),
                user: "test".into(),
                ..Default::default()
            },
        );
        assert!(job.spec.array_spec.is_none());
    }

    // ── T28.3: Array path resolution ─────────────────────────────

    #[test]
    fn t28_3_array_path_with_task_id() {
        let mut job = Job::new(
            100,
            JobSpec {
                name: "array_job".into(),
                user: "test".into(),
                stdout_path: Some("output_%A_%a.log".into()),
                ..Default::default()
            },
        );
        job.spec.array_job_id = Some(100);
        job.spec.array_task_id = Some(5);

        assert_eq!(job.resolved_stdout(), "output_100_5.log");
    }

    #[test]
    fn t28_4_array_path_without_task_id() {
        let job = Job::new(
            42,
            JobSpec {
                name: "regular".into(),
                user: "test".into(),
                stdout_path: Some("output_%A_%a.log".into()),
                ..Default::default()
            },
        );
        // %A and %a should not be substituted for non-array jobs
        assert_eq!(job.resolved_stdout(), "output_%A_%a.log");
    }

    // ── T28.5: Array job IDs ─────────────────────────────────────

    #[test]
    fn t28_5_array_job_id_tracking() {
        let mut job = Job::new(
            200,
            JobSpec {
                name: "array".into(),
                user: "test".into(),
                ..Default::default()
            },
        );
        job.spec.array_job_id = Some(200);
        job.spec.array_task_id = Some(42);

        assert_eq!(job.spec.array_job_id, Some(200));
        assert_eq!(job.spec.array_task_id, Some(42));
    }

    // ── T28.6: Array tasks are independent jobs ──────────────────

    #[test]
    fn t28_6_array_tasks_independent_state() {
        let mut task1 = Job::new(
            201,
            JobSpec {
                name: "array".into(),
                user: "test".into(),
                ..Default::default()
            },
        );
        task1.spec.array_job_id = Some(200);
        task1.spec.array_task_id = Some(0);

        let mut task2 = Job::new(
            202,
            JobSpec {
                name: "array".into(),
                user: "test".into(),
                ..Default::default()
            },
        );
        task2.spec.array_job_id = Some(200);
        task2.spec.array_task_id = Some(1);

        // Transition task1 to running, task2 stays pending
        task1.transition(JobState::Running).unwrap();
        assert_eq!(task1.state, JobState::Running);
        assert_eq!(task2.state, JobState::Pending);
    }

    // ── T28.7: Array max concurrent parsing ────────────────────

    #[test]
    fn t28_7_array_max_concurrent_set() {
        use spur_core::array::parse_array_spec;

        // Parse array spec "0-9%3", verify max_concurrent = 3
        let spec = parse_array_spec("0-9%3").unwrap();
        assert_eq!(spec.max_concurrent, 3);
        assert_eq!(spec.task_ids.len(), 10);
        assert_eq!(spec.task_ids, (0..=9).collect::<Vec<_>>());
    }

    #[test]
    fn t28_8_array_max_concurrent_zero_means_unlimited() {
        use spur_core::array::parse_array_spec;

        // Without %N, max_concurrent should be 0 (unlimited)
        let spec = parse_array_spec("0-9").unwrap();
        assert_eq!(spec.max_concurrent, 0);
    }

    #[test]
    fn t28_9_array_max_concurrent_with_step() {
        use spur_core::array::parse_array_spec;

        // Combine step and concurrent limit: "0-20:5%2"
        let spec = parse_array_spec("0-20:5%2").unwrap();
        assert_eq!(spec.task_ids, vec![0, 5, 10, 15, 20]);
        assert_eq!(spec.max_concurrent, 2);
    }

    #[test]
    fn t28_10_array_max_concurrent_stored_on_spec() {
        let mut job = Job::new(
            300,
            JobSpec {
                name: "array_conc".into(),
                user: "test".into(),
                array_spec: Some("0-99%5".into()),
                ..Default::default()
            },
        );
        job.spec.array_max_concurrent = Some(5);
        assert_eq!(job.spec.array_max_concurrent, Some(5));
    }
}
