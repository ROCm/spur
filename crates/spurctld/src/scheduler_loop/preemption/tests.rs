// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;
use spur_core::gpu_request::GpuRequest;
use spur_core::job::JobSpec;
use spur_core::node::NodeState;
use spur_core::resource::{GpuLinkType, GpuResource, ResourceSet};

struct Fixture {
    nodes: Vec<Node>,
    holders: Vec<Job>,
    eligible: HashSet<JobId>,
    priorities: HashMap<JobId, i32>,
    usage: PartitionNodeUsage,
    handoffs: HashMap<String, Vec<JobId>>,
    partitions: Vec<spur_core::partition::Partition>,
    reservations: Vec<spur_core::reservation::Reservation>,
}

impl Fixture {
    fn new(cpus: u32) -> Self {
        let inventory = ResourceSet {
            cpus,
            memory_mb: 100,
            gpus: (0..4)
                .map(|index| GpuResource {
                    device_id: index,
                    stable_id: 1000 + u64::from(index),
                    gpu_type: if index < 2 { "a" } else { "b" }.into(),
                    memory_mb: 100,
                    peer_gpus: Vec::new(),
                    link_type: GpuLinkType::PCIe,
                    topology: None,
                })
                .collect(),
            ..Default::default()
        };
        let mut node = Node::new("n1".into(), inventory);
        node.state = NodeState::Idle;
        Self {
            nodes: vec![node],
            holders: Vec::new(),
            eligible: HashSet::new(),
            priorities: HashMap::new(),
            usage: HashMap::new(),
            handoffs: HashMap::new(),
            partitions: Vec::new(),
            reservations: Vec::new(),
        }
    }

    fn hold(&mut self, id: JobId, cpus: u32, memory: u64, gpus: &[u64], eligible: bool) {
        let mut allocation = ResourceAllocations::with_scalar(cpus, memory);
        allocation.add(&ResourceAllocations::from_device_ids("gpu", gpus));
        self.nodes[0].alloc_resources.add(&allocation);
        let mut job = Job::new(id, JobSpec::default());
        job.state = JobState::Running;
        job.allocated_nodes = vec!["n1".into()];
        job.per_node_alloc.insert("n1".into(), allocation);
        self.holders.push(job);
        if eligible {
            self.eligible.insert(id);
        }
    }

    fn select(&self, request: &Job, budget: &mut SearchBudget) -> Option<HashSet<JobId>> {
        let state = self.state();
        satisfiable_victim_set(
            request,
            &VictimPool {
                holders: &self.holders,
                evictable: &self.eligible,
                priority: &self.priorities,
                max_priority: None,
            },
            &state,
            DateTime::from_timestamp(1_800_000_000, 0).expect("fixture time"),
            budget,
        )
    }

    fn state(&self) -> ClusterState<'_> {
        static EMPTY: std::sync::LazyLock<HashMap<String, DateTime<Utc>>> =
            std::sync::LazyLock::new(HashMap::new);
        ClusterState {
            nodes: &self.nodes,
            partitions: &self.partitions,
            reservations: &self.reservations,
            topology: None,
            fabric: None,
            busy_until: &EMPTY,
            preemption_handoffs: &self.handoffs,
            partition_node_usage: &self.usage,
        }
    }
}

fn request(cpus: u32, memory: u64) -> Job {
    Job::new(
        999,
        JobSpec {
            cpus_per_task: cpus,
            memory_per_node_mb: Some(memory),
            time_limit: Some(chrono::Duration::minutes(10)),
            ..Default::default()
        },
    )
}

fn selected(fixture: &Fixture, request: &Job) -> Option<HashSet<JobId>> {
    fixture.select(request, &mut SearchBudget::default())
}

#[test]
fn exact_cpu_shortfall_spares_every_other_tenant() {
    for needed in [1, 2] {
        let mut f = Fixture::new(4);
        for id in 1..=4 {
            f.hold(id, 1, 0, &[], true);
        }
        assert_eq!(
            selected(&f, &request(needed, 0)),
            Some((1..=needed).collect())
        );
        assert_eq!(f.nodes[0].alloc_resources.cpus, 4);
    }
}

#[test]
fn unresponsive_nodes_do_not_justify_eviction() {
    for state in [NodeState::Idle, NodeState::Mixed, NodeState::Allocated] {
        let mut f = Fixture::new(4);
        f.hold(1, 4, 0, &[], true);
        f.nodes[0].state = state;
        f.nodes[0].not_responding = true;
        assert_eq!(selected(&f, &request(1, 0)), None, "{state:?}");

        f.nodes[0].not_responding = false;
        assert_eq!(selected(&f, &request(1, 0)), Some(HashSet::from([1])));
    }
}

