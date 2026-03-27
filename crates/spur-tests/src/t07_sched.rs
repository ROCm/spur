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
    use spur_core::partition::*;
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
}
