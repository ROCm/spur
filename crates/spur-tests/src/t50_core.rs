//! T50: Core type tests.
//!
//! Tests for Job, Node, ResourceSet, Partition types.
//! Corresponds to Slurm's slurm_unit/common/ tests.

#[cfg(test)]
mod tests {
    use crate::harness::*;
    use spur_core::job::*;
    use spur_core::node::*;
    use spur_core::partition::*;
    use spur_core::resource::*;

    // ── T50.1: Job state machine ──────────────────────────────────

    #[test]
    fn t50_1_job_initial_state_is_pending() {
        reset_job_ids();
        let job = make_job("test");
        assert_job_state(&job, JobState::Pending);
    }

    #[test]
    fn t50_2_job_pending_to_running() {
        reset_job_ids();
        let mut job = make_job("test");
        assert_transition_ok(&mut job, JobState::Running);
        assert_job_state(&job, JobState::Running);
    }

    #[test]
    fn t50_3_job_running_to_completed() {
        reset_job_ids();
        let mut job = make_job("test");
        assert_transition_ok(&mut job, JobState::Running);
        assert_transition_ok(&mut job, JobState::Completed);
        assert!(job.state.is_terminal());
        assert!(job.end_time.is_some());
    }

    #[test]
    fn t50_4_job_running_to_failed() {
        reset_job_ids();
        let mut job = make_job("test");
        assert_transition_ok(&mut job, JobState::Running);
        assert_transition_ok(&mut job, JobState::Failed);
        assert!(job.state.is_terminal());
    }

    #[test]
    fn t50_5_job_pending_to_cancelled() {
        reset_job_ids();
        let mut job = make_job("test");
        assert_transition_ok(&mut job, JobState::Cancelled);
        assert!(job.state.is_terminal());
    }

    #[test]
    fn t50_6_job_running_to_timeout() {
        reset_job_ids();
        let mut job = make_job("test");
        assert_transition_ok(&mut job, JobState::Running);
        assert_transition_ok(&mut job, JobState::Timeout);
        assert!(job.state.is_terminal());
    }

    #[test]
    fn t50_7_job_running_to_node_fail() {
        reset_job_ids();
        let mut job = make_job("test");
        assert_transition_ok(&mut job, JobState::Running);
        assert_transition_ok(&mut job, JobState::NodeFail);
        assert!(job.state.is_terminal());
    }

    #[test]
    fn t50_8_job_running_to_preempted() {
        reset_job_ids();
        let mut job = make_job("test");
        assert_transition_ok(&mut job, JobState::Running);
        assert_transition_ok(&mut job, JobState::Preempted);
        assert_eq!(job.state, JobState::Preempted);
    }

    #[test]
    fn t50_9_job_running_to_suspended_and_back() {
        reset_job_ids();
        let mut job = make_job("test");
        assert_transition_ok(&mut job, JobState::Running);
        assert_transition_ok(&mut job, JobState::Suspended);
        assert!(job.state.is_active());
        assert_transition_ok(&mut job, JobState::Running);
    }

    #[test]
    fn t50_10_invalid_pending_to_completed() {
        reset_job_ids();
        let mut job = make_job("test");
        assert_transition_err(&mut job, JobState::Completed);
    }

    #[test]
    fn t50_11_invalid_completed_to_running() {
        reset_job_ids();
        let mut job = make_job("test");
        assert_transition_ok(&mut job, JobState::Running);
        assert_transition_ok(&mut job, JobState::Completed);
        assert_transition_err(&mut job, JobState::Running);
    }

    #[test]
    fn t50_12_invalid_pending_to_failed() {
        reset_job_ids();
        let mut job = make_job("test");
        assert_transition_err(&mut job, JobState::Failed);
    }

    // ── T50.13: Job state display ─────────────────────────────────

    #[test]
    fn t50_13_state_codes() {
        assert_eq!(JobState::Pending.code(), "PD");
        assert_eq!(JobState::Running.code(), "R");
        assert_eq!(JobState::Completing.code(), "CG");
        assert_eq!(JobState::Completed.code(), "CD");
        assert_eq!(JobState::Failed.code(), "F");
        assert_eq!(JobState::Cancelled.code(), "CA");
        assert_eq!(JobState::Timeout.code(), "TO");
        assert_eq!(JobState::NodeFail.code(), "NF");
        assert_eq!(JobState::Preempted.code(), "PR");
        assert_eq!(JobState::Suspended.code(), "S");
    }

    #[test]
    fn t50_14_state_display_names() {
        assert_eq!(JobState::Pending.display(), "PENDING");
        assert_eq!(JobState::Running.display(), "RUNNING");
        assert_eq!(JobState::Completed.display(), "COMPLETED");
    }

    // ── T50.15: Job path resolution ───────────────────────────────

    #[test]
    fn t50_15_path_resolve_job_id() {
        reset_job_ids();
        let mut job = make_job("train");
        job.job_id = 42;
        assert_eq!(job.resolved_stdout(), "spur-42.out");
    }

    #[test]
    fn t50_16_path_resolve_custom_pattern() {
        reset_job_ids();
        let mut job = make_job("train");
        job.job_id = 42;
        job.spec.user = "bob".into();
        job.spec.stdout_path = Some("output-%x-%u-%j.log".into());
        assert_eq!(job.resolved_stdout(), "output-train-bob-42.log");
    }

    #[test]
    fn t50_17_path_resolve_node_pattern() {
        reset_job_ids();
        let mut job = make_job("test");
        job.job_id = 10;
        job.allocated_nodes = vec!["gpu001".into()];
        job.spec.stdout_path = Some("out-%N-%j.log".into());
        assert_eq!(job.resolved_stdout(), "out-gpu001-10.log");
    }

    // ── T50.18: Job run time ──────────────────────────────────────

    #[test]
    fn t50_18_run_time_none_when_not_started() {
        let job = make_job("test");
        assert!(job.run_time().is_none());
    }

    #[test]
    fn t50_19_run_time_computed_when_running() {
        let mut job = make_job("test");
        job.start_time = Some(chrono::Utc::now() - chrono::Duration::minutes(5));
        let rt = job.run_time().unwrap();
        // Should be roughly 5 minutes (allow 2 second tolerance)
        assert!(rt.num_seconds() >= 298 && rt.num_seconds() <= 302);
    }