#[test]
fn unresponsive_free_capacity_does_not_suppress_necessary_preemption() {
    let mut f = Fixture::new(4);
    let mut silent = f.nodes[0].clone();
    silent.name = "n2".into();
    silent.not_responding = true;
    f.nodes.push(silent);
    f.hold(1, 4, 0, &[], true);
    assert_eq!(selected(&f, &request(1, 0)), Some(HashSet::from([1])));

    f.nodes[1].not_responding = false;
    assert_eq!(selected(&f, &request(1, 0)), None);
}

#[test]
fn pinned_selector_keeps_its_budget_with_many_irrelevant_nodes() {
    for count in [64, 128, 512, 1024] {
        let mut f = Fixture::new(4);
        f.nodes[0].total_resources.gpus.clear();
        f.nodes[0].partitions = vec!["default".into()];
        f.nodes.extend((1..count).map(|index| {
            let mut node = Node::new(
                format!("node{index:04}"),
                ResourceSet {
                    cpus: 4,
                    memory_mb: 100,
                    ..Default::default()
                },
            );
            node.state = NodeState::Idle;
            node.partitions = vec!["default".into()];
            node
        }));
        f.partitions.push(spur_core::partition::Partition {
            name: "default".into(),
            is_default: true,
            ..Default::default()
        });
        f.hold(1, 4, 0, &[], true);
        let mut incoming = request(1, 0);
        incoming.spec.nodelist = Some("n1".into());
        incoming.spec.partition = Some("default".into());
        assert_eq!(
            selected(&f, &incoming),
            Some(HashSet::from([1])),
            "{count} nodes"
        );

        let mut freed = f.nodes.clone();
        freed[0].alloc_resources = ResourceAllocations::default();
        let state = ClusterState {
            nodes: &freed,
            ..f.state()
        };
        let mut budget = SearchBudget::default();
        let prepared = PlacementRequest::new(&incoming, &state, &mut budget).expect("request");
        let assignment = BackfillScheduler::new(1)
            .place_now(
                &prepared,
                &state,
                DateTime::from_timestamp(1_800_000_000, 0).expect("fixture time"),
                &mut budget,
            )
            .expect("trial budget")
            .expect("pinned placement");
        assert_eq!(assignment.nodes, ["n1"]);
        assert_eq!(assignment.per_node_alloc["n1"].cpus, 1);
    }
}

#[test]
fn feature_scoped_selector_keeps_its_budget_with_many_irrelevant_nodes() {
    let mut f = Fixture::new(4);
    f.nodes[0].features = vec!["selected".into()];
    f.hold(1, 4, 0, &[], true);
    for index in 1..512 {
        let mut node = f.nodes[0].clone();
        node.name = format!("node{index:04}");
        node.features.clear();
        node.alloc_resources = ResourceAllocations::default();
        f.nodes.push(node);
    }
    let mut incoming = request(1, 0);
    incoming.spec.constraint = Some("selected".into());
    assert_eq!(selected(&f, &incoming), Some(HashSet::from([1])));
    assert_eq!(f.nodes[0].alloc_resources.cpus, 4);
}

#[test]
fn protected_cpu_and_memory_remain_charged() {
    let mut f = Fixture::new(4);
    f.hold(1, 2, 60, &[], false);
    f.hold(2, 1, 20, &[], true);
    f.hold(3, 1, 20, &[], true);
    assert_eq!(selected(&f, &request(1, 20)), Some(HashSet::from([2])));
    assert_eq!(selected(&f, &request(1, 40)), Some(HashSet::from([2, 3])));
    assert_eq!(selected(&f, &request(1, 41)), None);
    assert_eq!(selected(&f, &request(3, 0)), None);
}

#[test]
fn pruning_removes_a_cheap_victim_that_did_not_close_the_memory_shortfall() {
    let mut f = Fixture::new(4);
    f.hold(1, 1, 50, &[], false);
    f.hold(2, 2, 0, &[], true);
    f.hold(3, 1, 50, &[], true);
    assert_eq!(selected(&f, &request(1, 50)), Some(HashSet::from([3])));
}

#[test]
fn typed_gpu_shortfall_releases_only_the_required_stable_identity() {
    let mut f = Fixture::new(4);
    f.hold(1, 1, 0, &[1000], false);
    f.hold(2, 1, 0, &[1001], true);
    f.hold(3, 1, 0, &[1002], true);
    f.hold(4, 1, 0, &[1003], true);
    let mut job = request(1, 0);
    job.spec.gpus_per_node = Some(GpuRequest::new(1, Some("a".into())));
    assert_eq!(selected(&f, &job), Some(HashSet::from([2])));
    job.spec.gpus_per_node = Some(GpuRequest::new(2, Some("a".into())));
    assert_eq!(selected(&f, &job), None);
}

