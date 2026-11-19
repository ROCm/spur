// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! T17: Job submission (sbatch / spur submit).
//!
//! Corresponds to Slurm's test17.x series.
//! Tests job spec defaults, hold, priority, flags, and GRES parsing.

#[cfg(test)]
mod tests {
    use crate::harness::*;

    // ── T17.3: Job spec defaults ─────────────────────────────────

    #[test]
    fn t17_3_job_spec_defaults() {
        let spec = spur_core::job::JobSpec::default();
        assert_eq!(spec.num_nodes, 1);
        assert_eq!(spec.num_tasks, 1);
        assert_eq!(spec.cpus_per_task, 1);
        assert!(!spec.requeue);
        assert!(!spec.exclusive);
        assert!(!spec.hold);
    }

    // ── T17.5–7: Job creation ────────────────────────────────────

    #[test]
    fn t17_5_job_submit_time_set() {
        let job = make_job("test");
        let now = chrono::Utc::now();
        let diff = (now - job.submit_time).num_seconds().abs();
        assert!(diff < 2, "submit_time should be within 2 seconds of now");
    }

    #[test]
    fn t17_6_job_initial_priority() {
        let job = make_job("test");
        assert_eq!(job.priority, 1000); // Default priority
    }

    #[test]
    fn t17_7_job_custom_priority() {
        let job = spur_core::job::Job::new(
            1,
            spur_core::job::JobSpec {
                name: "test".into(),
                user: "alice".into(),
                priority: Some(5000),
                ..Default::default()
            },
        );
        assert_eq!(job.priority, 5000);
    }

    // ── T17.8: Hold flag ─────────────────────────────────────────

    #[test]
    fn t17_8_hold_sets_pending_reason() {
        let job = spur_core::job::Job::new(
            1,
            spur_core::job::JobSpec {
                name: "held".into(),
                user: "alice".into(),
                hold: true,
                ..Default::default()
            },
        );
        assert_eq!(job.state, spur_core::job::JobState::Pending);
        assert_eq!(job.pending_reason, spur_core::job::PendingReason::Held);
        assert_eq!(job.priority, 0);
    }

    // ── T17.9: Array spec ────────────────────────────────────────

    #[test]
    fn t17_9_array_spec_stored() {
        let job = spur_core::job::Job::new(
            1,
            spur_core::job::JobSpec {
                name: "array".into(),
                user: "alice".into(),
                array_spec: Some("0-99%10".into()),
                ..Default::default()
            },
        );
        assert_eq!(job.spec.array_spec, Some("0-99%10".into()));
    }

    // ── T17.10: Dependency parsing ───────────────────────────────

    #[test]
    fn t17_10_dependencies_stored() {
        let job = spur_core::job::Job::new(
            1,
            spur_core::job::JobSpec {
                name: "dep".into(),
                user: "alice".into(),
                dependency: vec!["afterok:100".into(), "afterany:200".into()],
                ..Default::default()
            },
        );
        assert_eq!(job.spec.dependency.len(), 2);
        assert_eq!(job.spec.dependency[0], "afterok:100");
    }

    #[test]
    fn t17_16_job_requeue_flag() {
        // Verify the requeue flag is stored on the job spec
        let job = spur_core::job::Job::new(
            1,
            spur_core::job::JobSpec {
                name: "requeueable".into(),
                user: "alice".into(),
                requeue: true,
                ..Default::default()
            },
        );
        assert!(job.spec.requeue);
    }

    #[test]
    fn t17_17_job_exclusive_flag() {
        // Verify the exclusive flag is stored on the job spec
        let job = spur_core::job::Job::new(
            1,
            spur_core::job::JobSpec {
                name: "exclusive".into(),
                user: "alice".into(),
                exclusive: true,
                ..Default::default()
            },
        );
        assert!(job.spec.exclusive);
    }

    // ── T17.18: License GRES format ─────────────────────────────

    #[test]
    fn t17_18_license_gres_format() {
        // Test that license parsing produces correct GRES format
        let gres = "license:fluent:5";
        let parsed = spur_core::resource::parse_gres(gres);
        assert_eq!(parsed, Some(("license".into(), Some("fluent".into()), 5)));
    }

    // ── T17.19: License GRES tracking ────────────────────────────

    #[test]
    fn t17_19_license_gres_tracking() {
        // Verify license requirements can be extracted from GRES
        let gres = "license:fluent:5";
        let parsed = spur_core::resource::parse_gres(gres);
        assert_eq!(parsed, Some(("license".into(), Some("fluent".into()), 5)));
    }
}