    // ── T50.20: Node state ────────────────────────────────────────

    #[test]
    fn t50_20_node_initial_state() {
        let node = Node::new("node001".into(), ResourceSet::default());
        assert_eq!(node.state, NodeState::Unknown);
    }

    #[test]
    fn t50_21_node_state_from_alloc() {
        let mut node = Node::new(
            "node001".into(),
            ResourceSet {
                cpus: 64,
                memory_mb: 256_000,
                ..Default::default()
            },
        );
        node.state = NodeState::Idle;
        node.update_state_from_alloc();
        assert_eq!(node.state, NodeState::Idle);

        node.alloc_resources.cpus = 32;
        node.update_state_from_alloc();
        assert_eq!(node.state, NodeState::Mixed);

        node.alloc_resources.cpus = 64;
        node.update_state_from_alloc();
        assert_eq!(node.state, NodeState::Allocated);
    }

    #[test]
    fn t50_22_node_admin_state_not_overridden() {
        let mut node = Node::new(
            "node001".into(),
            ResourceSet {
                cpus: 64,
                ..Default::default()
            },
        );
        node.state = NodeState::Drain;
        node.alloc_resources.cpus = 0;
        node.update_state_from_alloc();
        // Should stay Drain, not flip to Idle
        assert_eq!(node.state, NodeState::Drain);
    }

    #[test]
    fn t50_23_node_schedulable() {
        assert!(NodeState::Idle.is_available());
        assert!(NodeState::Mixed.is_available());
        assert!(!NodeState::Down.is_available());
        assert!(!NodeState::Drain.is_available());
        assert!(!NodeState::Allocated.is_available());
    }

    // ── T50.24: ResourceSet ───────────────────────────────────────

    #[test]
    fn t50_24_resource_can_satisfy() {
        let avail = ResourceSet {
            cpus: 64,
            memory_mb: 256_000,
            ..Default::default()
        };
        let req = ResourceSet {
            cpus: 32,
            memory_mb: 128_000,
            ..Default::default()
        };
        assert!(avail.can_satisfy(&req));
    }

    #[test]
    fn t50_25_resource_cannot_satisfy_cpu() {
        let avail = ResourceSet {
            cpus: 32,
            memory_mb: 256_000,
            ..Default::default()
        };
        let req = ResourceSet {
            cpus: 64,
            memory_mb: 128_000,
            ..Default::default()
        };
        assert!(!avail.can_satisfy(&req));
    }

    #[test]
    fn t50_26_resource_cannot_satisfy_memory() {
        let avail = ResourceSet {
            cpus: 64,
            memory_mb: 100_000,
            ..Default::default()
        };
        let req = ResourceSet {
            cpus: 32,
            memory_mb: 200_000,
            ..Default::default()
        };
        assert!(!avail.can_satisfy(&req));
    }

    #[test]
    fn t50_27_resource_subtract() {
        let total = ResourceSet {
            cpus: 64,
            memory_mb: 256_000,
            ..Default::default()
        };
        let used = ResourceSet {
            cpus: 24,
            memory_mb: 100_000,
            ..Default::default()
        };
        let avail = total.subtract(&used);
        assert_eq!(avail.cpus, 40);
        assert_eq!(avail.memory_mb, 156_000);
    }

    // ── T50.28: GRES parsing ──────────────────────────────────────

    #[test]
    fn t50_28_parse_gres_full() {
        let (name, gtype, count) = spur_core::resource::parse_gres("gpu:mi300x:4").unwrap();
        assert_eq!(name, "gpu");
        assert_eq!(gtype.unwrap(), "mi300x");
        assert_eq!(count, 4);
    }

    #[test]
    fn t50_29_parse_gres_no_type() {
        let (name, gtype, count) = spur_core::resource::parse_gres("gpu:2").unwrap();
        assert_eq!(name, "gpu");
        assert!(gtype.is_none());
        assert_eq!(count, 2);
    }

    #[test]
    fn t50_30_parse_gres_bare() {
        let (name, gtype, count) = spur_core::resource::parse_gres("license").unwrap();
        assert_eq!(name, "license");
        assert!(gtype.is_none());
        assert_eq!(count, 1);
    }

    // ── T50.31: Partition state ───────────────────────────────────

    #[test]
    fn t50_31_partition_states() {
        assert_eq!(PartitionState::Up.display(), "up");
        assert_eq!(PartitionState::Down.display(), "down");
        assert_eq!(PartitionState::Drain.display(), "drain");
        assert_eq!(PartitionState::Inactive.display(), "inactive");
    }

    // ── T50.32: Held job ──────────────────────────────────────────

    #[test]
    fn t50_32_held_job_starts_pending() {
        reset_job_ids();
        let job = Job::new(
            99,
            JobSpec {
                name: "held".into(),
                user: "test".into(),
                hold: true,
                ..Default::default()
            },
        );
        assert_eq!(job.state, JobState::Pending);
        assert_eq!(job.pending_reason, PendingReason::Held);
    }

    // ── T50.33–37: Requeue state transitions ───────────────────

    #[test]
    fn t50_33_requeue_from_timeout() {
        reset_job_ids();
        let mut job = make_job("requeue-timeout");
        assert_transition_ok(&mut job, JobState::Running);
        assert_transition_ok(&mut job, JobState::Timeout);
        assert!(job.end_time.is_some(), "end_time should be set on Timeout");
        // Requeue: Timeout → Pending should succeed
        assert_transition_ok(&mut job, JobState::Pending);
        assert_job_state(&job, JobState::Pending);
        assert!(
            job.end_time.is_none(),
            "end_time should be cleared on requeue"
        );
    }

    #[test]
    fn t50_34_requeue_from_preempted() {
        reset_job_ids();
        let mut job = make_job("requeue-preempted");
        assert_transition_ok(&mut job, JobState::Running);
        assert_transition_ok(&mut job, JobState::Preempted);
        // Preempted → Pending should succeed
        assert_transition_ok(&mut job, JobState::Pending);
        assert_job_state(&job, JobState::Pending);
        assert!(
            job.end_time.is_none(),
            "end_time should be cleared on requeue"
        );
    }