#[test]
fn job_total_gpus_release_two_tenants_instead_of_all_four() {
    let mut f = Fixture::new(4);
    for id in 1..=4 {
        f.hold(id, 1, 0, &[999 + u64::from(id)], true);
    }
    let mut job = request(1, 0);
    job.spec.gpus = Some(GpuRequest::new(2, None));
    assert_eq!(selected(&f, &job), Some(HashSet::from([1, 2])));
}

#[test]
fn free_capacity_causes_no_eviction() {
    let mut f = Fixture::new(4);
    f.hold(1, 1, 0, &[], true);
    assert_eq!(selected(&f, &request(1, 0)), None);
}

#[test]
fn exclusive_request_cannot_displace_a_protected_or_memory_only_tenant() {
    let mut f = Fixture::new(4);
    f.hold(1, 0, 50, &[], false);
    f.hold(2, 4, 0, &[], true);
    let mut job = request(1, 0);
    job.spec.exclusive = true;
    assert_eq!(selected(&f, &job), None);
    f.eligible.insert(1);
    assert_eq!(selected(&f, &job), Some(HashSet::from([1, 2])));
}

#[test]
fn missing_slice_receives_no_release_credit() {
    let mut f = Fixture::new(4);
    f.hold(1, 4, 0, &[], true);
    f.holders[0].per_node_alloc.clear();
    assert_eq!(selected(&f, &request(1, 0)), None);
}

#[test]
fn completed_nodes_and_old_inventory_generations_receive_no_release_credit() {
    let mut f = Fixture::new(4);
    f.hold(1, 4, 0, &[], true);
    f.holders[0].node_completions.insert(
        "n1".into(),
        spur_core::job::NodeCompletion { code: 0, signal: 0 },
    );
    assert_eq!(selected(&f, &request(1, 0)), None);
    f.holders[0].node_completions.clear();
    f.holders[0]
        .per_node_alloc
        .get_mut("n1")
        .expect("slice")
        .generation = 10;
    assert_eq!(selected(&f, &request(1, 0)), None);
}

#[test]
fn inconsistent_ownership_and_unknown_residual_are_preserved() {
    let mut f = Fixture::new(4);
    f.hold(1, 2, 0, &[], true);
    f.hold(2, 2, 0, &[], false);
    f.nodes[0].alloc_resources.cpus = 3;
    assert_eq!(selected(&f, &request(3, 0)), None);
    f.holders.pop();
    f.nodes[0].alloc_resources.cpus = 4;
    assert_eq!(selected(&f, &request(3, 0)), None);
    assert_eq!(selected(&f, &request(2, 0)), Some(HashSet::from([1])));
}

#[test]
fn held_multinode_victim_releases_each_exact_slice() {
    let mut f = Fixture::new(4);
    f.hold(1, 3, 0, &[], true);
    f.hold(2, 1, 0, &[], false);
    let mut second = f.nodes[0].clone();
    second.name = "n2".into();
    second.alloc_resources.cpus = 2;
    f.nodes.push(second);
    f.holders[0].allocated_nodes.push("n2".into());
    f.holders[0]
        .per_node_alloc
        .insert("n2".into(), ResourceAllocations::with_scalar(2, 0));
    let mut job = request(1, 0);
    job.spec.num_nodes = 2;
    job.spec.num_tasks = 5;
    job.spec.nodelist = Some("n1,n2".into());
    assert_eq!(selected(&f, &job), Some(HashSet::from([1])));
}

#[test]
fn partition_caps_retain_protected_and_future_charges() {
    let mut f = Fixture::new(4);
    f.nodes[0].partitions = vec!["p".into()];
    f.partitions.push(spur_core::partition::Partition {
        name: "p".into(),
        max_tres_per_node: [("cpu".into(), 2)].into(),
        ..Default::default()
    });
    f.hold(1, 1, 0, &[], false);
    f.hold(2, 1, 0, &[], true);
    let until = DateTime::from_timestamp(1_800_001_000, 0).expect("fixture time");
    f.usage = HashMap::from([(
        "p".into(),
        HashMap::from([(
            "n1".into(),
            vec![
                RunningCharge {
                    job_id: 1,
                    tres: TresCharge::new(1, 0, 0, None),
                    until,
                },
                RunningCharge {
                    job_id: 2,
                    tres: TresCharge::new(1, 0, 0, None),
                    until,
                },
            ],
        )]),
    )]);
    let mut job = request(1, 0);
    job.spec.partition = Some("p".into());
    assert_eq!(selected(&f, &job), Some(HashSet::from([2])));
    job.spec.cpus_per_task = 2;
    assert_eq!(selected(&f, &job), None);
}

