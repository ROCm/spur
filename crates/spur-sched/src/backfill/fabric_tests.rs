// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::traits::FabricSchedulingContext;
use spur_core::fabric::FabricView;
use spur_core::gpu_request::GpuRequest;
use spur_core::job::JobSpec;
use spur_core::node::NodeState;
use spur_core::partition::Partition;
use spur_core::resource::{AllocatedDevice, DeviceTopology, GpuLinkType, GpuResource};

fn now() -> chrono::DateTime<Utc> {
    chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap()
}

fn nodes(count: usize) -> Vec<Node> {
    (0..count)
        .map(|index| {
            let mut node = Node::new(
                format!("n{index}"),
                ResourceSet {
                    cpus: 8,
                    memory_mb: 1024,
                    generation: 7,
                    fabric_binding_version: 1,
                    gpus: (0..2)
                        .map(|local| {
                            let endpoint = (index % 2 * 2 + local).to_string();
                            GpuResource {
                                device_id: local as u32,
                                stable_id: local as u64 + 42,
                                gpu_type: "test".into(),
                                memory_mb: 100,
                                peer_gpus: vec![],
                                link_type: GpuLinkType::PCIe,
                                topology: Some(DeviceTopology {
                                    pci_bdf: Some(format!("000{}:04:00.0", local + 1)),
                                    fabric: Some(spur_core::fabric::FabricObservation {
                                        source: "test".into(),
                                        status: spur_core::fabric::ObservationStatus::Observed,
                                        physical_domain_id: Some(format!("rack-{}", index / 2)),
                                        fabric_domain_id: Some("7".into()),
                                        endpoint_id: Some(endpoint),
                                        physical_capacity: Some(8),
                                        fabric_capacity: Some(4),
                                        active_endpoints: (0..4).map(|id| id.to_string()).collect(),
                                        local_endpoints: (index % 2 * 2..index % 2 * 2 + 2)
                                            .map(|id| id.to_string())
                                            .collect(),
                                        ..Default::default()
                                    }),
                                    ..Default::default()
                                }),
                            }
                        })
                        .collect(),
                    ..Default::default()
                },
            );
            node.state = NodeState::Idle;
            node.partitions = vec!["rack".into()];
            node.last_heartbeat = Some(now());
            node
        })
        .collect()
}

fn job(id: u32, node_count: u32, gpus: u32) -> Job {
    Job::new(
        id,
        JobSpec {
            partition: Some("rack".into()),
            user: "test".into(),
            num_nodes: node_count,
            num_tasks: node_count,
            gpus: Some(GpuRequest::new(gpus, None)),
            time_limit: Some(Duration::seconds(30)),
            ..Default::default()
        },
    )
}

fn run(nodes: &[Node], jobs: &[Job]) -> (BackfillScheduler, Vec<Assignment>) {
    let partitions = [Partition {
        name: "rack".into(),
        is_default: true,
        nodes: "ALL".into(),
        ..Default::default()
    }];
    let view = FabricView::from_nodes("test", nodes, now(), 90);
    assert!(
        view.blocks.iter().all(|block| block.issues.is_empty()),
        "{:?}",
        view.blocks
    );
    let context = FabricSchedulingContext {
        view,
        single_block_partitions: vec!["rack".into()],
    };
    let busy: HashMap<_, _> = nodes
        .iter()
        .filter(|node| node.alloc_resources.has_devices())
        .map(|node| (node.name.clone(), now() + Duration::seconds(60)))
        .collect();
    let cluster = ClusterState {
        nodes,
        partitions: &partitions,
        reservations: &[],
        topology: None,
        fabric: Some(&context),
        busy_until: &busy,
    };
    let mut scheduler = BackfillScheduler::new(100);
    let assignments = scheduler.schedule_jobs(jobs, &cluster, now(), true);
    (scheduler, assignments)
}