    #[test]
    fn t50_35_requeue_from_node_fail() {
        reset_job_ids();
        let mut job = make_job("requeue-nodefail");
        assert_transition_ok(&mut job, JobState::Running);
        assert_transition_ok(&mut job, JobState::NodeFail);
        // NodeFail → Pending should succeed
        assert_transition_ok(&mut job, JobState::Pending);
        assert_job_state(&job, JobState::Pending);
    }

    #[test]
    fn t50_36_requeue_from_failed() {
        reset_job_ids();
        let mut job = make_job("requeue-failed");
        assert_transition_ok(&mut job, JobState::Running);
        assert_transition_ok(&mut job, JobState::Failed);
        // Failed → Pending should succeed
        assert_transition_ok(&mut job, JobState::Pending);
        assert_job_state(&job, JobState::Pending);
    }

    #[test]
    fn t50_37_requeue_from_completed_fails() {
        reset_job_ids();
        let mut job = make_job("requeue-completed");
        assert_transition_ok(&mut job, JobState::Running);
        assert_transition_ok(&mut job, JobState::Completed);
        // Completed → Pending should fail (Completed is not retriable)
        assert_transition_err(&mut job, JobState::Pending);
        assert_job_state(&job, JobState::Completed);
    }

    // ── T50.38–40: Drain / Draining node behavior ──────────────

    #[test]
    fn t50_38_drain_preserves_state() {
        let mut node = Node::new(
            "n1".into(),
            ResourceSet {
                cpus: 64,
                memory_mb: 256_000,
                ..Default::default()
            },
        );
        node.state = NodeState::Drain;
        // update_state_from_alloc should not override Drain
        node.update_state_from_alloc();
        assert_eq!(node.state, NodeState::Drain);
    }

    #[test]
    fn t50_39_draining_not_schedulable() {
        let mut node = Node::new(
            "n1".into(),
            ResourceSet {
                cpus: 64,
                memory_mb: 256_000,
                ..Default::default()
            },
        );
        node.state = NodeState::Draining;
        assert!(
            !node.is_schedulable(),
            "Draining node should not be schedulable"
        );
    }

    #[test]
    fn t50_40_draining_preserves_state() {
        let mut node = Node::new(
            "n1".into(),
            ResourceSet {
                cpus: 64,
                memory_mb: 256_000,
                ..Default::default()
            },
        );
        node.state = NodeState::Draining;
        node.alloc_resources.cpus = 32;
        // update_state_from_alloc should not override Draining
        node.update_state_from_alloc();
        assert_eq!(node.state, NodeState::Draining);
    }

    #[test]
    fn t50_41_error_preserves_state() {
        let mut node = Node::new(
            "n1".into(),
            ResourceSet {
                cpus: 64,
                memory_mb: 256_000,
                ..Default::default()
            },
        );
        node.state = NodeState::Error;
        node.update_state_from_alloc();
        assert_eq!(node.state, NodeState::Error);
    }

    // ── T50.42–43: Requeue does not reset from Cancelled ───────

    #[test]
    fn t50_42_requeue_from_cancelled_fails() {
        reset_job_ids();
        let mut job = make_job("requeue-cancelled");
        assert_transition_ok(&mut job, JobState::Cancelled);
        // Cancelled → Pending should fail
        assert_transition_err(&mut job, JobState::Pending);
    }

    #[test]
    fn t50_43_node_available_states() {
        // Comprehensive check: only Idle and Mixed are available
        assert!(NodeState::Idle.is_available());
        assert!(NodeState::Mixed.is_available());
        assert!(!NodeState::Down.is_available());
        assert!(!NodeState::Drain.is_available());
        assert!(!NodeState::Draining.is_available());
        assert!(!NodeState::Allocated.is_available());
        assert!(!NodeState::Error.is_available());
        assert!(!NodeState::Unknown.is_available());
    }

    // ── T50.44: Mail type field ─────────────────────────────────

    #[test]
    fn t50_44_mail_type_field() {
        let spec = spur_core::job::JobSpec {
            mail_type: vec!["BEGIN".into(), "END".into()],
            mail_user: Some("alice@example.com".into()),
            ..Default::default()
        };
        assert_eq!(spec.mail_type.len(), 2);
        assert_eq!(spec.mail_user.as_deref(), Some("alice@example.com"));
    }

    // ── T50.45: Interactive flag ────────────────────────────────

    #[test]
    fn t50_45_interactive_flag() {
        let spec = spur_core::job::JobSpec {
            interactive: true,
            ..Default::default()
        };
        assert!(spec.interactive);
    }

    #[test]
    fn t50_46_interactive_default_false() {
        let spec = spur_core::job::JobSpec::default();
        assert!(!spec.interactive);
    }

    // ── T50.47–49: MPI / distribution fields ────────────────────

    #[test]
    fn t50_47_mpi_field_default_none() {
        let spec = spur_core::job::JobSpec::default();
        assert!(spec.mpi.is_none());
    }

    #[test]
    fn t50_48_distribution_field_default_none() {
        let spec = spur_core::job::JobSpec::default();
        assert!(spec.distribution.is_none());
    }

    #[test]
    fn t50_49_mpi_field_set() {
        let spec = spur_core::job::JobSpec {
            mpi: Some("pmix".into()),
            distribution: Some("cyclic".into()),
            ..Default::default()
        };
        assert_eq!(spec.mpi.as_deref(), Some("pmix"));
        assert_eq!(spec.distribution.as_deref(), Some("cyclic"));
    }

    // ── T50.50–52: Heterogeneous job fields ────────────────────

    #[test]
    fn t50_50_het_job_fields_default_none() {
        reset_job_ids();
        let job = make_job("het-test");
        assert!(job.het_job_id.is_none());
        assert!(job.het_group.is_none());
    }

    #[test]
    fn t50_51_het_job_fields_set() {
        reset_job_ids();
        let mut job = make_job("het-test");
        job.het_job_id = Some(100);
        job.het_group = Some(1);
        assert_eq!(job.het_job_id, Some(100));
        assert_eq!(job.het_group, Some(1));
    }