#[test]
fn another_preemptors_handoff_and_additive_required_node_block_eviction() {
    let mut f = Fixture::new(4);
    f.hold(1, 4, 0, &[], true);
    f.handoffs.insert("n1".into(), vec![100]);
    assert_eq!(selected(&f, &request(1, 0)), None);
    let mut job = request(1, 0);
    job.spec.num_nodes = 2;
    job.spec.nodelist = Some("n1".into());
    assert_eq!(selected(&f, &job), None);
}

#[test]
fn byte_work_and_call_limits_return_no_victims() {
    let mut f = Fixture::new(4);
    f.hold(1, 4, 0, &[], true);
    let mut budget = SearchBudget {
        calls_left: 3,
        work_left: 2_000_000,
    };
    assert_eq!(f.select(&request(1, 0), &mut budget), None);
    let mut budget = SearchBudget {
        calls_left: 256,
        work_left: 1,
    };
    assert_eq!(f.select(&request(1, 0), &mut budget), None);
    let mut job = request(1, 0);
    job.spec.constraint = Some("x".repeat(spur_sched::immediate::MAX_TEXT + 1));
    assert_eq!(selected(&f, &job), None);
    job.spec.constraint = None;
    job.spec.nodelist = Some("n[1-999999999]".into());
    assert_eq!(selected(&f, &job), None);
}

#[test]
fn pruning_exhaustion_discards_even_a_sufficient_unpruned_set() {
    let mut f = Fixture::new(64);
    for id in 1..=64 {
        f.hold(id, 1, 0, &[], true);
    }
    assert_eq!(selected(&f, &request(64, 0)), None);
    assert_eq!(f.nodes[0].alloc_resources.cpus, 64);
}

#[test]
fn a_cycle_budget_is_shared_across_requests() {
    let mut f = Fixture::new(4);
    f.hold(1, 4, 0, &[], true);
    let mut budget = SearchBudget {
        calls_left: 4,
        work_left: 2_000_000,
    };
    assert_eq!(
        f.select(&request(1, 0), &mut budget),
        Some(HashSet::from([1]))
    );
    assert_eq!(f.select(&request(1, 0), &mut budget), None);
}

#[test]
fn oversized_node_or_victim_domains_defer() {
    let mut f = Fixture::new(4);
    f.hold(1, 4, 0, &[], true);
    f.eligible.extend(1..=MAX_VICTIMS as u32 + 1);
    assert_eq!(selected(&f, &request(1, 0)), None);
    f.eligible = HashSet::from([1]);
    let mut job = request(1, 0);
    job.spec.num_nodes = MAX_NODES as u32 + 1;
    assert_eq!(selected(&f, &job), None);
}

#[test]
#[ignore = "manual selector timing and heap profile"]
fn victim_selection_profile() {
    for (label, node_count, per_node, needed, protected) in [
        ("one_of_four", 1u32, 4u32, 1u32, false),
        ("one_of_eight_nodes", 8, 1, 1, false),
        ("eight_nodes", 8, 1, 8, false),
        ("protected_sharing", 1, 2, 1, true),
        ("unplaceable", 1, 4, 2, false),
    ] {
        let mut nodes = Vec::new();
        let mut holders = Vec::new();
        let mut eligible = HashSet::new();
        for ni in 0..node_count {
            let name = format!("n{ni}");
            let mut node = Node::new(
                name.clone(),
                spur_core::resource::ResourceSet {
                    cpus: 4,
                    memory_mb: 100,
                    ..Default::default()
                },
            );
            node.state = spur_core::node::NodeState::Idle;
            for vi in 0..per_node {
                let id = ni * per_node + vi + 1;
                let slice =
                    ResourceAllocations::with_scalar(if protected { 1 } else { 4 / per_node }, 0);
                node.alloc_resources.add(&slice);
                let mut job = Job::new(id, spur_core::job::JobSpec::default());
                job.state = JobState::Running;
                job.allocated_nodes = vec![name.clone()];
                job.per_node_alloc.insert(name.clone(), slice);
                holders.push(job);
                eligible.insert(id);
            }
            if protected {
                let slice = ResourceAllocations::with_scalar(2, 0);
                node.alloc_resources.add(&slice);
                let mut job = Job::new(900, spur_core::job::JobSpec::default());
                job.state = JobState::Running;
                job.allocated_nodes = vec![name.clone()];
                job.per_node_alloc.insert(name.clone(), slice);
                holders.push(job);
            }
            nodes.push(node);
        }
        let incoming = Job::new(
            999,
            spur_core::job::JobSpec {
                num_nodes: needed,
                num_tasks: needed,
                time_limit: Some(chrono::Duration::minutes(10)),
                ..Default::default()
            },
        );
        let empty = HashMap::new();
        let state = ClusterState {
            nodes: &nodes,
            partitions: &[],
            reservations: &[],
            topology: None,
            fabric: None,
            busy_until: &empty,
            preemption_handoffs: &HashMap::new(),
            partition_node_usage: &HashMap::new(),
        };
        let now = DateTime::from_timestamp(1_800_000_000, 0).expect("fixture time");
        let priorities = HashMap::new();
        let pool = VictimPool {
            holders: &holders,
            evictable: &eligible,
            priority: &priorities,
            max_priority: None,
        };
        let mut times = Vec::new();
        let mut count = 0usize;
        for _ in 0..256 {
            let begin = std::time::Instant::now();
            let outcome =
                satisfiable_victim_set(&incoming, &pool, &state, now, &mut SearchBudget::default());
            times.push(begin.elapsed().as_nanos());
            count = std::hint::black_box(outcome).map_or(0, |ids| ids.len());
        }
        times.sort_unstable();
        eprintln!(
            "PROFILE {label} median_ns={} p95_ns={} victims={count}",
            times[128], times[243]
        );
    }
}

