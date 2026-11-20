// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

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
        let job = make_job("test");
        assert_job_state(&job, JobState::Pending);
    }

    #[test]
    fn t50_2_job_pending_to_running() {
        let mut job = make_job("test");
        assert_transition_ok(&mut job, JobState::Running);
        assert_job_state(&job, JobState::Running);
    }

    #[test]
    fn t50_3_job_running_to_completed() {
        let mut job = make_job("test");
        assert_transition_ok(&mut job, JobState::Running);
        assert_transition_ok(&mut job, JobState::Completed);
        assert!(job.state.is_terminal());
        assert!(job.end_time.is_some());
    }

    #[test]
    fn t50_4_job_running_to_failed() {
        let mut job = make_job("test");
        assert_transition_ok(&mut job, JobState::Running);
        assert_transition_ok(&mut job, JobState::Failed);
        assert!(job.state.is_terminal());
    }

    #[test]
    fn t50_5_job_pending_to_cancelled() {
        let mut job = make_job("test");
        assert_transition_ok(&mut job, JobState::Cancelled);
        assert!(job.state.is_terminal());
    }

    #[test]
    fn t50_6_job_running_to_timeout() {
        let mut job = make_job("test");
        assert_transition_ok(&mut job, JobState::Running);
        assert_transition_ok(&mut job, JobState::Timeout);
        assert!(job.state.is_terminal());
    }

    #[test]
    fn t50_7_job_running_to_node_fail() {
        let mut job = make_job("test");
        assert_transition_ok(&mut job, JobState::Running);
        assert_transition_ok(&mut job, JobState::NodeFail);
        assert!(job.state.is_terminal());
    }

    #[test]
    fn t50_8_job_running_to_preempted() {
        let mut job = make_job("test");
        assert_transition_ok(&mut job, JobState::Running);
        assert_transition_ok(&mut job, JobState::Preempted);
        assert_eq!(job.state, JobState::Preempted);
    }

    #[test]
    fn t50_9_job_running_to_suspended_and_back() {
        let mut job = make_job("test");
        assert_transition_ok(&mut job, JobState::Running);
        assert_transition_ok(&mut job, JobState::Suspended);
        assert!(job.state.is_active());
        assert_transition_ok(&mut job, JobState::Running);
    }

    #[test]
    fn t50_10_invalid_pending_to_completed() {
        let mut job = make_job("test");
        assert_transition_err(&mut job, JobState::Completed);
    }

    #[test]
    fn t50_11_invalid_completed_to_running() {
        let mut job = make_job("test");
        assert_transition_ok(&mut job, JobState::Running);
        assert_transition_ok(&mut job, JobState::Completed);
        assert_transition_err(&mut job, JobState::Running);
    }

    #[test]
    fn t50_12_invalid_pending_to_failed() {
        let mut job = make_job("test");
        assert_transition_err(&mut job, JobState::Failed);
    }

    // ── T50.15: Job path resolution ───────────────────────────────

    #[test]
    fn t50_15_path_resolve_job_id() {
        let mut job = make_job("train");
        job.job_id = 42;
        // Relative default is anchored to work_dir (/tmp), matching Slurm.
        assert_eq!(job.resolved_stdout(), "/tmp/spur-42.out");
    }

    #[test]
    fn t50_16_path_resolve_custom_pattern() {
        let mut job = make_job("train");
        job.job_id = 42;
        job.spec.user = "bob".into();
        job.spec.stdout_path = Some("output-%x-%u-%j.log".into());
        assert_eq!(job.resolved_stdout(), "/tmp/output-train-bob-42.log");
    }

    #[test]
    fn t50_17_path_resolve_node_pattern() {
        let mut job = make_job("test");
        job.job_id = 10;
        job.allocated_nodes = vec!["gpu001".into()];
        job.spec.stdout_path = Some("out-%N-%j.log".into());
        assert_eq!(job.resolved_stdout(), "/tmp/out-gpu001-10.log");
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
    fn t50_27_resource_alloc_subtract() {
        use spur_core::resource::ResourceAllocations;

        let total = ResourceSet {
            cpus: 64,
            memory_mb: 256_000,
            ..Default::default()
        };
        let used = ResourceAllocations::with_scalar(24, 100_000);
        let req = ResourceSet {
            cpus: 40,
            memory_mb: 156_000,
            ..Default::default()
        };
        assert!(total.can_satisfy_with_allocated(&used, &req));
        let too_much = ResourceSet {
            cpus: 41,
            memory_mb: 156_000,
            ..Default::default()
        };
        assert!(!total.can_satisfy_with_allocated(&used, &too_much));
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
        assert_eq!(job.priority, 0);
    }

    // ── T50.33–37: Requeue state transitions ───────────────────

    #[test]
    fn t50_33_requeue_from_timeout() {
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
        let mut job = make_job("requeue-nodefail");
        assert_transition_ok(&mut job, JobState::Running);
        assert_transition_ok(&mut job, JobState::NodeFail);
        // NodeFail → Pending should succeed
        assert_transition_ok(&mut job, JobState::Pending);
        assert_job_state(&job, JobState::Pending);
    }

    #[test]
    fn t50_36_requeue_from_failed() {
        let mut job = make_job("requeue-failed");
        assert_transition_ok(&mut job, JobState::Running);
        assert_transition_ok(&mut job, JobState::Failed);
        // Failed → Pending should succeed
        assert_transition_ok(&mut job, JobState::Pending);
        assert_job_state(&job, JobState::Pending);
    }

    #[test]
    fn t50_37_requeue_from_completed_is_guarded() {
        let mut job = make_job("requeue-completed");
        assert_transition_ok(&mut job, JobState::Running);
        assert_transition_ok(&mut job, JobState::Completed);
        // The general state machine must NOT resurrect a finished job; only the
        // guarded admin path (`Job::requeue_to_pending`) may.
        assert_transition_err(&mut job, JobState::Pending);
        assert_job_state(&job, JobState::Completed);
        job.requeue_to_pending()
            .expect("admin requeue re-pends a completed job");
        assert_job_state(&job, JobState::Pending);
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

    // ── T50.42–43: Cancelled re-pend is admin-guarded ─────────

    #[test]
    fn t50_42_requeue_from_cancelled_is_guarded() {
        let mut job = make_job("requeue-cancelled");
        assert_transition_ok(&mut job, JobState::Cancelled);
        // The general machine rejects Cancelled → Pending; only the guarded
        // admin requeue path re-pends it.
        assert_transition_err(&mut job, JobState::Pending);
        job.requeue_to_pending()
            .expect("admin requeue re-pends a cancelled job");
        assert_job_state(&job, JobState::Pending);
    }

    #[test]
    fn t50_43_node_available_states() {
        for state in &NodeState::ALL {
            let available = state.is_available();
            assert_eq!(
                available,
                matches!(state, NodeState::Idle | NodeState::Mixed),
                "{state:?} availability"
            );
        }
    }

    // ── T50.46: Interactive flag ────────────────────────────────

    #[test]
    fn t50_46_interactive_default_false() {
        let spec = spur_core::job::JobSpec::default();
        assert!(!spec.interactive);
    }

    // ── T50.47–48: MPI / distribution fields ────────────────────

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

    // ── T50.49b–50: MPI list and heterogeneous job fields ───────

    #[test]
    fn t50_49b_mpi_list_lines() {
        let lines = spur_core::mpi::mpi_list_lines("/usr/lib/spur");
        assert!(lines.iter().any(|l| l == "none"));
        assert!(lines.iter().any(|l| l == "pmix"));
        assert!(lines.iter().any(|l| l.contains("plugin_dir=/usr/lib/spur")));
    }

    #[test]
    fn t50_50_het_job_fields_default_none() {
        let job = make_job("het-test");
        assert!(job.het_job_id.is_none());
        assert!(job.het_group.is_none());
    }

    // ── T50.53–55: Step constants and state transitions ────────

    #[test]
    fn t50_53_step_batch_constant() {
        assert_eq!(spur_core::step::STEP_BATCH, 0xFFFF_FFFB);
        assert_eq!(spur_core::step::STEP_EXTERN, 0xFFFF_FFFC);
        assert_eq!(spur_core::step::STEP_INTERACTIVE, 0xFFFF_FFFA);
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
        assert!(StepState::OutOfMemory.is_terminal());
    }

    #[test]
    fn t50_56_step_state_display() {
        use spur_core::step::StepState;
        assert_eq!(StepState::Running.display(), "RUNNING");
        assert_eq!(StepState::Completed.display(), "COMPLETED");
        assert_eq!(StepState::Failed.display(), "FAILED");
        assert_eq!(StepState::Pending.display(), "PENDING");
        assert_eq!(StepState::Cancelled.display(), "CANCELLED");
        assert_eq!(StepState::OutOfMemory.display(), "OUT_OF_MEMORY");
    }

    // ── T50.58: Burst buffer field ────────────────────────────────

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

    // ── T50.66, T50.68–70: Begin time, deadline, spread_job, open_mode fields ──

    #[test]
    fn t50_66_spread_job_default_false() {
        assert!(!JobSpec::default().spread_job);
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

    // ── T50.72: Reservation field on JobSpec ────────────────────

    #[test]
    fn t50_72_reservation_default_none() {
        assert!(JobSpec::default().reservation.is_none());
    }

    // ── T50.75: Node count single-node partition ─────────────────

    #[test]
    fn t50_75_single_node_partition_count_nonzero() {
        // Regression: node count showed 0 on single-node setups.
        // A partition with one node in its spec must report count >= 1.
        let nodes_spec = "node001";
        let count = spur_core::hostlist::expand(nodes_spec)
            .map(|v| v.len())
            .unwrap_or(0);
        assert_eq!(count, 1, "single node spec must expand to exactly 1 node");
        assert!(count > 0, "node count must be non-zero");
    }

    #[test]
    fn t50_78_single_node_name_matches_hostlist() {
        // Regression: single-node clusters had inconsistent node naming.
        // The node name used at registration must be what the hostlist expands to.
        let config_spec = "ubb-r09-01";
        let expanded = spur_core::hostlist::expand(config_spec).unwrap();
        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0], "ubb-r09-01");
    }

    // ── T50.82 and T50.85: Federation config parsing ─────────────

    #[test]
    fn t50_82_federation_config_default_empty() {
        use spur_core::config::FederationConfig;
        let fed = FederationConfig::default();
        assert!(fed.clusters.is_empty());
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
        let cfg = SlurmConfig::load_from_str(toml).unwrap();
        assert_eq!(cfg.federation.clusters.len(), 1);
        assert_eq!(cfg.federation.clusters[0].name, "peer-a");
    }

    // ── T50.90: Pending reason when no nodes ─────────────────────

    #[test]
    fn t50_90_pending_reason_resources_when_no_nodes() {
        // pending_reason should reflect inability to schedule, not
        // just always show "Priority".
        let reason = spur_core::job::PendingReason::Resources;
        assert_eq!(reason.display(), "Resources");

        let reason = spur_core::job::PendingReason::NodeDown;
        assert_eq!(reason.display(), "NodeDown");

        let reason = spur_core::job::PendingReason::Priority;
        assert_eq!(reason.display(), "Priority");
    }

    // ── CLI reads config file for controller port ─────────────────

    #[test]
    fn t50_93_config_controller_addr_from_toml() {
        // The CLI should read controller address from config.
        // Verify SlurmConfig can parse a custom port and hosts.
        let toml = r#"
            cluster_name = "test"
            [controller]
            listen_addr = "[::]:6821"
            hosts = ["ctrl.example.com"]
        "#;
        let config = spur_core::config::SlurmConfig::load_from_str(toml).unwrap();
        assert_eq!(config.controller.listen_addr, "[::]:6821");
        assert_eq!(config.controller.hosts[0], "ctrl.example.com");
    }

    // ── Scheduler crash recovery ──────────────────────────────────

    #[test]
    fn t50_96_scheduler_handles_num_nodes_zero_safely() {
        // If num_nodes is somehow 0, the scheduler should not
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
            fabric: None,
            preemption_handoffs: &std::collections::HashMap::new(),
            busy_until: &std::collections::HashMap::new(),
            partition_node_usage: &Default::default(),
            nodes: &nodes,
            partitions: &partitions,
            reservations: &[],
            topology: None,
            script_placement_default: Default::default(),
        };
        // This should NOT panic
        let assignments = sched.schedule(&[job], &cluster);
        // With num_nodes.max(1), the job should still get scheduled
        assert_eq!(assignments.len(), 1);
    }

    // ── Pending reason uses available resources ───────────────────

    #[test]
    fn t50_107_pending_reason_checks_available_not_total() {
        // update_pending_reasons used total_resources.can_satisfy()
        // which always returned true for nodes with enough total capacity,
        // even when alloc_resources consumed most of the node. Should use
        // available = total - alloc.
        use spur_core::resource::{ResourceAllocations, ResourceSet};

        let total = ResourceSet {
            fabric_binding_version: 0,
            block_file_revision: None,
            cpus: 64,
            memory_mb: 256000,
            gpus: vec![],
            generic: std::collections::HashMap::new(),
            generation: 0,
            topology: None,
        };
        let alloc = ResourceAllocations::with_scalar(60, 200000);
        let required = ResourceSet {
            fabric_binding_version: 0,
            block_file_revision: None,
            cpus: 32,
            memory_mb: 128000,
            gpus: vec![],
            generic: std::collections::HashMap::new(),
            generation: 0,
            topology: None,
        };

        assert!(total.can_satisfy(&required));

        assert!(
            !total.can_satisfy_with_allocated(&alloc, &required),
            "available resources (4 cpus) should NOT satisfy requirement (32 cpus)"
        );
    }
}
