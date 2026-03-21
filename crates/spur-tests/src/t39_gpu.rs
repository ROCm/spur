//! T39: GPU/GRES scheduling tests.
//!
//! Corresponds to Slurm's test39.x series.
//! Tests GRES parsing, GPU matching in scheduler, resource satisfaction.

#[cfg(test)]
mod tests {
    use spur_core::resource::*;

    // ── T39.1: GRES parsing ─────────────────────────────────────

    #[test]
    fn t39_1_parse_gpu_full() {
        let (name, gtype, count) = parse_gres("gpu:mi300x:8").unwrap();
        assert_eq!(name, "gpu");
        assert_eq!(gtype.unwrap(), "mi300x");
        assert_eq!(count, 8);
    }

    #[test]
    fn t39_2_parse_gpu_no_type() {
        let (name, gtype, count) = parse_gres("gpu:4").unwrap();
        assert_eq!(name, "gpu");
        assert!(gtype.is_none());
        assert_eq!(count, 4);
    }

    #[test]
    fn t39_3_parse_gpu_bare() {
        let (name, gtype, count) = parse_gres("gpu").unwrap();
        assert_eq!(name, "gpu");
        assert!(gtype.is_none());
        assert_eq!(count, 1);
    }

    // ── T39.4: GPU resource matching ─────────────────────────────

    #[test]
    fn t39_4_node_satisfies_gpu_request() {
        let node = ResourceSet {
            cpus: 128,
            memory_mb: 512_000,
            gpus: (0..8)
                .map(|i| GpuResource {
                    device_id: i,
                    gpu_type: "mi300x".into(),
                    memory_mb: 192_000,
                    peer_gpus: vec![],
                    link_type: GpuLinkType::XGMI,
                })
                .collect(),
            ..Default::default()
        };

        // Request 4 mi300x GPUs
        let req = ResourceSet {
            cpus: 64,
            memory_mb: 256_000,
            gpus: (0..4)
                .map(|i| GpuResource {
                    device_id: i,
                    gpu_type: "mi300x".into(),
                    memory_mb: 0,
                    peer_gpus: vec![],
                    link_type: GpuLinkType::PCIe,
                })
                .collect(),
            ..Default::default()
        };

        assert!(node.can_satisfy(&req));
    }

    #[test]
    fn t39_5_node_rejects_too_many_gpus() {
        let node = ResourceSet {
            cpus: 128,
            memory_mb: 512_000,
            gpus: (0..4)
                .map(|i| GpuResource {
                    device_id: i,
                    gpu_type: "mi300x".into(),
                    memory_mb: 192_000,
                    peer_gpus: vec![],
                    link_type: GpuLinkType::XGMI,
                })
                .collect(),
            ..Default::default()
        };

        // Request 8 GPUs but node only has 4
        let req = ResourceSet {
            cpus: 64,
            memory_mb: 256_000,
            gpus: (0..8)
                .map(|i| GpuResource {
                    device_id: i,
                    gpu_type: "mi300x".into(),
                    memory_mb: 0,
                    peer_gpus: vec![],
                    link_type: GpuLinkType::PCIe,
                })
                .collect(),
            ..Default::default()
        };

        assert!(!node.can_satisfy(&req));
    }

    #[test]
    fn t39_6_wrong_gpu_type_rejected() {
        let node = ResourceSet {
            cpus: 128,
            memory_mb: 512_000,
            gpus: (0..8)
                .map(|i| GpuResource {
                    device_id: i,
                    gpu_type: "mi300x".into(),
                    memory_mb: 192_000,
                    peer_gpus: vec![],
                    link_type: GpuLinkType::XGMI,
                })
                .collect(),
            ..Default::default()
        };

        // Request h100 GPUs on a mi300x node
        let req = ResourceSet {
            cpus: 64,
            memory_mb: 256_000,
            gpus: vec![GpuResource {
                device_id: 0,
                gpu_type: "h100".into(),
                memory_mb: 0,
                peer_gpus: vec![],
                link_type: GpuLinkType::PCIe,
            }],
            ..Default::default()
        };

        assert!(!node.can_satisfy(&req));
    }

    #[test]
    fn t39_7_any_gpu_type_matches() {
        let node = ResourceSet {
            cpus: 128,
            memory_mb: 512_000,
            gpus: (0..8)
                .map(|i| GpuResource {
                    device_id: i,
                    gpu_type: "mi300x".into(),
                    memory_mb: 192_000,
                    peer_gpus: vec![],
                    link_type: GpuLinkType::XGMI,
                })
                .collect(),
            ..Default::default()
        };

        // Request "any" GPU type
        let req = ResourceSet {
            cpus: 64,
            memory_mb: 256_000,
            gpus: (0..4)
                .map(|i| GpuResource {
                    device_id: i,
                    gpu_type: "any".into(),
                    memory_mb: 0,
                    peer_gpus: vec![],
                    link_type: GpuLinkType::PCIe,
                })
                .collect(),
            ..Default::default()
        };

        assert!(node.can_satisfy(&req));
    }

    // ── T39.8: GPU counts ────────────────────────────────────────

    // ── T39.9: Constraint scheduling ──────────────────────────────