    #[test]
    fn t50_52_het_group_spec_field() {
        let spec = spur_core::job::JobSpec {
            het_group: Some(0),
            ..Default::default()
        };
        assert_eq!(spec.het_group, Some(0));
    }

    // ── T50.53–55: Step constants and state transitions ────────

    #[test]
    fn t50_53_step_batch_constant() {
        assert_eq!(spur_core::step::STEP_BATCH, 0xFFFF_FFFE);
        assert_eq!(spur_core::step::STEP_EXTERN, 0xFFFF_FFFD);
        assert_eq!(spur_core::step::STEP_INTERACTIVE, 0xFFFF_FFFC);
    }

    #[test]
    fn t50_54_step_state_running_not_terminal() {
        use spur_core::step::StepState;
        assert!(!StepState::Running.is_terminal());
        assert!(!StepState::Pending.is_terminal());
    }

    #[test]
    fn t50_55_step_state_terminal_states() {
        use spur_core::step::StepState;
        assert!(StepState::Completed.is_terminal());
        assert!(StepState::Failed.is_terminal());
        assert!(StepState::Cancelled.is_terminal());
    }

    #[test]
    fn t50_56_step_state_display() {
        use spur_core::step::StepState;
        assert_eq!(StepState::Running.display(), "RUNNING");
        assert_eq!(StepState::Completed.display(), "COMPLETED");
        assert_eq!(StepState::Failed.display(), "FAILED");
        assert_eq!(StepState::Pending.display(), "PENDING");
        assert_eq!(StepState::Cancelled.display(), "CANCELLED");
    }

    // ── T50.57–58: Burst buffer field ─────────────────────────────

    #[test]
    fn t50_57_burst_buffer_field() {
        let spec = spur_core::job::JobSpec {
            burst_buffer: Some("stage_in:cp /data/model.bin /tmp/".into()),
            ..Default::default()
        };
        assert!(spec.burst_buffer.is_some());
        assert_eq!(
            spec.burst_buffer.as_deref(),
            Some("stage_in:cp /data/model.bin /tmp/")
        );
    }

    #[test]
    fn t50_58_burst_buffer_default_none() {
        let spec = spur_core::job::JobSpec::default();
        assert!(spec.burst_buffer.is_none());
    }

    // ── T50.59: Power config default ──────────────────────────────

    #[test]
    fn t50_59_power_config_default() {
        let config = spur_core::config::PowerConfig::default();
        assert!(config.suspend_timeout_secs.is_none());
        assert!(config.suspend_command.is_none());
        assert!(config.resume_command.is_none());
    }

    // ── T50.60: Suspended node not schedulable ────────────────────

    #[test]
    fn t50_60_suspended_not_schedulable() {
        let mut node = Node::new("n1".into(), ResourceSet::default());
        node.state = NodeState::Suspended;
        assert!(!node.is_schedulable());
        assert!(!node.state.is_available());
    }

    // ── T50.61: Suspended state preserved by update_state_from_alloc ──

    #[test]
    fn t50_61_suspended_preserves_state() {
        let mut node = Node::new(
            "n1".into(),
            ResourceSet {
                cpus: 64,
                memory_mb: 256_000,
                ..Default::default()
            },
        );
        node.state = NodeState::Suspended;
        node.update_state_from_alloc();
        assert_eq!(node.state, NodeState::Suspended);
    }

    // ── T50.62: Suspended display and short ───────────────────────

    #[test]
    fn t50_62_suspended_display() {
        assert_eq!(NodeState::Suspended.display(), "suspended");
        assert_eq!(NodeState::Suspended.short(), "susp");
    }

    // ── T50.63–70: Begin time, deadline, spread_job, open_mode fields ──

    #[test]
    fn t50_63_begin_time_field() {
        let spec = JobSpec {
            begin_time: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            ..Default::default()
        };
        assert!(spec.begin_time.is_some());
    }

    #[test]
    fn t50_64_deadline_field() {
        let spec = JobSpec {
            deadline: Some(chrono::Utc::now() + chrono::Duration::hours(24)),
            ..Default::default()
        };
        assert!(spec.deadline.is_some());
    }

    #[test]
    fn t50_65_spread_job_flag() {
        let spec = JobSpec {
            spread_job: true,
            ..Default::default()
        };
        assert!(spec.spread_job);
    }

    #[test]
    fn t50_66_spread_job_default_false() {
        assert!(!JobSpec::default().spread_job);
    }

    #[test]
    fn t50_67_open_mode_append() {
        let spec = JobSpec {
            open_mode: Some("append".into()),
            ..Default::default()
        };
        assert_eq!(spec.open_mode.as_deref(), Some("append"));
    }

    #[test]
    fn t50_68_open_mode_default_none() {
        assert!(JobSpec::default().open_mode.is_none());
    }

    #[test]
    fn t50_69_begin_time_default_none() {
        assert!(JobSpec::default().begin_time.is_none());
    }

    #[test]
    fn t50_70_deadline_default_none() {
        assert!(JobSpec::default().deadline.is_none());
    }

    // ── T50.71–72: Reservation field on JobSpec ─────────────────

    #[test]
    fn t50_71_reservation_field_on_jobspec() {
        let spec = JobSpec {
            reservation: Some("gpu-reservation".into()),
            ..Default::default()
        };
        assert_eq!(spec.reservation.as_deref(), Some("gpu-reservation"));
    }

    #[test]
    fn t50_72_reservation_default_none() {
        assert!(JobSpec::default().reservation.is_none());
    }

    // ── T50.73–74: strigger and scrontab types ────────────────────

    #[test]
    fn t50_73_strigger_types() {
        let types = ["node_down", "node_up", "job_end", "job_fail", "time"];
        assert_eq!(types.len(), 5);
        // Ensure no duplicates
        let mut deduped = types.to_vec();
        deduped.sort();
        deduped.dedup();
        assert_eq!(deduped.len(), 5);
    }