#[test]
fn overlap_handles_additive_ranges_and_exclusions() {
    let mut f = Fixture::new(4);
    let mut other = f.nodes[0].clone();
    other.name = "n2".into();
    f.nodes.push(other);
    f.hold(1, 1, 0, &[], true);
    f.holders[0].allocated_nodes = vec!["n2".into()];
    let mut incoming = request(1, 0);
    incoming.spec.nodelist = Some("n1".into());
    incoming.spec.num_nodes = 2;
    assert!(super::super::preempt_overlaps_pending_nodes(
        &incoming,
        &f.holders[0],
        &f.nodes
    ));
    incoming.spec.num_nodes = 1;
    assert!(!super::super::preempt_overlaps_pending_nodes(
        &incoming,
        &f.holders[0],
        &f.nodes
    ));
    incoming.spec.nodelist = Some("n[1-2]".into());
    assert!(super::super::preempt_overlaps_pending_nodes(
        &incoming,
        &f.holders[0],
        &f.nodes
    ));
    incoming.spec.exclude = Some("n[2-3]".into());
    assert!(!super::super::preempt_overlaps_pending_nodes(
        &incoming,
        &f.holders[0],
        &f.nodes
    ));
}

#[test]
fn prepared_overlap_reuses_ranges_and_honors_preferred_nodes_and_partitions() {
    let mut f = Fixture::new(4);
    f.nodes[0].partitions = vec!["p".into()];
    for (name, partition) in [("n2", "q"), ("n3", "r")] {
        let mut node = f.nodes[0].clone();
        node.name = name.into();
        node.partitions = vec![partition.into()];
        f.nodes.push(node);
    }
    let holder = |name: &str| {
        let mut job = Job::new(1, JobSpec::default());
        job.allocated_nodes = vec![name.into()];
        job
    };
    let prepare = |job: &Job| {
        PlacementRequest::new(job, &f.state(), &mut SearchBudget::default()).expect("scope")
    };
    let overlaps = |prepared: &PlacementRequest, name: &str| {
        prepared
            .scope()
            .overlaps(&holder(name), &mut SearchBudget::default())
            .expect("overlap")
    };

    let mut incoming = request(1, 0);
    incoming.preferred_nodes.insert("n1".into());
    let prepared = prepare(&incoming);
    assert!(overlaps(&prepared, "n1"));
    assert!(!overlaps(&prepared, "n2"));
    incoming.spec.num_nodes = 2;
    let prepared = prepare(&incoming);
    assert!(overlaps(&prepared, "n2"));
    incoming.spec.num_nodes = 1;
    incoming.spec.nodelist = Some("n2".into());
    let prepared = prepare(&incoming);
    assert!(!overlaps(&prepared, "n1"));
    assert!(overlaps(&prepared, "n2"));

    incoming.spec.nodelist = Some("n[1-1024]".into());
    incoming.spec.exclude = Some("n[3-1024]".into());
    incoming.spec.partition = Some(" p , q ".into());
    let prepared = prepare(&incoming);
    assert!(overlaps(&prepared, "n1"));
    assert!(overlaps(&prepared, "n2"));
    assert!(!overlaps(&prepared, "n3"));
    let mut overlap_budget = SearchBudget {
        calls_left: 0,
        work_left: 16,
    };
    assert_eq!(
        prepared
            .scope()
            .overlaps(&holder("n2"), &mut overlap_budget),
        Some(true)
    );
    assert_eq!(
        prepared
            .scope()
            .overlaps(&holder("n1"), &mut overlap_budget),
        Some(true)
    );
    incoming.spec.exclude = None;
    let prepared = prepare(&incoming);
    assert!(!overlaps(&prepared, "n3"));
    incoming.spec.nodelist = Some("n1".into());
    incoming.spec.num_nodes = 2;
    let prepared = prepare(&incoming);
    assert!(overlaps(&prepared, "n2"));
    assert!(!overlaps(&prepared, "n3"));
}

