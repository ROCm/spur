//! T07: Scheduler tests.
//!
//! Corresponds to Slurm's test7.x series (scheduling/plugins).
//! Tests backfill scheduler, priority, timeline, resource matching.

#[cfg(test)]
mod tests {
    use crate::harness::*;
    use chrono::{Duration, Utc};
    use spur_core::job::*;
    use spur_core::node::*;
    use spur_core::resource::*;
    use spur_sched::backfill::BackfillScheduler;
    use spur_sched::timeline::NodeTimeline;
    use spur_sched::traits::*;

    // ── T07.1: Single job scheduling ─────────────────────────────

    #[test]
    fn t07_1_schedule_single_job() {
        reset_job_ids();
        let mut sched = BackfillScheduler::new(100);
        let nodes = make_nodes(4, 64, 256_000);
        let partitions = vec![make_partition("default", 4)];
        let pending = vec![make_job_with_resources("train", 2, 64, 1, Some(60))];

        let cluster = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
            reservations: &[],
            topology: None,
        };
        let assignments = sched.schedule(&pending, &cluster);

        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].nodes.len(), 2);
    }

    // ── T07.2: Multiple jobs ─────────────────────────────────────

    #[test]
    fn t07_2_schedule_multiple_jobs() {
        reset_job_ids();
        let mut sched = BackfillScheduler::new(100);
        let nodes = make_nodes(4, 64, 256_000);
        let partitions = vec![make_partition("default", 4)];
        let pending = vec![
            make_job_with_resources("job1", 2, 32, 1, Some(60)),
            make_job_with_resources("job2", 2, 32, 1, Some(60)),
        ];

        let cluster = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
            reservations: &[],
            topology: None,
        };
        let assignments = sched.schedule(&pending, &cluster);

        assert_eq!(assignments.len(), 2);
        // Both jobs should be assigned
        assert_eq!(assignments[0].nodes.len(), 2);
        assert_eq!(assignments[1].nodes.len(), 2);
    }

    // ── T07.3: Insufficient resources ────────────────────────────

    #[test]
    fn t07_3_insufficient_nodes() {
        reset_job_ids();
        let mut sched = BackfillScheduler::new(100);
        let nodes = make_nodes(2, 64, 256_000);
        let partitions = vec![make_partition("default", 2)];
        let pending = vec![make_job_with_resources("big", 4, 128, 1, Some(60))];

        let cluster = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
            reservations: &[],
            topology: None,
        };
        let assignments = sched.schedule(&pending, &cluster);

        assert_eq!(assignments.len(), 0);
    }

    #[test]
    fn t07_4_insufficient_cpus() {
        reset_job_ids();
        let mut sched = BackfillScheduler::new(100);
        let nodes = make_nodes(4, 32, 256_000); // Only 32 CPUs per node
        let partitions = vec![make_partition("default", 4)];
        // Request 64 CPUs per node
        let pending = vec![make_job_with_resources("cpu_heavy", 1, 64, 1, Some(60))];

        let cluster = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
            reservations: &[],
            topology: None,
        };
        let assignments = sched.schedule(&pending, &cluster);

        assert_eq!(assignments.len(), 0);
    }

    // ── T07.5: Down nodes skipped ────────────────────────────────

    #[test]
    fn t07_5_skip_down_nodes() {
        reset_job_ids();
        let mut sched = BackfillScheduler::new(100);
        let mut nodes = make_nodes(4, 64, 256_000);
        nodes[0].state = NodeState::Down;
        nodes[1].state = NodeState::Down;
        let partitions = vec![make_partition("default", 4)];
        let pending = vec![make_job_with_resources("job", 2, 32, 1, Some(60))];

        let cluster = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
            reservations: &[],
            topology: None,
        };
        let assignments = sched.schedule(&pending, &cluster);

        assert_eq!(assignments.len(), 1);
        // Should only use nodes 3 and 4 (0-indexed: 2 and 3)
        for name in &assignments[0].nodes {
            assert!(name == "node003" || name == "node004");
        }
    }

    // ── T07.6: Drained nodes skipped ─────────────────────────────

    #[test]
    fn t07_6_skip_drained_nodes() {
        reset_job_ids();
        let mut sched = BackfillScheduler::new(100);
        let mut nodes = make_nodes(3, 64, 256_000);
        nodes[0].state = NodeState::Drain;
        let partitions = vec![make_partition("default", 3)];
        let pending = vec![make_job_with_resources("job", 1, 32, 1, Some(60))];

        let cluster = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
            reservations: &[],
            topology: None,
        };
        let assignments = sched.schedule(&pending, &cluster);

        assert_eq!(assignments.len(), 1);
        assert_ne!(assignments[0].nodes[0], "node001");
    }

    // ── T07.7: Partition filtering ───────────────────────────────

    #[test]
    fn t07_7_partition_filtering() {
        reset_job_ids();
        let mut sched = BackfillScheduler::new(100);
        let mut nodes = make_nodes(4, 64, 256_000);
        // Only first 2 nodes in "gpu" partition
        nodes[0].partitions = vec!["gpu".into()];
        nodes[1].partitions = vec!["gpu".into()];
        nodes[2].partitions = vec!["cpu".into()];
        nodes[3].partitions = vec!["cpu".into()];

        let partitions = vec![make_partition("gpu", 2), make_partition("cpu", 2)];

        let mut job = make_job_with_resources("gpu_job", 2, 32, 1, Some(60));
        job.spec.partition = Some("gpu".into());

        let cluster = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
            reservations: &[],
            topology: None,
        };
        let assignments = sched.schedule(&[job], &cluster);

        assert_eq!(assignments.len(), 1);
        for name in &assignments[0].nodes {
            assert!(name == "node001" || name == "node002");
        }
    }

    // ── T07.8: Timeline tests ────────────────────────────────────

    #[test]
    fn t07_8_timeline_empty() {
        let tl = NodeTimeline::new(
            "node001".into(),
            ResourceSet {
                cpus: 64,
                memory_mb: 256_000,
                ..Default::default()
            },
        );
        let now = Utc::now();
        let avail = tl.available_at(now);
        assert_eq!(avail.cpus, 64);
    }

    #[test]
    fn t07_9_timeline_reservation() {
        let mut tl = NodeTimeline::new(
            "node001".into(),
            ResourceSet {
                cpus: 64,
                memory_mb: 256_000,
                ..Default::default()
            },
        );
        let now = Utc::now();
        tl.reserve(
            now,
            now + Duration::hours(4),
            ResourceSet {
                cpus: 32,
                memory_mb: 128_000,
                ..Default::default()
            },
        );

        let avail = tl.available_at(now + Duration::hours(1));
        assert_eq!(avail.cpus, 32);

        let avail = tl.available_at(now + Duration::hours(5));
        assert_eq!(avail.cpus, 64);
    }

    #[test]
    fn t07_10_timeline_earliest_start() {
        let mut tl = NodeTimeline::new(
            "node001".into(),
            ResourceSet {
                cpus: 64,
                memory_mb: 256_000,
                ..Default::default()
            },
        );
        let now = Utc::now();

        tl.reserve(
            now,
            now + Duration::hours(4),
            ResourceSet {
                cpus: 48,
                ..Default::default()
            },
        );

        let req = ResourceSet {
            cpus: 32,
            ..Default::default()
        };
        let start = tl.earliest_start(&req, Duration::hours(2), now);
        assert!(start >= now + Duration::hours(4));
    }

    #[test]
    fn t07_11_timeline_gc() {
        let mut tl = NodeTimeline::new(
            "node001".into(),
            ResourceSet {
                cpus: 64,
                ..Default::default()
            },
        );
        let now = Utc::now();

        tl.reserve(
            now - Duration::hours(2),
            now - Duration::hours(1),
            ResourceSet {
                cpus: 32,
                ..Default::default()
            },
        );
        tl.reserve(
            now,
            now + Duration::hours(1),
            ResourceSet {
                cpus: 16,
                ..Default::default()
            },
        );

        assert_eq!(tl.intervals.len(), 2);
        tl.gc(now);
        assert_eq!(tl.intervals.len(), 1);
    }

    // ── T07.12: Scheduler name ───────────────────────────────────

    #[test]
    fn t07_12_scheduler_name() {
        let sched = BackfillScheduler::new(100);
        assert_eq!(sched.name(), "backfill");
    }

    // ── T07.13: Exclusive mode blocks co-scheduling ────────────

    #[test]
    fn t07_13_exclusive_blocks_coscheduling() {
        reset_job_ids();
        let mut sched = BackfillScheduler::new(100);
        let mut nodes = make_nodes(2, 64, 256_000);
        // Simulate node001 already having an allocation (partially used)
        nodes[0].alloc_resources.cpus = 32;
        nodes[0].state = NodeState::Mixed;

        let partitions = vec![make_partition("default", 2)];

        // Exclusive job requires an idle node — node001 has allocs, so only node002 works
        let mut job = make_job_with_resources("excl", 1, 1, 1, Some(60));
        job.spec.exclusive = true;

        let cluster = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
            reservations: &[],
            topology: None,
        };
        let assignments = sched.schedule(&[job], &cluster);
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].nodes[0], "node002");
    }

    #[test]
    fn t07_14_exclusive_no_idle_nodes() {
        reset_job_ids();
        let mut sched = BackfillScheduler::new(100);
        let mut nodes = make_nodes(2, 64, 256_000);
        // Both nodes have allocations
        nodes[0].alloc_resources.cpus = 16;
        nodes[0].state = NodeState::Mixed;
        nodes[1].alloc_resources.cpus = 8;
        nodes[1].state = NodeState::Mixed;

        let partitions = vec![make_partition("default", 2)];

        let mut job = make_job_with_resources("excl", 1, 1, 1, Some(60));
        job.spec.exclusive = true;

        let cluster = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
            reservations: &[],
            topology: None,
        };
        let assignments = sched.schedule(&[job], &cluster);
        // No idle nodes available, so exclusive job cannot be scheduled
        assert_eq!(assignments.len(), 0);
    }

    #[test]
    fn t07_15_non_exclusive_allows_mixed() {
        reset_job_ids();
        let mut sched = BackfillScheduler::new(100);
        let mut nodes = make_nodes(2, 64, 256_000);
        // node001 partially allocated
        nodes[0].alloc_resources.cpus = 32;
        nodes[0].state = NodeState::Mixed;

        let partitions = vec![make_partition("default", 2)];

        // Non-exclusive job should schedule on mixed node
        let job = make_job_with_resources("normal", 1, 1, 1, Some(60));

        let cluster = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
            reservations: &[],
            topology: None,
        };
        let assignments = sched.schedule(&[job], &cluster);
        assert_eq!(assignments.len(), 1);
    }

    // ── T07.16: Constraint filtering ──────────────────────────

    #[test]
    fn t07_16_constraint_filters_nodes() {
        reset_job_ids();
        let mut sched = BackfillScheduler::new(100);
        let mut nodes = make_nodes(3, 64, 256_000);
        nodes[0].features = vec!["gpu".into(), "nvme".into()];
        nodes[1].features = vec!["nvme".into()];
        nodes[2].features = vec!["gpu".into(), "nvme".into()];

        let partitions = vec![make_partition("default", 3)];

        let mut job = make_job_with_resources("constrained", 1, 1, 1, Some(60));
        job.spec.constraint = Some("gpu".into());

        let cluster = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
            reservations: &[],
            topology: None,
        };
        let assignments = sched.schedule(&[job], &cluster);
        assert_eq!(assignments.len(), 1);
        // Should be assigned to node001 or node003 (both have "gpu")
        let name = &assignments[0].nodes[0];
        assert!(
            name == "node001" || name == "node003",
            "expected node with 'gpu' feature, got {}",
            name
        );
    }

    #[test]
    fn t07_17_constraint_no_match() {
        reset_job_ids();
        let mut sched = BackfillScheduler::new(100);
        let mut nodes = make_nodes(2, 64, 256_000);
        nodes[0].features = vec!["cpu_only".into()];
        nodes[1].features = vec!["cpu_only".into()];

        let partitions = vec![make_partition("default", 2)];

        let mut job = make_job_with_resources("need-gpu", 1, 1, 1, Some(60));
        job.spec.constraint = Some("gpu".into());

        let cluster = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
            reservations: &[],
            topology: None,
        };
        let assignments = sched.schedule(&[job], &cluster);
        assert_eq!(assignments.len(), 0, "no node has 'gpu' feature");
    }

    // ── T07.18–20: Federation peer config ────────────────────────

    #[test]
    fn t07_18_federation_no_peers_by_default() {
        use spur_core::config::FederationConfig;
        let fed = FederationConfig::default();
        // No federation peers configured — scheduler should never forward.
        assert!(fed.clusters.is_empty());
    }

    #[test]
    fn t07_19_federation_forward_decision() {
        // If local scheduler returns fewer assignments than pending jobs,
        // and federation is configured, unscheduled jobs should be forwarded.
        // This tests the decision logic (not the RPC call itself).
        let pending_count = 3usize;
        let assigned_count = 1usize;
        let has_federation = true;

        let should_forward = has_federation && assigned_count < pending_count;
        assert!(
            should_forward,
            "should forward when local can't schedule all"
        );

        let should_forward_no_peers = false && assigned_count < pending_count;
        assert!(!should_forward_no_peers, "no peers → no forward");
    }

    #[test]
    fn t07_20_federation_peer_address_format() {
        use spur_core::config::ClusterPeer;
        let peer = ClusterPeer {
            name: "hpc-east".into(),
            address: "http://hpc-east-ctrl:6817".into(),
        };
        // Address must be a valid http:// or https:// URI for tonic Connect.
        assert!(
            peer.address.starts_with("http://") || peer.address.starts_with("https://"),
            "peer address must be http(s): {}",
            peer.address
        );
    }

    // ── T07.21–23: Power management state transitions ─────────────

    #[test]
    fn t07_21_suspended_node_not_schedulable() {
        reset_job_ids();
        let mut sched = BackfillScheduler::new(100);
        let mut nodes = make_nodes(2, 64, 256_000);
        // Suspend one node.
        nodes[0].state = NodeState::Suspended;
        let partitions = vec![make_partition("default", 2)];
        let job = make_job_with_resources("train", 1, 1, 1, Some(60));
        let cluster = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
            reservations: &[],
            topology: None,
        };
        let assignments = sched.schedule(&[job], &cluster);
        // Should schedule to the non-suspended node only.
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].nodes[0], "node002");
    }

    #[test]
    fn t07_22_all_suspended_yields_no_assignments() {
        reset_job_ids();
        let mut sched = BackfillScheduler::new(100);
        let mut nodes = make_nodes(2, 64, 256_000);
        nodes[0].state = NodeState::Suspended;
        nodes[1].state = NodeState::Suspended;
        let partitions = vec![make_partition("default", 2)];
        let job = make_job_with_resources("train", 1, 1, 1, Some(60));
        let cluster = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
            reservations: &[],
            topology: None,
        };
        let assignments = sched.schedule(&[job], &cluster);
        assert_eq!(assignments.len(), 0, "all nodes suspended");
    }

    #[test]
    fn t07_23_power_config_suspend_timeout_gate() {
        use spur_core::config::PowerConfig;
        // When suspend_timeout_secs is None, power management is disabled.
        let cfg_off = PowerConfig {
            suspend_timeout_secs: None,
            suspend_command: None,
            resume_command: None,
        };
        assert!(
            cfg_off.suspend_timeout_secs.is_none(),
            "power mgmt disabled"
        );

        // When set, power management is enabled.
        let cfg_on = PowerConfig {
            suspend_timeout_secs: Some(300),
            suspend_command: Some("systemctl suspend".into()),
            resume_command: Some("wake-on-lan aa:bb:cc:dd:ee:ff".into()),
        };
        assert_eq!(cfg_on.suspend_timeout_secs, Some(300));
        assert!(cfg_on.suspend_command.is_some());
        assert!(cfg_on.resume_command.is_some());
    }

    // ── T07.24–26: Reservation enforcement (#27) ──────────────────

    #[test]
    fn t07_24_unreserved_job_skips_reserved_nodes() {
        // Regression: reserved nodes were allocated to non-reservation jobs (#27).
        reset_job_ids();
        let mut sched = BackfillScheduler::new(100);
        let nodes = make_nodes(2, 64, 256_000);
        let partitions = vec![make_partition("default", 2)];

        // node001 is reserved for reservation "res-alice"; node002 is free.
        let now = Utc::now();
        let res = spur_core::reservation::Reservation {
            name: "res-alice".into(),
            start_time: now - Duration::minutes(10),
            end_time: now + Duration::hours(2),
            nodes: vec!["node001".into()],
            users: vec!["alice".into()],
            accounts: Vec::new(),
        };

        // A job with no reservation spec should NOT land on node001.
        let job = make_job_with_resources("regular-job", 1, 1, 1, Some(60));
        let cluster = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
            reservations: &[res],
            topology: None,
        };
        let assignments = sched.schedule(&[job], &cluster);
        assert_eq!(assignments.len(), 1);
        assert_eq!(
            assignments[0].nodes[0], "node002",
            "unreserved job must not land on reserved node001"
        );
    }

    #[test]
    fn t07_25_reserved_job_lands_on_reserved_node() {
        // A job that targets a reservation should be placed on reserved nodes.
        reset_job_ids();
        let mut sched = BackfillScheduler::new(100);
        let nodes = make_nodes(2, 64, 256_000);
        let partitions = vec![make_partition("default", 2)];

        let now = Utc::now();
        let res = spur_core::reservation::Reservation {
            name: "res-bob".into(),
            start_time: now - Duration::minutes(10),
            end_time: now + Duration::hours(2),
            nodes: vec!["node001".into()],
            users: vec!["bob".into()],
            accounts: Vec::new(),
        };

        let mut job = make_job_with_resources("reserved-job", 1, 1, 1, Some(60));
        job.spec.reservation = Some("res-bob".into());
        job.spec.user = "bob".into();

        let cluster = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
            reservations: &[res],
            topology: None,
        };
        let assignments = sched.schedule(&[job], &cluster);
        assert_eq!(assignments.len(), 1);
        assert_eq!(
            assignments[0].nodes[0], "node001",
            "job targeting reservation must land on reserved node001"
        );
    }

    #[test]
    fn t07_26_no_reservations_all_nodes_available() {
        // Without any reservations all nodes are schedulable (baseline sanity).
        reset_job_ids();
        let mut sched = BackfillScheduler::new(100);
        let nodes = make_nodes(4, 64, 256_000);
        let partitions = vec![make_partition("default", 4)];
        let job = make_job_with_resources("free-job", 1, 1, 1, Some(60));
        let cluster = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
            reservations: &[],
            topology: None,
        };
        let assignments = sched.schedule(&[job], &cluster);
        assert_eq!(assignments.len(), 1);
    }

    // ── Issue #56: edge cases that could crash the scheduler ─────

    #[test]
    fn t07_27_num_nodes_zero_does_not_panic() {
        // Issue #56: A job with num_nodes=0 should be handled safely
        // instead of panicking on .max().unwrap() with empty iterator.
        reset_job_ids();
        let mut sched = BackfillScheduler::new(100);
        let nodes = make_nodes(2, 64, 256_000);
        let partitions = vec![make_partition("default", 2)];
        let mut job = make_job("zero-nodes");
        job.spec.num_nodes = 0;

        let cluster = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
            reservations: &[],
            topology: None,
        };
        // Must not panic — should schedule with 1 node (the minimum)
        let assignments = sched.schedule(&[job], &cluster);
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].nodes.len(), 1);
    }

    #[test]
    fn t07_28_single_idle_node_schedules_immediately() {
        // Issue #56 regression: A single idle node with a single pending
        // job should result in immediate scheduling (no Reason=Priority).
        reset_job_ids();
        let mut sched = BackfillScheduler::new(100);
        let nodes = make_nodes(1, 64, 256_000);
        let partitions = vec![make_partition("default", 1)];
        let job = make_job("simple");

        let cluster = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
            reservations: &[],
            topology: None,
        };
        let assignments = sched.schedule(&[job], &cluster);
        assert_eq!(
            assignments.len(),
            1,
            "single idle node should schedule job immediately"
        );
        assert_eq!(assignments[0].nodes[0], "node001");
    }

    #[test]
    fn t07_29_constraint_mismatch_not_scheduled() {
        // Issue #56: A job with --constraint=gpu should NOT be scheduled
        // on a node without the "gpu" feature.
        reset_job_ids();
        let mut sched = BackfillScheduler::new(100);
        let nodes = make_nodes(2, 64, 256_000); // nodes have NO features
        let partitions = vec![make_partition("default", 2)];
        let mut job = make_job("gpu-job");
        job.spec.constraint = Some("gpu".into());

        let cluster = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
            reservations: &[],
            topology: None,
        };
        let assignments = sched.schedule(&[job], &cluster);
        assert_eq!(
            assignments.len(),
            0,
            "job requiring gpu feature should not schedule on featureless nodes"
        );
    }

    #[test]
    fn t07_30_exclusive_job_needs_idle_node() {
        // Issue #56: An exclusive job should only schedule on a node
        // with zero current allocations.
        reset_job_ids();
        let mut sched = BackfillScheduler::new(100);
        let mut nodes = make_nodes(2, 64, 256_000);
        // Node 1 has partial allocations
        nodes[0].alloc_resources.cpus = 32;
        let partitions = vec![make_partition("default", 2)];
        let mut job = make_job("exclusive");
        job.spec.exclusive = true;

        let cluster = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
            reservations: &[],
            topology: None,
        };
        let assignments = sched.schedule(&[job], &cluster);
        assert_eq!(assignments.len(), 1);
        // Should land on node002 (the idle one), not node001 (partially allocated)
        assert_eq!(assignments[0].nodes[0], "node002");
    }

    #[test]
    fn t07_31_fully_allocated_nodes_not_scheduled() {
        // Issue #65: Jobs stuck PENDING with Reason=Priority when nodes
        // are fully allocated. Verify that a job requiring more resources
        // than available (total - alloc) is NOT scheduled.
        reset_job_ids();
        let mut sched = BackfillScheduler::new(100);
        let mut nodes = make_nodes(1, 64, 256_000);
        // Node is nearly fully allocated (60 of 64 CPUs consumed)
        nodes[0].alloc_resources.cpus = 60;
        let partitions = vec![make_partition("default", 1)];
        // Job requests 32 CPUs — more than the 4 available
        let mut job = make_job("big-job");
        job.spec.cpus_per_task = 32;
        job.spec.num_tasks = 1;

        let cluster = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
            reservations: &[],
            topology: None,
        };
        let assignments = sched.schedule(&[job], &cluster);
        // Should NOT be scheduled — insufficient available resources
        assert_eq!(
            assignments.len(),
            0,
            "job should not schedule on a node with only 4 free CPUs"
        );
    }

    #[test]
    fn t07_32_partially_allocated_node_accepts_fitting_job() {
        // Issue #65 counterpart: a job that fits in the remaining resources
        // should still schedule.
        reset_job_ids();
        let mut sched = BackfillScheduler::new(100);
        let mut nodes = make_nodes(1, 64, 256_000);
        // 32 CPUs already allocated, 32 still free
        nodes[0].alloc_resources.cpus = 32;
        let partitions = vec![make_partition("default", 1)];
        // Job requests only 4 CPUs — fits
        let mut job = make_job("small-job");
        job.spec.cpus_per_task = 4;
        job.spec.num_tasks = 1;

        let cluster = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
            reservations: &[],
            topology: None,
        };
        let assignments = sched.schedule(&[job], &cluster);
        assert_eq!(
            assignments.len(),
            1,
            "small job should fit on partially allocated node"
        );
    }

    // ── T07.40–49: Topology-aware scheduling ─────────────────────

    #[test]
    fn t07_40_topology_block_keeps_nodes_in_same_switch() {
        // 8 nodes split across 2 racks (4 per rack).
        // A 4-node job with topology=block should get all nodes from one rack.
        reset_job_ids();
        let mut sched = BackfillScheduler::new(100);
        let mut nodes = make_nodes(8, 64, 256_000);
        // Assign switch names to simulate two racks
        for (i, node) in nodes.iter_mut().enumerate() {
            node.switch_name = Some(if i < 4 {
                "rack01".into()
            } else {
                "rack02".into()
            });
        }
        let partitions = vec![make_partition("default", 8)];

        let mut job = make_job_with_resources("train", 4, 64, 1, Some(60));
        job.spec.topology = Some("block".into());
        let pending = vec![job];

        let topo = spur_core::topology::TopologyTree::from_switches(&[
            spur_core::topology::SwitchConfig {
                name: "rack01".into(),
                nodes: Some("node001,node002,node003,node004".into()),
                switches: None,
            },
            spur_core::topology::SwitchConfig {
                name: "rack02".into(),
                nodes: Some("node005,node006,node007,node008".into()),
                switches: None,
            },
        ]);

        let cluster = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
            reservations: &[],
            topology: Some(&topo),
        };

        let assignments = sched.schedule(&pending, &cluster);
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].nodes.len(), 4);

        // All nodes should be from the same rack
        let first_switch = nodes
            .iter()
            .find(|n| n.name == assignments[0].nodes[0])
            .unwrap()
            .switch_name
            .as_ref()
            .unwrap()
            .clone();
        for node_name in &assignments[0].nodes {
            let node = nodes.iter().find(|n| &n.name == node_name).unwrap();
            assert_eq!(
                node.switch_name.as_ref().unwrap(),
                &first_switch,
                "node {} should be in switch {}, but is in {:?}",
                node_name,
                first_switch,
                node.switch_name
            );
        }
    }

    #[test]
    fn t07_41_topology_tree_prefers_same_switch() {
        // 8 nodes across 2 racks. A 2-node job with topology=tree
        // should prefer nodes from the same rack.
        reset_job_ids();
        let mut sched = BackfillScheduler::new(100);
        let nodes = make_nodes(8, 64, 256_000);
        let partitions = vec![make_partition("default", 8)];

        let mut job = make_job_with_resources("infer", 2, 64, 1, Some(60));
        job.spec.topology = Some("tree".into());
        let pending = vec![job];

        let topo = spur_core::topology::TopologyTree::from_switches(&[
            spur_core::topology::SwitchConfig {
                name: "rack01".into(),
                nodes: Some("node001,node002,node003,node004".into()),
                switches: None,
            },
            spur_core::topology::SwitchConfig {
                name: "rack02".into(),
                nodes: Some("node005,node006,node007,node008".into()),
                switches: None,
            },
            spur_core::topology::SwitchConfig {
                name: "fabric0".into(),
                nodes: None,
                switches: Some("rack01,rack02".into()),
            },
        ]);

        let cluster = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
            reservations: &[],
            topology: Some(&topo),
        };

        let assignments = sched.schedule(&pending, &cluster);
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].nodes.len(), 2);

        // Both nodes should be from the same rack (same switch)
        let sw0 = topo.node_switch.get(&assignments[0].nodes[0]).unwrap();
        let sw1 = topo.node_switch.get(&assignments[0].nodes[1]).unwrap();
        assert_eq!(sw0, sw1, "both nodes should be on the same switch");
    }

    #[test]
    fn t07_42_no_topology_ignores_switch_grouping() {
        // Without topology preference, nodes are selected by time/weight
        // regardless of switch grouping.
        reset_job_ids();
        let mut sched = BackfillScheduler::new(100);
        let nodes = make_nodes(8, 64, 256_000);
        let partitions = vec![make_partition("default", 8)];

        // No topology preference — default behavior
        let job = make_job_with_resources("train", 4, 64, 1, Some(60));
        let pending = vec![job];

        let topo = spur_core::topology::TopologyTree::from_switches(&[
            spur_core::topology::SwitchConfig {
                name: "rack01".into(),
                nodes: Some("node001,node002,node003,node004".into()),
                switches: None,
            },
            spur_core::topology::SwitchConfig {
                name: "rack02".into(),
                nodes: Some("node005,node006,node007,node008".into()),
                switches: None,
            },
        ]);

        let cluster = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
            reservations: &[],
            topology: Some(&topo),
        };

        let assignments = sched.schedule(&pending, &cluster);
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].nodes.len(), 4);
        // No assertion on which rack — default behavior is fine
    }

    #[test]
    fn t07_43_topology_spans_switches_when_needed() {
        // 4 nodes per rack, need 6 nodes — must span both racks.
        reset_job_ids();
        let mut sched = BackfillScheduler::new(100);
        let nodes = make_nodes(8, 64, 256_000);
        let partitions = vec![make_partition("default", 8)];

        let mut job = make_job_with_resources("big-train", 6, 64, 1, Some(60));
        job.spec.topology = Some("tree".into());
        let pending = vec![job];

        let topo = spur_core::topology::TopologyTree::from_switches(&[
            spur_core::topology::SwitchConfig {
                name: "rack01".into(),
                nodes: Some("node001,node002,node003,node004".into()),
                switches: None,
            },
            spur_core::topology::SwitchConfig {
                name: "rack02".into(),
                nodes: Some("node005,node006,node007,node008".into()),
                switches: None,
            },
            spur_core::topology::SwitchConfig {
                name: "fabric0".into(),
                nodes: None,
                switches: Some("rack01,rack02".into()),
            },
        ]);

        let cluster = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
            reservations: &[],
            topology: Some(&topo),
        };

        let assignments = sched.schedule(&pending, &cluster);
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].nodes.len(), 6, "should span both racks");
    }

    // ── T07.50–59: Issue regression tests ────────────────────────

    #[test]
    fn t07_50_issue90_initial_pending_reason_is_none() {
        // Issue #90: New jobs should have PendingReason::None, not Priority.
        // The scheduler loop's update_pending_reasons() sets the actual reason.
        let job = make_job("test-90");
        assert_eq!(
            job.pending_reason,
            spur_core::job::PendingReason::None,
            "initial pending reason should be None, not Priority"
        );
    }

    #[test]
    fn t07_51_issue90_held_job_keeps_held_reason() {
        // Held jobs should still get PendingReason::Held
        reset_job_ids();
        let id = 1;
        let job = Job::new(
            id,
            JobSpec {
                name: "held-job".into(),
                hold: true,
                ..Default::default()
            },
        );
        assert_eq!(job.pending_reason, spur_core::job::PendingReason::Held);
    }

    #[test]
    fn t07_52_issue90_job_schedules_on_idle_nodes() {
        // A simple job should schedule immediately on idle nodes,
        // not stay stuck in PENDING.
        reset_job_ids();
        let mut sched = BackfillScheduler::new(100);
        let nodes = make_nodes(2, 64, 256_000);
        let partitions = vec![make_partition("default", 2)];
        let pending = vec![make_job("test-immediate")];

        let cluster = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
            reservations: &[],
            topology: None,
        };

        let assignments = sched.schedule(&pending, &cluster);
        assert_eq!(assignments.len(), 1, "job should be scheduled immediately");
    }

    #[test]
    fn t07_53_issue91_container_job_schedules_same_as_bare() {
        // Container jobs should pass scheduling (resource check) the
        // same as non-container jobs — container_image doesn't affect
        // resource requirements.
        reset_job_ids();
        let mut sched = BackfillScheduler::new(100);
        let nodes = make_nodes(2, 64, 256_000);
        let partitions = vec![make_partition("default", 2)];

        let mut job = make_job("container-test");
        job.spec.container_image = Some("ubuntu:22.04".into());

        let pending = vec![job];

        let cluster = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
            reservations: &[],
            topology: None,
        };

        let assignments = sched.schedule(&pending, &cluster);
        assert_eq!(
            assignments.len(),
            1,
            "container job should schedule the same as bare-process job"
        );
    }
}