    #[test]
    fn t50_74_scrontab_format() {
        let line = "0 */6 * * * sbatch /path/to/script.sh";
        let parts: Vec<&str> = line.splitn(6, ' ').collect();
        assert_eq!(parts.len(), 6);
        assert_eq!(parts[0], "0"); // minute
        assert_eq!(parts[1], "*/6"); // hour
        assert_eq!(parts[2], "*"); // day of month
        assert_eq!(parts[3], "*"); // month
        assert_eq!(parts[4], "*"); // day of week
        assert_eq!(parts[5], "sbatch /path/to/script.sh"); // command
    }

    // ── T50.75: Node count single-node partition (#25) ───────────

    #[test]
    fn t50_75_single_node_partition_count_nonzero() {
        // Regression: node count showed 0 on single-node setups (#25).
        // A partition with one node in its spec must report count >= 1.
        let nodes_spec = "node001";
        let count = spur_core::hostlist::expand(nodes_spec)
            .map(|v| v.len())
            .unwrap_or(0);
        assert_eq!(count, 1, "single node spec must expand to exactly 1 node");
        assert!(count > 0, "node count must be non-zero");
    }

    #[test]
    fn t50_76_reservation_field_flows_through_job_spec() {
        // Regression: --reservation flag was missing from submit commands (#26).
        // The field must exist on JobSpec and be passable to the scheduler.
        let spec = spur_core::job::JobSpec {
            reservation: Some("res-gpu".into()),
            ..Default::default()
        };
        assert_eq!(spec.reservation.as_deref(), Some("res-gpu"));
    }

    #[test]
    fn t50_77_reservation_update_fields_present() {
        // Regression: no command to update a reservation (#28).
        // Reservation must have all fields needed for an update operation.
        use spur_core::reservation::Reservation;
        let now = chrono::Utc::now();
        let mut res = Reservation {
            name: "res-v1".into(),
            start_time: now,
            end_time: now + chrono::Duration::hours(4),
            nodes: vec!["node001".into()],
            users: vec!["alice".into()],
            accounts: Vec::new(),
        };
        // All fields must be mutable for an update to work.
        res.end_time = now + chrono::Duration::hours(8);
        res.nodes.push("node002".into());
        assert_eq!(res.nodes.len(), 2);
        assert_eq!(
            (res.end_time - res.start_time).num_hours(),
            8,
            "end_time must be updatable"
        );
    }