    #[test]
    fn t39_9_constraint_filters_nodes_by_feature() {
        use chrono::Duration;
        use spur_core::job::{Job, JobSpec};
        use spur_core::node::{Node, NodeState};
        use spur_core::partition::Partition;
        use spur_sched::backfill::BackfillScheduler;
        use spur_sched::traits::{ClusterState, Scheduler};

        let mut sched = BackfillScheduler::new(100);

        let make_node = |name: &str, features: Vec<String>| -> Node {
            let mut node = Node::new(
                name.into(),
                ResourceSet {
                    cpus: 64,
                    memory_mb: 256_000,
                    ..Default::default()
                },
            );
            node.state = NodeState::Idle;
            node.partitions = vec!["default".into()];
            node.features = features;
            node
        };

        let nodes = vec![
            make_node("node001", vec!["mi300x".into(), "nvlink".into()]),
            make_node("node002", vec!["mi300x".into(), "nvlink".into()]),
            make_node("node003", vec!["h100".into()]),
            make_node("node004", vec!["h100".into()]),
        ];
        let partitions = vec![Partition {
            name: "default".into(),
            ..Default::default()
        }];

        let mut job = Job::new(
            1,
            JobSpec {
                name: "test".into(),
                partition: Some("default".into()),
                user: "test".into(),
                num_nodes: 1,
                num_tasks: 32,
                cpus_per_task: 1,
                constraint: Some("mi300x".into()),
                time_limit: Some(Duration::hours(1)),
                ..Default::default()
            },
        );

        let pending = vec![job];
        let cluster = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
        };
        let assignments = sched.schedule(&pending, &cluster);
        assert_eq!(assignments.len(), 1);
        assert!(
            assignments[0].nodes[0] == "node001" || assignments[0].nodes[0] == "node002",
            "expected mi300x node, got {}",
            assignments[0].nodes[0]
        );
    }

    #[test]
    fn t39_10_constraint_no_matching_nodes() {
        use chrono::Duration;
        use spur_core::job::{Job, JobSpec};
        use spur_core::node::{Node, NodeState};
        use spur_core::partition::Partition;
        use spur_sched::backfill::BackfillScheduler;
        use spur_sched::traits::{ClusterState, Scheduler};

        let mut sched = BackfillScheduler::new(100);

        let make_node = |name: &str, features: Vec<String>| -> Node {
            let mut node = Node::new(
                name.into(),
                ResourceSet {
                    cpus: 64,
                    memory_mb: 256_000,
                    ..Default::default()
                },
            );
            node.state = NodeState::Idle;
            node.partitions = vec!["default".into()];
            node.features = features;
            node
        };

        let nodes = vec![
            make_node("node001", vec!["h100".into()]),
            make_node("node002", vec!["h100".into()]),
        ];
        let partitions = vec![Partition {
            name: "default".into(),
            ..Default::default()
        }];

        let job = Job::new(
            1,
            JobSpec {
                name: "test".into(),
                partition: Some("default".into()),
                user: "test".into(),
                num_nodes: 1,
                num_tasks: 32,
                cpus_per_task: 1,
                constraint: Some("mi300x".into()),
                time_limit: Some(Duration::hours(1)),
                ..Default::default()
            },
        );

        let pending = vec![job];
        let cluster = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
        };
        let assignments = sched.schedule(&pending, &cluster);
        assert_eq!(assignments.len(), 0);
    }

    #[test]
    fn t39_11_constraint_multi_feature_requires_all() {
        use chrono::Duration;
        use spur_core::job::{Job, JobSpec};
        use spur_core::node::{Node, NodeState};
        use spur_core::partition::Partition;
        use spur_sched::backfill::BackfillScheduler;
        use spur_sched::traits::{ClusterState, Scheduler};

        let mut sched = BackfillScheduler::new(100);

        let make_node = |name: &str, features: Vec<String>| -> Node {
            let mut node = Node::new(
                name.into(),
                ResourceSet {
                    cpus: 64,
                    memory_mb: 256_000,
                    ..Default::default()
                },
            );
            node.state = NodeState::Idle;
            node.partitions = vec!["default".into()];
            node.features = features;
            node
        };

        let nodes = vec![
            make_node("node001", vec!["mi300x".into(), "nvlink".into()]),
            make_node("node002", vec!["mi300x".into()]), // missing nvlink
            make_node("node003", vec!["h100".into()]),
        ];
        let partitions = vec![Partition {
            name: "default".into(),
            ..Default::default()
        }];

        // Require both mi300x AND nvlink
        let job = Job::new(
            1,
            JobSpec {
                name: "test".into(),
                partition: Some("default".into()),
                user: "test".into(),
                num_nodes: 1,
                num_tasks: 32,
                cpus_per_task: 1,
                constraint: Some("mi300x,nvlink".into()),
                time_limit: Some(Duration::hours(1)),
                ..Default::default()
            },
        );

        let pending = vec![job];
        let cluster = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
        };
        let assignments = sched.schedule(&pending, &cluster);
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].nodes[0], "node001");
    }

    // ── T39.8: GPU counts ────────────────────────────────────────

    #[test]
    fn t39_8_gpu_count() {
        let r = ResourceSet {
            gpus: (0..8)
                .map(|i| GpuResource {
                    device_id: i,
                    gpu_type: "mi300x".into(),
                    memory_mb: 0,
                    peer_gpus: vec![],
                    link_type: GpuLinkType::XGMI,
                })
                .collect(),
            ..Default::default()
        };
        assert_eq!(r.total_gpus(), 8);
        let counts = r.gpu_counts();
        assert_eq!(counts.get("mi300x"), Some(&8));
    }
}