#[test]
fn immediate_trial_rechecks_changed_input_and_restores_scheduler_mode() {
    let f = Fixture::new(4);
    let incoming = request(1, 0);
    let mut budget = SearchBudget::default();
    let prepared =
        PlacementRequest::new(&incoming, &f.state(), &mut budget).expect("bounded request");
    let mut expanded = f.nodes.clone();
    expanded.resize(MAX_NODES + 1, f.nodes[0].clone());
    let changed = ClusterState {
        nodes: &expanded,
        ..f.state()
    };
    let mut scheduler = BackfillScheduler::new(1);
    let now = DateTime::from_timestamp(1_800_000_000, 0).expect("fixture time");
    assert!(scheduler
        .place_now(&prepared, &changed, now, &mut budget)
        .is_none());
    assert!(scheduler
        .place_now(&prepared, &f.state(), now, &mut budget)
        .expect("trial")
        .is_some());
    assert!(
        spur_sched::traits::Scheduler::schedule(&mut scheduler, &[incoming], &f.state()).len() == 1
    );
}

#[test]
fn snapshot_preserves_live_node_responsiveness() {
    use crate::cluster::{tests::set_last_heartbeat, MarkDownPolicy};
    use crate::raft::StateMachineApply;
    use spur_core::wal::WalOperation;

    let mut f = Fixture::new(4);
    let dir = tempfile::TempDir::new().expect("state directory");
    let cluster = ClusterManager::new(
        spur_core::config::SlurmConfig::load_from_str(
            "cluster_name = \"test\"\n[controller]\nhostname = \"test\"\n",
        )
        .expect("config"),
        dir.path(),
    )
    .expect("cluster");
    let register: WalOperation = serde_json::from_value(serde_json::json!({"NodeRegister": {
        "name": "n1", "resources": f.nodes[0].total_resources, "address": ""
    }}))
    .expect("registration");
    cluster.apply_operation(&register);
    let now = DateTime::from_timestamp(1_800_000_000, 0).expect("fixture time");
    set_last_heartbeat(&cluster, "n1", Some(now - chrono::Duration::seconds(200)));
    cluster.check_node_health_at(90, MarkDownPolicy::Only(HashSet::new()), now);

    let snapshot = Snapshot::capture(&cluster, &f.state(), &[], now, &mut SearchBudget::default())
        .expect("silent snapshot");
    assert!(snapshot.nodes[0].state.is_up());
    assert!(snapshot.nodes[0].not_responding);
    assert!(!snapshot.nodes[0].is_up_for_work());

    cluster.clear_not_responding();
    f.nodes[0].not_responding = true;
    let snapshot = Snapshot::capture(&cluster, &f.state(), &[], now, &mut SearchBudget::default())
        .expect("responding snapshot");
    assert!(!snapshot.nodes[0].not_responding);
    assert!(snapshot.nodes[0].is_up_for_work());
}

#[test]
fn snapshot_charges_typed_same_cycle_assignments_and_checks_reserved_slices() {
    use crate::raft::StateMachineApply;
    use spur_core::wal::WalOperation;
    let f = Fixture::new(4);
    let dir = tempfile::TempDir::new().expect("state directory");
    let cluster = ClusterManager::new(
        spur_core::config::SlurmConfig::load_from_str(
            "cluster_name = \"test\"\n[controller]\nhostname = \"test\"\n",
        )
        .expect("config"),
        dir.path(),
    )
    .expect("cluster");
    let apply = |value: serde_json::Value| {
        let op: WalOperation = serde_json::from_value(value).expect("operation");
        cluster.apply_operation(&op);
    };
    apply(
        serde_json::json!({"NodeRegister": {"name": "n1", "resources": f.nodes[0].total_resources, "address": ""}}),
    );
    let mut incoming = request(1, 0);
    incoming.spec.gpus_per_node = Some(GpuRequest::new(1, Some("a".into())));
    apply(serde_json::json!({"JobSubmit": {"job_id": 999, "spec": incoming.spec}}));
    let alloc = ResourceAllocations::from_device_ids("gpu", &[1000]);
    let mut assignment = Assignment {
        job_id: 999,
        nodes: vec!["n1".into()],
        per_node_alloc: HashMap::from([("n1".into(), alloc.clone())]),
        partition: Some("p".into()),
        memory_adjustment: None,
    };
    let now = DateTime::from_timestamp(1_800_000_000, 0).expect("time");
    let snapshot = Snapshot::capture(
        &cluster,
        &f.state(),
        &[assignment.clone()],
        now,
        &mut SearchBudget::default(),
    )
    .expect("snapshot");
    assert_eq!(
        snapshot.nodes[0].alloc_resources.total_device_count("gpu"),
        1
    );
    assert_eq!(snapshot.usage["p"]["n1"][0].tres.get("gres/gpu:a"), 1);
    apply(
        serde_json::json!({"JobStart": {"job_id": 999, "nodes": ["n1"], "resources": alloc, "per_node_alloc": assignment.per_node_alloc, "effective_partition": "p"}}),
    );
    let reserved = Snapshot::capture(
        &cluster,
        &f.state(),
        &[assignment.clone()],
        now,
        &mut SearchBudget::default(),
    )
    .expect("reserved snapshot");
    assert_eq!(
        reserved.nodes[0].alloc_resources.total_device_count("gpu"),
        1
    );
    assert_eq!(reserved.holders.len(), 1);
    assignment.per_node_alloc.get_mut("n1").expect("slice").cpus = 1;
    assert!(Snapshot::capture(
        &cluster,
        &f.state(),
        &[assignment],
        now,
        &mut SearchBudget::default()
    )
    .is_none());
}