#[test]
fn total_gpu_demand_uses_exact_members_from_one_block() {
    let nodes = nodes(4);
    let (_, assignments) = run(&nodes, &[job(1, 2, 3)]);
    assert_eq!(assignments.len(), 1);
    assert_eq!(assignments[0].nodes, ["n0", "n1"]);
    assert_eq!(
        assignments[0]
            .per_node_alloc
            .values()
            .map(|a| a.total_device_count("gpu"))
            .sum::<u64>(),
        3
    );
    for (node, allocation) in &assignments[0].per_node_alloc {
        let binding = allocation.fabric_binding.as_ref().unwrap();
        assert_eq!(binding.key.physical_domain, "rack-0");
        binding
            .validate(
                "test",
                node,
                &nodes
                    .iter()
                    .find(|n| n.name == *node)
                    .unwrap()
                    .total_resources,
                allocation,
            )
            .unwrap();
    }
}

#[test]
fn count_capacity_and_topology_hint_cannot_cross_blocks() {
    let mut request = job(1, 3, 4);
    request.spec.topology = Some("none".into());
    let (scheduler, assignments) = run(&nodes(4), &[request]);
    assert!(assignments.is_empty());
    assert_eq!(scheduler.fabric_unavailable_jobs(), [1]);
}

#[test]
fn multiple_blocks_on_a_node_do_not_merge_device_capacity() {
    let mut nodes = nodes(2);
    for node in &mut nodes {
        for (local, gpu) in node.total_resources.gpus.iter_mut().enumerate() {
            let observation = gpu.topology.as_mut().unwrap().fabric.as_mut().unwrap();
            observation.physical_domain_id = Some(format!("rack-{local}"));
            observation.active_endpoints = vec![local.to_string(), (local + 2).to_string()];
            observation.local_endpoints = vec![observation.endpoint_id.clone().unwrap()];
            observation.fabric_capacity = Some(2);
        }
    }
    assert!(run(&nodes, &[job(1, 1, 2)]).1.is_empty());
    let (_, assignments) = run(&nodes, &[job(2, 2, 2)]);
    assert_eq!(assignments.len(), 1);
    assert!(assignments[0]
        .per_node_alloc
        .values()
        .all(|a| a.device_ids("gpu") == [42]));
    let mut exclusive = job(3, 1, 1);
    exclusive.spec.exclusive = true;
    assert!(run(&nodes, &[exclusive]).1.is_empty());
}

#[test]
fn unsupported_agent_cannot_satisfy_mandatory_policy() {
    let mut nodes = nodes(2);
    nodes[0].total_resources.fabric_binding_version = 0;
    assert!(run(&nodes, &[job(1, 2, 2)]).1.is_empty());
}

#[test]
fn backfill_preserves_exact_future_devices_and_admits_only_a_short_gap() {
    let mut nodes = nodes(2);
    nodes[0].alloc_resources = ResourceAllocations {
        cpus: 1,
        devices: [(
            "gpu".into(),
            vec![
                AllocatedDevice::injectable(42),
                AllocatedDevice::injectable(43),
            ],
        )]
        .into(),
        ..Default::default()
    };
    nodes[0].state = NodeState::Mixed;
    let (scheduler, assignments) = run(&nodes, &[job(1, 2, 4), job(2, 1, 1)]);
    assert_eq!(
        assignments.iter().map(|a| a.job_id).collect::<Vec<_>>(),
        [2]
    );
    assert!(scheduler.timelines.iter().all(|t| t
        .intervals
        .iter()
        .any(|i| i.job_id == Some(1) && i.start == now() + Duration::seconds(60))));
    let mut long = job(3, 1, 1);
    long.spec.time_limit = Some(Duration::seconds(90));
    assert!(run(&nodes, &[job(1, 2, 4), long]).1.is_empty());
}

#[test]
fn required_nodelist_and_node_exclusivity_are_preserved() {
    let nodes = nodes(4);
    let mut request = job(1, 2, 2);
    request.spec.nodelist = Some("n0,n2".into());
    assert!(run(&nodes, &[request]).1.is_empty());
    let mut exclusive = job(2, 1, 1);
    exclusive.spec.exclusive = true;
    let (_, assignments) = run(&nodes, &[exclusive]);
    assert_eq!(assignments.len(), 1);
    let allocation = assignments[0].per_node_alloc.values().next().unwrap();
    assert_eq!(allocation.cpus, 8);
    assert_eq!(allocation.total_device_count("gpu"), 2);
}