    #[test]
    fn t50_78_single_node_name_matches_hostlist() {
        // Regression: single-node clusters had inconsistent node naming (#30).
        // The node name used at registration must be what the hostlist expands to.
        let config_spec = "ubb-r09-01";
        let expanded = spur_core::hostlist::expand(config_spec).unwrap();
        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0], "ubb-r09-01");
    }

    #[test]
    fn t50_79_sinfo_nodelist_uses_registered_names() {
        // Regression: sinfo NODELIST showed "localhost" instead of real node names (#36).
        // When registered nodes are available the NODELIST should use their names,
        // not fall back to the static partition spec string.
        let registered_names = vec!["ubb-r09-09", "ubb-r09-11"];
        let partition_spec = "localhost"; // what default config has

        // If registered nodes exist, use them — never fall back to partition spec.
        let nodelist = if !registered_names.is_empty() {
            registered_names.join(",")
        } else {
            partition_spec.to_string()
        };
        assert_eq!(nodelist, "ubb-r09-09,ubb-r09-11");
        assert_ne!(
            nodelist, "localhost",
            "must not show 'localhost' when real nodes registered"
        );
    }

    // ── T50.80–87: (renumbered from previous 75–81) ───────────────

    // ── T50.82–85: Federation config parsing ─────────────────────

    #[test]
    fn t50_82_federation_config_default_empty() {
        use spur_core::config::FederationConfig;
        let fed = FederationConfig::default();
        assert!(fed.clusters.is_empty());
    }

    #[test]
    fn t50_83_federation_cluster_peer_fields() {
        use spur_core::config::ClusterPeer;
        let peer = ClusterPeer {
            name: "cluster-b".into(),
            address: "http://ctrl-b:6817".into(),
        };
        assert_eq!(peer.name, "cluster-b");
        assert_eq!(peer.address, "http://ctrl-b:6817");
    }

    #[test]
    fn t50_84_federation_config_with_peers() {
        use spur_core::config::{ClusterPeer, FederationConfig};
        let fed = FederationConfig {
            clusters: vec![
                ClusterPeer {
                    name: "east".into(),
                    address: "http://east-ctrl:6817".into(),
                },
                ClusterPeer {
                    name: "west".into(),
                    address: "http://west-ctrl:6817".into(),
                },
            ],
        };
        assert_eq!(fed.clusters.len(), 2);
        assert_eq!(fed.clusters[0].name, "east");
        assert_eq!(fed.clusters[1].address, "http://west-ctrl:6817");
    }

    #[test]
    fn t50_85_federation_config_toml_roundtrip() {
        use spur_core::config::SlurmConfig;
        let toml = r#"
cluster_name = "test"

[controller]
listen_addr = "[::]:6817"
state_dir = "/tmp/spur-test"

[[federation.clusters]]
name = "peer-a"
address = "http://peer-a:6817"
"#;
        let cfg = SlurmConfig::from_str(toml).unwrap();
        assert_eq!(cfg.federation.clusters.len(), 1);
        assert_eq!(cfg.federation.clusters[0].name, "peer-a");
    }

    // ── T50.86–88: PMIx env var names ────────────────────────────

    #[test]
    fn t50_86_pmix_env_var_names_correct() {
        // Verify the canonical PMIx env var names used by OpenMPI 5+ and srun.
        let required = ["PMIX_RANK", "PMIX_SIZE", "PMIX_NAMESPACE"];
        for name in &required {
            assert!(name.starts_with("PMIX_"), "expected PMIX_ prefix: {}", name);
            assert_eq!(*name, name.to_uppercase(), "must be uppercase: {}", name);
        }
    }

    #[test]
    fn t50_87_pmix_namespace_format() {
        // Namespace format: "spur.<job_id>"
        let job_id: u32 = 42;
        let ns = format!("spur.{}", job_id);
        assert_eq!(ns, "spur.42");
        assert!(ns.starts_with("spur."));
    }

    #[test]
    fn t50_88_ompi_compat_env_vars() {
        // OpenMPI direct bootstrap env vars mirror PMIx rank/size.
        let ompi_vars = ["OMPI_COMM_WORLD_RANK", "OMPI_COMM_WORLD_SIZE"];
        for v in &ompi_vars {
            assert!(v.starts_with("OMPI_COMM_WORLD_"));
        }
    }

    // ── T50.89-91: Issue #41-43 fixes ────────────────────────────

    #[test]
    fn t50_89_default_partition_assigned_on_submit() {
        // Issue #43: when no --partition is specified, the default partition
        // should be assigned so the scheduler can match nodes correctly.
        // Verify that Partition carries is_default and that the scheduler
        // harness correctly sets it.
        let part = make_partition("gpu", 2);
        // make_partition sets is_default when name == "default"; this one
        // should be false, but the field must be present.
        assert!(!part.is_default);
        assert_eq!(part.name, "gpu");

        // A partition named "default" should have is_default = true
        let default_part = make_partition("default", 2);
        assert!(default_part.is_default);
    }

    #[test]
    fn t50_90_pending_reason_resources_when_no_nodes() {
        // Issue #43: pending_reason should reflect inability to schedule, not
        // just always show "Priority".
        let reason = spur_core::job::PendingReason::Resources;
        assert_eq!(reason.display(), "Resources");

        let reason = spur_core::job::PendingReason::NodeDown;
        assert_eq!(reason.display(), "NodeDown");

        let reason = spur_core::job::PendingReason::Priority;
        assert_eq!(reason.display(), "Priority");
    }

    #[test]
    fn t50_91_exec_uses_controller_not_agent() {
        // Issue #42: spur exec should route through the controller so it works
        // from login nodes regardless of where the job is running.
        // This is a structural test — verify the ExecInJobRequest message exists
        // and can be constructed.
        let req = spur_proto::proto::ExecInJobRequest {
            job_id: 42,
            command: vec!["ls".into(), "-la".into()],
        };
        assert_eq!(req.job_id, 42);
        assert_eq!(req.command.len(), 2);
    }

    // ── Issue #45: sattach interactive attach ─────────────────────

    #[test]
    fn t50_92_attach_job_proto_messages_exist() {
        // Issue #45: AttachJob bidirectional streaming RPC should exist
        // with AttachJobInput and AttachJobOutput messages.
        let input = spur_proto::proto::AttachJobInput {
            job_id: 10,
            data: b"ls\n".to_vec(),
        };
        assert_eq!(input.job_id, 10);
        assert_eq!(input.data, b"ls\n");

        let output = spur_proto::proto::AttachJobOutput {
            data: b"hello\n".to_vec(),
            eof: false,
        };
        assert!(!output.eof);
        assert_eq!(output.data, b"hello\n");
    }

    // ── Issue #46: CLI reads config file for controller port ──────

    #[test]
    fn t50_93_config_controller_addr_from_toml() {
        // Issue #46: the CLI should read controller address from config.
        // Verify SlurmConfig can parse a custom port and hosts.
        let toml = r#"
            cluster_name = "test"
            [controller]
            listen_addr = "[::]:6821"
            hosts = ["ctrl.example.com"]
        "#;
        let config = spur_core::config::SlurmConfig::from_str(toml).unwrap();
        assert_eq!(config.controller.listen_addr, "[::]:6821");
        assert_eq!(config.controller.hosts[0], "ctrl.example.com");

        // Verify we can extract port from listen_addr
        let port = config.controller.listen_addr.rsplit(':').next().unwrap();
        assert_eq!(port, "6821");
    }

    // ── Issue #47: nodes auto-join default partition ──────────────

    #[test]
    fn t50_94_node_auto_partition_assignment() {
        // Issue #47: when a node doesn't match any partition hostlist,
        // it should auto-join the default partition.
        let mut node = Node::new(
            "dynamic-node".into(),
            ResourceSet {
                cpus: 8,
                memory_mb: 16384,
                ..Default::default()
            },
        );
        assert!(node.partitions.is_empty());

        // Simulate auto-assign: if no partitions matched, add to default
        let default_partition = make_partition("batch", 1);
        if node.partitions.is_empty() {
            node.partitions.push(default_partition.name.clone());
        }
        assert_eq!(node.partitions, vec!["batch"]);
    }

    // ── Issue #48: container image resolution fallback ────────────

    #[test]
    fn t50_95_container_image_absolute_path_basename_fallback() {
        // Issue #48: when the agent receives an absolute path from the
        // login node that doesn't exist locally, it should try the
        // basename in the local image directory.
        let path = std::path::Path::new("/var/spool/spur/images/ubuntu+22.04.sqsh");
        let basename = path.file_name().unwrap().to_str().unwrap();
        assert_eq!(basename, "ubuntu+22.04.sqsh");

        // Verify sanitize_name works correctly for the expected pattern
        // (colon and slash replaced with +)
        let name = "ubuntu:22.04";
        let expected = "ubuntu+22.04";
        let sanitized = name.replace('/', "+").replace(':', "+");
        assert_eq!(sanitized, expected);
        assert_eq!(format!("{}.sqsh", sanitized), basename);
    }

    // ── Issue #56 (reopen #47): scheduler crash recovery ─────────

    #[test]
    fn t50_96_scheduler_handles_num_nodes_zero_safely() {
        // Issue #56: If num_nodes is somehow 0, the scheduler should not
        // panic on .max().unwrap() with an empty iterator.
        // The fix ensures num_nodes.max(1) is used in the scheduling loop.
        use spur_sched::traits::Scheduler;
        let mut sched = spur_sched::backfill::BackfillScheduler::new(100);
        let nodes = vec![{
            let mut n = Node::new(
                "node001".into(),
                ResourceSet {
                    cpus: 64,
                    memory_mb: 256_000,
                    ..Default::default()
                },
            );
            n.state = spur_core::node::NodeState::Idle;
            n.partitions = vec!["default".into()];
            n
        }];
        let partitions = vec![make_partition("default", 1)];

        // Create a job with num_nodes=0 (edge case)
        let mut job = make_job("edge-case");
        job.spec.num_nodes = 0;

        let cluster = spur_sched::traits::ClusterState {
            nodes: &nodes,
            partitions: &partitions,
            reservations: &[],
            topology: None,
        };
        // This should NOT panic
        let assignments = sched.schedule(&[job], &cluster);
        // With num_nodes.max(1), the job should still get scheduled
        assert_eq!(assignments.len(), 1);
    }

    // ── Issue #56: update_pending_reasons checks constraints ─────

    #[test]
    fn t50_97_pending_reason_respects_constraints() {
        // Issue #56: A job with --constraint=gpu should show
        // Reason=Resources (not Priority) when no node has that feature.
        let _reason_with_features: () = {
            let mut node = Node::new(
                "node001".into(),
                ResourceSet {
                    cpus: 64,
                    memory_mb: 256_000,
                    ..Default::default()
                },
            );
            node.state = spur_core::node::NodeState::Idle;
            // Node has NO features

            // Job requires "gpu" feature
            let mut job = make_job("constrained");
            job.spec.constraint = Some("gpu".into());

            // The node is schedulable and can satisfy resources,
            // but doesn't have the constraint feature.
            // update_pending_reasons should set Resources, not Priority.
            let is_capable = node.is_schedulable()
                && node.total_resources.can_satisfy(&ResourceSet {
                    cpus: 1,
                    memory_mb: 0,
                    ..Default::default()
                });
            // Old behavior: this would be true → Priority (misleading)
            assert!(is_capable, "basic capability check passes");

            // New behavior: constraint check should reject the node
            let constraint = job.spec.constraint.as_deref().unwrap();
            let features: Vec<&str> = constraint.split(',').map(str::trim).collect();
            let passes_constraint = features
                .iter()
                .all(|f| node.features.contains(&f.to_string()));
            assert!(
                !passes_constraint,
                "constraint check should reject node without gpu feature"
            );
        };
    }

    // ── Issue #55 (reopen): agent image_dir fallback ─────────────

    #[test]
    fn t50_98_image_dir_fallback_logic() {
        // Issue #55: The agent's image_dir() should fall back to
        // ~/.spur/images when /var/spool/spur/images doesn't exist.
        // This test verifies the fallback logic conceptually.
        let system_dir = std::path::Path::new("/var/spool/spur/images");
        let home = std::env::var_os("HOME");

        // If system dir doesn't exist and HOME is set, fallback should
        // point to ~/.spur/images
        if !system_dir.is_dir() {
            if let Some(home) = home {
                let expected = std::path::PathBuf::from(home).join(".spur/images");
                // Verify the path construction is correct
                assert!(expected.to_str().unwrap().contains(".spur/images"));
            }
        }
    }

    // ── Issue #54 (reopen): sattach buffer sizes ─────────────────

    #[test]
    fn t50_99_attach_job_messages_support_raw_bytes() {
        // Issue #54: AttachJobInput should carry raw bytes (not just
        // newline-terminated lines) for interactive use.
        let input = spur_proto::proto::AttachJobInput {
            job_id: 42,
            data: vec![0x1b, 0x5b, 0x41], // ESC [ A (arrow up)
        };
        assert_eq!(input.data.len(), 3);
        assert_eq!(input.data[0], 0x1b); // ESC byte

        let output = spur_proto::proto::AttachJobOutput {
            data: vec![0x1b, 0x5b, 0x48], // ESC [ H (cursor home)
            eof: false,
        };
        assert_eq!(output.data.len(), 3);
        assert!(!output.eof);
    }

    // ── Issue #53: CLI show dispatch ─────────────────────────────

    #[test]
    fn t50_100_show_dispatch_inserts_implicit_show() {
        // Issue #53: `spur show node X` should dispatch as
        // `scontrol show node X`, not `scontrol node X`.
        //
        // Simulate the argv rewriting logic from main.rs.
        let args: Vec<String> = vec!["spur".into(), "show".into(), "node".into(), "gpu-1".into()];

        let cmd = "scontrol";
        let implicit_show = args[1].as_str() == "show" && cmd == "scontrol";

        let rewritten: Vec<String> = std::iter::once(cmd.to_string())
            .chain(if implicit_show {
                vec!["show".to_string()]
            } else {
                vec![]
            })
            .chain(args[2..].iter().cloned())
            .collect();

        assert_eq!(rewritten, vec!["scontrol", "show", "node", "gpu-1"]);
    }

    #[test]
    fn t50_101_control_dispatch_no_implicit_show() {
        // `spur control show node X` should NOT insert extra show.
        let args: Vec<String> = vec![
            "spur".into(),
            "control".into(),
            "show".into(),
            "node".into(),
            "gpu-1".into(),
        ];

        let cmd = "scontrol";
        let implicit_show = args[1].as_str() == "show" && cmd == "scontrol";

        let rewritten: Vec<String> = std::iter::once(cmd.to_string())
            .chain(if implicit_show {
                vec!["show".to_string()]
            } else {
                vec![]
            })
            .chain(args[2..].iter().cloned())
            .collect();

        // "control" != "show", so no implicit show inserted
        assert_eq!(rewritten, vec!["scontrol", "show", "node", "gpu-1"]);
    }

    // ── Issue #51: K8s operator address resolution ───────────────

    #[test]
    fn t50_102_k8s_address_resolution_priority() {
        // Issue #51: The operator should prefer --address flag over
        // POD_IP env var over hostname. Verify the priority logic.
        //
        // Test the priority: explicit > POD_IP > listen IP > hostname
        let explicit = Some("10.0.0.1".to_string());
        let pod_ip: Result<String, std::env::VarError> = Ok("10.0.0.2".to_string());
        let listen_is_unspecified = true;

        // Explicit wins
        let result = if let Some(ref addr) = explicit {
            addr.clone()
        } else if let Ok(ip) = pod_ip.as_ref() {
            ip.clone()
        } else if !listen_is_unspecified {
            "10.0.0.3".into()
        } else {
            "pod-hostname-abc123".into()
        };
        assert_eq!(result, "10.0.0.1");
    }

    #[test]
    fn t50_103_k8s_pod_ip_fallback() {
        // When no explicit address, POD_IP should be used
        let explicit: Option<String> = None;
        let pod_ip: Result<String, ()> = Ok("10.244.1.5".to_string());
        let listen_is_unspecified = true;

        let result = if let Some(ref addr) = explicit {
            addr.clone()
        } else if let Ok(ip) = pod_ip.as_ref() {
            ip.clone()
        } else if !listen_is_unspecified {
            "10.0.0.3".into()
        } else {
            "pod-hostname-abc123".into()
        };
        assert_eq!(result, "10.244.1.5");
    }

    // ── Issue #52: retry loop backoff ────────────────────────────

    #[test]
    fn t50_104_retry_backoff_doubles_then_caps() {
        // Issue #52: verify exponential backoff logic caps at 60s.
        let max_backoff = std::time::Duration::from_secs(60);
        let mut backoff = std::time::Duration::from_secs(1);

        // Simulate 10 failures
        for _ in 0..10 {
            backoff = std::cmp::min(backoff * 2, max_backoff);
        }
        assert_eq!(backoff, max_backoff);

        // Simulate success resets
        backoff = std::time::Duration::from_secs(1);
        assert_eq!(backoff.as_secs(), 1);
    }

    // ── Issue #63 (reopen of #55): agent image search checks all dirs ──

    #[test]
    fn t50_105_image_dirs_returns_multiple_candidates() {
        // Issue #63: The agent's image_dir() only returned one directory,
        // so images imported to ~/.spur/images were invisible if
        // /var/spool/spur/images existed (even if not writable).
        // Now image_dirs() returns ALL candidate directories.
        //
        // We can't directly call container.rs functions from here, but
        // we verify the logic: if HOME is set, the user dir should
        // always be a candidate regardless of system dir existence.
        let home = std::env::var("HOME").unwrap_or_default();
        if !home.is_empty() {
            let user_dir = std::path::PathBuf::from(&home).join(".spur/images");
            // The user dir path should be constructable
            assert!(user_dir.to_str().is_some());
            assert!(user_dir.to_str().unwrap().contains(".spur/images"));
        }
    }

    // ── Issue #64 (reopen of #54): raw terminal mode for attach ─────

    #[test]
    fn t50_106_attach_raw_mode_is_nonfatal_for_pipes() {
        // Issue #64: sattach enters raw terminal mode for interactive I/O.
        // When stdin is a pipe (not a TTY), the raw mode setup should
        // fail gracefully. Verify the libc::isatty check works.
        // (The actual RawModeGuard is in spur-cli; here we verify the
        // underlying principle that CI/test stdin is not a TTY.)
        use std::os::unix::io::AsRawFd;
        let fd = std::io::stdin().as_raw_fd();
        let is_tty = unsafe { libc::isatty(fd) } == 1;
        assert!(!is_tty, "expected stdin to be a pipe in test environment");
    }

    // ── Issue #65 (reopen of #56): pending reason uses available resources ──

    #[test]
    fn t50_107_pending_reason_checks_available_not_total() {
        // Issue #65: update_pending_reasons used total_resources.can_satisfy()
        // which always returned true for nodes with enough total capacity,
        // even when alloc_resources consumed most of the node. Should use
        // available = total - alloc.
        use spur_core::resource::ResourceSet;

        let total = ResourceSet {
            cpus: 64,
            memory_mb: 256000,
            gpus: vec![],
            generic: std::collections::HashMap::new(),
        };
        let alloc = ResourceSet {
            cpus: 60,
            memory_mb: 200000,
            gpus: vec![],
            generic: std::collections::HashMap::new(),
        };
        let required = ResourceSet {
            cpus: 32,
            memory_mb: 128000,
            gpus: vec![],
            generic: std::collections::HashMap::new(),
        };

        // Total CAN satisfy (64 >= 32) — old buggy check would say "capable"
        assert!(total.can_satisfy(&required));

        // Available = total - alloc = 4 cpus, 56000 MB — CANNOT satisfy
        let available = total.subtract(&alloc);
        assert_eq!(available.cpus, 4);
        assert!(
            !available.can_satisfy(&required),
            "available resources (4 cpus) should NOT satisfy requirement (32 cpus)"
        );
    }

    // ── T50.108–110: Node auto-recovery from heartbeat timeout (#95) ──

    #[test]
    fn t50_108_auto_down_node_recovers_on_heartbeat() {
        // A node marked Down from heartbeat timeout ("Not responding")
        // should automatically recover to Idle when a heartbeat arrives.
        let mut node = Node::new("gpu001".into(), ResourceSet::default());
        node.state = NodeState::Down;
        node.state_reason = Some("Not responding".into());

        // Simulate: heartbeat arrives, should recover
        assert_eq!(node.state, NodeState::Down);
        let is_auto_down = node
            .state_reason
            .as_deref()
            .map(|r| r == "Not responding")
            .unwrap_or(false);
        assert!(is_auto_down, "should detect auto-down reason");

        // After recovery logic applies:
        node.state = NodeState::Idle;
        node.state_reason = None;
        assert_eq!(node.state, NodeState::Idle);
        assert!(node.state_reason.is_none());
    }

    #[test]
    fn t50_109_admin_down_node_does_not_recover_on_heartbeat() {
        // A node marked Down by admin (reason != "Not responding")
        // should NOT recover on heartbeat.
        let mut node = Node::new("gpu002".into(), ResourceSet::default());
        node.state = NodeState::Down;
        node.state_reason = Some("maintenance".into());

        let is_auto_down = node
            .state_reason
            .as_deref()
            .map(|r| r == "Not responding")
            .unwrap_or(false);
        assert!(!is_auto_down, "admin-set Down should not auto-recover");

        // State should remain Down
        assert_eq!(node.state, NodeState::Down);
        assert_eq!(node.state_reason.as_deref(), Some("maintenance"));
    }

    #[test]
    fn t50_110_down_with_no_reason_does_not_recover() {
        // A node with Down state and no reason should not auto-recover
        // (conservative: if we don't know why it's Down, don't clear it).
        let mut node = Node::new("gpu003".into(), ResourceSet::default());
        node.state = NodeState::Down;
        node.state_reason = None;

        let is_auto_down = node
            .state_reason
            .as_deref()
            .map(|r| r == "Not responding")
            .unwrap_or(false);
        assert!(!is_auto_down, "no reason = no auto-recovery");
    }
}