#[test]
fn snapshot_preserves_suspension_timing_for_controller_grace() {
    use crate::raft::StateMachineApply;
    use spur_core::wal::WalOperation;

    let mut f = Fixture::new(4);
    f.nodes[0]
        .capabilities
        .push(spur_core::node::CAP_SIGNAL_JOB.into());
    let dir = tempfile::TempDir::new().expect("state directory");
    let cluster = ClusterManager::new(
        spur_core::config::SlurmConfig::load_from_str(
            "cluster_name = \"test\"\n[controller]\nhostname = \"test\"\n",
        )
        .expect("config"),
        dir.path(),
    )
    .expect("cluster");
    let apply = |value: serde_json::Value| {
        let op: WalOperation = serde_json::from_value(value).expect("operation");
        cluster.apply_operation(&op);
    };
    let now = DateTime::from_timestamp(1_800_000_000, 0).expect("fixture time");
    let start = now - chrono::Duration::seconds(900);
    let suspended_at = now - chrono::Duration::seconds(720);
    let resumed_at = now - chrono::Duration::seconds(120);
    apply(serde_json::json!({"NodeRegister": {
        "name": "n1", "resources": f.nodes[0].total_resources, "address": ""
    }}));
    apply(serde_json::json!({"JobSubmit": {"job_id": 1, "spec": request(1, 0).spec}}));
    let allocation = ResourceAllocations::with_scalar(1, 0);
    apply(serde_json::json!({"JobStart": {
        "job_id": 1, "nodes": ["n1"], "resources": allocation,
        "per_node_alloc": {"n1": allocation}
    }}));
    apply(serde_json::json!({"JobStateChange": {
        "job_id": 1, "old_state": JobState::Pending, "new_state": JobState::Running, "at": start
    }}));
    apply(serde_json::json!({"JobSuspend": {"job_id": 1, "at": suspended_at}}));
    let snapshot = Snapshot::capture(&cluster, &f.state(), &[], now, &mut SearchBudget::default())
        .expect("suspended snapshot");
    assert_eq!(snapshot.holders[0].suspended_at, Some(suspended_at));
    apply(serde_json::json!({"JobResume": {"job_id": 1, "at": resumed_at}}));
    let snapshot = Snapshot::capture(&cluster, &f.state(), &[], now, &mut SearchBudget::default())
        .expect("resumed snapshot");
    let holder = &snapshot.holders[0];
    assert_eq!(holder.suspended_at, None);
    assert_eq!(holder.suspended_secs, 600);
    assert_eq!(
        super::super::victim_grace_secs(
            holder,
            spur_core::partition::PreemptMode::Cancel,
            600,
            &f.nodes,
            now
        ),
        300
    );
}

#[test]
fn malformed_injectable_gpu_counts_defer_before_victim_selection() {
    for count in [0, 2, u64::from(u32::MAX) + 1] {
        let mut f = Fixture::new(4);
        f.nodes[0].total_resources.gpus.truncate(2);
        f.hold(1, 0, 0, &[1000], false);
        f.hold(2, 4, 0, &[], true);
        f.holders[0]
            .per_node_alloc
            .get_mut("n1")
            .expect("slice")
            .devices
            .get_mut("gpu")
            .expect("devices")[0]
            .count = count;
        f.nodes[0]
            .alloc_resources
            .devices
            .get_mut("gpu")
            .expect("devices")[0]
            .count = count;
        let mut incoming = request(1, 0);
        incoming.spec.gpus_per_node = Some(GpuRequest::new(2, None));
        let mut budget = SearchBudget::default();
        assert_eq!(f.select(&incoming, &mut budget), None);
        assert_eq!(budget.calls_left, SearchBudget::default().calls_left);
        assert_eq!(f.nodes[0].alloc_resources.cpus, 4);
        assert!(
            PlacementRequest::new(&incoming, &f.state(), &mut SearchBudget::default()).is_none()
        );
    }
}

#[test]
fn expanded_gpu_type_text_defers_before_placeholder_allocation() {
    let mut f = Fixture::new(4);
    f.hold(1, 4, 0, &[], true);
    for form in ["total", "per_node", "per_task", "gres"] {
        for long_type in [false, true] {
            let gpu_type = if long_type {
                "x".repeat(128 * 1024)
            } else {
                "a".into()
            };
            let mut incoming = request(1, 0);
            let gpu_request = GpuRequest::new(1000, Some(gpu_type.clone()));
            match form {
                "total" => incoming.spec.gpus = Some(gpu_request),
                "per_node" => incoming.spec.gpus_per_node = Some(gpu_request),
                "per_task" => incoming.spec.gpus_per_task = Some(gpu_request),
                "gres" => incoming.spec.gres = vec![format!("gpu:{gpu_type}:1000")],
                _ => unreachable!("fixture form"),
            }
            let mut budget = SearchBudget::default();
            assert_eq!(
                PlacementRequest::new(&incoming, &f.state(), &mut budget).is_none(),
                long_type,
                "{form}, long_type={long_type}"
            );
            assert_eq!(budget.calls_left, SearchBudget::default().calls_left);
            if long_type {
                assert_eq!(selected(&f, &incoming), None);
                assert_eq!(f.nodes[0].alloc_resources.cpus, 4);
            }
        }
    }
}

#[test]
fn duplicate_device_slices_and_huge_payload_metadata_are_safe() {
    let mut f = Fixture::new(4);
    f.hold(1, 4, 0, &[1000], true);
    let slice = f.holders[0].per_node_alloc.get_mut("n1").expect("slice");
    let duplicate = slice.devices["gpu"][0].clone();
    slice
        .devices
        .get_mut("gpu")
        .expect("devices")
        .push(duplicate);
    assert_eq!(selected(&f, &request(1, 0)), None);
    let mut incoming = request(1, 0);
    incoming.spec.script = Some("x".repeat(1_000_000));
    incoming
        .spec
        .environment
        .insert("LARGE".into(), "x".repeat(1_000_000));
    let prepared = PlacementRequest::new(
        &incoming,
        &Fixture::new(4).state(),
        &mut SearchBudget::default(),
    )
    .expect("payload is not placement input");
    assert!(prepared.job().spec.script.is_none());
    assert!(prepared.job().spec.environment.is_empty());
}

#[test]
fn cyclic_topology_defers_before_locality_search() {
    let f = Fixture::new(4);
    let topology = spur_core::topology::TopologyTree {
        switches: HashMap::from([(
            "s".into(),
            spur_core::topology::Switch {
                name: "s".into(),
                nodes: vec!["n1".into()],
                children: Vec::new(),
                parent: Some("s".into()),
                depth: 0,
            },
        )]),
        node_switch: HashMap::from([("n1".into(), "s".into())]),
    };
    let mut incoming = request(1, 0);
    incoming.spec.topology = Some("tree".into());
    let state = ClusterState {
        topology: Some(&topology),
        ..f.state()
    };
    assert!(PlacementRequest::new(&incoming, &state, &mut SearchBudget::default()).is_none());
}

#[test]
fn oversized_topology_parent_defers_before_ancestor_cloning() {
    let f = Fixture::new(4);
    let topology = spur_core::topology::TopologyTree {
        switches: HashMap::from([(
            "s".into(),
            spur_core::topology::Switch {
                name: "s".into(),
                nodes: vec!["n1".into()],
                children: Vec::new(),
                parent: Some("x".repeat(spur_sched::immediate::MAX_TEXT + 1)),
                depth: 0,
            },
        )]),
        node_switch: HashMap::from([("n1".into(), "s".into())]),
    };
    let mut incoming = request(1, 0);
    incoming.spec.topology = Some("tree".into());
    let state = ClusterState {
        topology: Some(&topology),
        ..f.state()
    };
    assert!(PlacementRequest::new(&incoming, &state, &mut SearchBudget::default()).is_none());
}

#[test]
fn overflowing_device_totals_defer_without_placement() {
    let mut f = Fixture::new(4);
    f.hold(1, 4, 0, &[1000, 1001], true);
    let slice = f.holders[0].per_node_alloc.get_mut("n1").expect("slice");
    for device in slice.devices.get_mut("gpu").expect("devices") {
        device.count = u64::MAX;
    }
    f.nodes[0].alloc_resources = slice.clone();
    assert_eq!(selected(&f, &request(1, 0)), None);
}
