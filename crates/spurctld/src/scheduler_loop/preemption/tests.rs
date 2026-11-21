// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;
use spur_core::gpu_request::GpuRequest;
use spur_core::job::JobSpec;
use spur_core::node::NodeState;
use spur_core::resource::{GpuLinkType, GpuResource, ResourceSet};
use spur_sched::immediate::PlacementRequest;

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
                    numa_node: None,
                    affinity_cpus: Vec::new(),
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
        self.hold_on(0, id, allocation, eligible);
    }

    fn hold_on(
        &mut self,
        index: usize,
        id: JobId,
        allocation: ResourceAllocations,
        eligible: bool,
    ) {
        let node = &mut self.nodes[index];
        node.hold(&allocation);
        let mut job = Job::new(id, JobSpec::default());
        job.state = JobState::Running;
        job.allocated_nodes = vec![node.name.clone()];
        job.per_node_alloc.insert(node.name.clone(), allocation);
        self.holders.push(job);
        if eligible {
            self.eligible.insert(id);
        }
    }

    fn select(&self, request: &Job, budget: &mut SearchBudget) -> Option<HashSet<JobId>> {
        self.select_with_order(request, budget, None)
    }

    fn select_with_order(
        &self,
        request: &Job,
        budget: &mut SearchBudget,
        order: Option<VictimOrder<'_>>,
    ) -> Option<HashSet<JobId>> {
        let state = self.state();
        satisfiable_victim_set(
            request,
            &VictimPool {
                holders: &self.holders,
                evictable: &self.eligible,
                priority: &self.priorities,
                max_priority: None,
                order,
            },
            &state,
            DateTime::from_timestamp(1_800_000_000, 0).expect("fixture time"),
            budget,
        )
        .ok()
        .flatten()
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

fn reserved_cpu_fixture() -> Fixture {
    use spur_core::core_layout::{CoreLayout, GpuReservedCpus};
    let mut f = Fixture::new(12);
    f.nodes[0].core_layout = Some(CoreLayout {
        gpu_reserved: (0..4)
            .map(|i| GpuReservedCpus {
                stable_id: 1000 + u64::from(i),
                cpus: vec![2 * i, 2 * i + 1],
            })
            .collect(),
        general_cpus: vec![8, 9, 10, 11],
        ..Default::default()
    });
    f.nodes[0].sync_reserved_tag();
    f
}

fn reserved_gpu_slice(f: &Fixture, gpu: u64) -> ResourceAllocations {
    let mut alloc = ResourceAllocations::with_scalar(2, 1);
    alloc.add(&ResourceAllocations::from_device_ids("gpu", &[gpu]));
    f.nodes[0]
        .core_layout
        .as_ref()
        .expect("layout")
        .stamp(&mut alloc);
    alloc
}

#[test]
fn per_gpu_cpu_requirement_survives_trial_preparation() {
    let mut f = Fixture::new(8);
    f.hold(1, 4, 1, &[], false);
    f.hold(2, 2, 1, &[], true);
    f.hold(3, 2, 1, &[], true);
    let mut incoming = request(1, 1);
    incoming.spec.gpus_per_node = Some(GpuRequest {
        count: 1,
        gpu_type: None,
    });
    incoming.spec.cpus_per_gpu = Some(4);
    assert_eq!(selected(&f, &incoming), Some(HashSet::from([2, 3])));
}

#[test]
fn victim_selection_frees_general_cpus_instead_of_reserved_cpus() {
    let mut f = reserved_cpu_fixture();
    f.hold_on(0, 1, reserved_gpu_slice(&f, 1000), true);
    f.hold(2, 2, 1, &[], true);
    f.hold(3, 2, 1, &[], false);
    assert_eq!(selected(&f, &request(2, 1)), Some(HashSet::from([2])));
}

#[test]
fn protected_gpu_holder_keeps_its_reserved_cpu_charge() {
    let mut f = reserved_cpu_fixture();
    f.hold_on(0, 1, reserved_gpu_slice(&f, 1000), false);
    f.hold(2, 4, 1, &[], true);
    assert_eq!(selected(&f, &request(4, 1)), Some(HashSet::from([2])));
}

#[test]
fn old_layout_victim_does_not_release_another_holders_reserved_charge() {
    let mut f = reserved_cpu_fixture();
    let mut old = reserved_gpu_slice(&f, 1000);
    old.layout_fingerprint ^= 1;
    f.hold_on(0, 1, old, true);
    f.hold_on(0, 2, reserved_gpu_slice(&f, 1001), false);
    f.hold(3, 2, 1, &[], false);
    assert_eq!(selected(&f, &request(2, 1)), Some(HashSet::from([1])));
    f.nodes[0].partitions = vec!["default".into()];
    f.partitions.push(spur_core::partition::Partition {
        name: "default".into(),
        def_mem_per_node: Some(1),
        ..Default::default()
    });
    assert_eq!(selected(&f, &request(2, 1)), Some(HashSet::from([1])));
}

#[test]
fn projection_reset_restores_reserved_cpu_accounting() {
    let mut f = reserved_cpu_fixture();
    let slice = reserved_gpu_slice(&f, 1000);
    f.hold_on(0, 1, slice.clone(), true);
    let incoming = request(4, 1);
    let mut budget = SearchBudget::default();
    let mut projection =
        PlacementProjection::new(&incoming, &f.state(), &mut budget, InputSize::default())
            .expect("projection");
    projection.release("n1", &slice);
    projection.reset();
    let now = DateTime::from_timestamp(1_800_000_000, 0).expect("fixture time");
    assert!(BackfillScheduler::new(1)
        .place_releasing(&projection, now, &mut budget)
        .expect("trial")
        .is_some());
}

#[test]
fn reused_trial_timelines_refresh_cpu_pools() {
    let mut f = Fixture::new(12);
    f.hold(1, 4, 1, &[], false);
    let mut reserved = reserved_cpu_fixture();
    reserved.hold(1, 4, 1, &[], false);
    let incoming = request(2, 1);
    let now = DateTime::from_timestamp(1_800_000_000, 0).expect("fixture time");
    let mut budget = SearchBudget::default();
    let mut scheduler = BackfillScheduler::new(1);
    for (fixture, fits) in [(&f, true), (&reserved, false), (&f, true)] {
        let prepared = PlacementRequest::new(&incoming, &fixture.state(), &mut budget)
            .expect("prepared request");
        assert_eq!(
            scheduler
                .place_now(&prepared, &fixture.state(), now, &mut budget)
                .expect("trial")
                .is_some(),
            fits,
        );
    }
}

#[test]
fn trial_preserves_gpus_blocked_by_an_older_layout() {
    let mut f = reserved_cpu_fixture();
    f.nodes[0].layout_blocked_cpus = (1000..1004).map(|id| (id, 1)).collect();
    f.hold(1, 4, 100, &[], true);
    let mut incoming = request(1, 1);
    incoming.spec.gpus_per_node = Some(GpuRequest {
        count: 1,
        gpu_type: None,
    });
    assert_eq!(selected(&f, &incoming), None);
}

#[test]
fn oversized_core_layout_defers_before_trial_copying() {
    let mut f = reserved_cpu_fixture();
    f.nodes[0]
        .core_layout
        .as_mut()
        .expect("layout")
        .general_cpus = vec![0; spur_sched::immediate::MAX_ENTRIES + 1];
    assert!(
        PlacementRequest::new(&request(1, 1), &f.state(), &mut SearchBudget::default()).is_none()
    );
}

#[test]
fn blocked_gpu_seeding_is_bounded_outside_the_request_scope() {
    let mut f = Fixture::new(4);
    let mut other = f.nodes[0].clone();
    other.name = "other".into();
    let gpu = other.total_resources.gpus[0].clone();
    other.total_resources.gpus = (0..128)
        .map(|id| GpuResource {
            device_id: id,
            stable_id: u64::from(id),
            ..gpu.clone()
        })
        .collect();
    other.layout_blocked_cpus = (0..128).map(|id| (id, 1)).collect();
    f.nodes.push(other);
    let mut incoming = request(1, 1);
    incoming.spec.nodelist = Some("n1".into());
    let mut budget = SearchBudget {
        work_left: 10_000,
        ..Default::default()
    };
    let prepared =
        PlacementRequest::new(&incoming, &f.state(), &mut budget).expect("bounded inputs");
    let now = DateTime::from_timestamp(1_800_000_000, 0).expect("fixture time");
    assert!(BackfillScheduler::new(1)
        .place_now(&prepared, &f.state(), now, &mut budget)
        .is_none());
    assert!(BackfillScheduler::new(1)
        .place_now(&prepared, &f.state(), now, &mut SearchBudget::default())
        .expect("sufficient budget")
        .is_some());
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
fn unpinned_selector_keeps_its_budget_with_partition_charges() {
    let now = DateTime::from_timestamp(1_800_000_000, 0).expect("fixture time");
    for count in [64, 128, 1025] {
        for with_gpus in [false, true] {
            let mut f = Fixture::new(4);
            if !with_gpus {
                f.nodes[0].total_resources.gpus.clear();
            }
            f.nodes[0].partitions = vec!["default".into()];
            let template = f.nodes[0].clone();
            f.nodes = (0..count)
                .map(|index| {
                    let mut node = template.clone();
                    node.name = format!("node{index:04}");
                    node
                })
                .collect();
            for index in 0..count {
                f.hold_on(
                    index,
                    index as u32 + 1,
                    ResourceAllocations::with_scalar(4, 0),
                    true,
                );
            }
            for holder in &mut f.holders {
                holder.effective_partition = Some("default".into());
            }
            f.partitions.push(spur_core::partition::Partition {
                name: "default".into(),
                is_default: true,
                max_tres_per_node: [("cpu".into(), 4)].into(),
                ..Default::default()
            });
            f.usage = partition_usage_from_jobs(
                &f.holders,
                &f.partitions,
                now,
                ReleaseWaits::of(&Default::default()),
            );
            assert_eq!(f.usage["default"].len(), count);
            let mut incoming = request(1, 0);
            incoming.spec.partition = Some("default".into());
            assert_eq!(
                selected(&f, &incoming),
                Some(HashSet::from([1])),
                "{count} nodes, GPU inventory: {with_gpus}"
            );
            let facts = crate::victim_order::victim_facts(&f.holders, now);
            let order = VictimOrder::new(
                &spur_core::partition::DEFAULT_VICTIM_ORDER,
                &facts,
                incoming.job_id,
            );
            let selected = f
                .select_with_order(&incoming, &mut SearchBudget::default(), Some(order))
                .expect("configured ordering fits the default budget");
            assert_eq!(selected.len(), 1);
            assert!(selected.is_subset(&f.eligible));
        }
    }
}

#[test]
fn configured_order_ranks_exact_shared_allocations() {
    let now = DateTime::from_timestamp(1_800_000_000, 0).expect("fixture time");
    let mut f = Fixture::new(4);
    f.hold(1, 2, 0, &[], true);
    f.hold(2, 2, 0, &[], true);
    f.holders[0].start_time = Some(now - chrono::Duration::hours(1));
    f.holders[1].start_time = Some(now - chrono::Duration::minutes(1));
    let incoming = request(1, 0);
    let facts = crate::victim_order::victim_facts(&f.holders, now);
    let order = VictimOrder::new(
        &spur_core::partition::DEFAULT_VICTIM_ORDER,
        &facts,
        incoming.job_id,
    );
    assert_eq!(
        f.select_with_order(&incoming, &mut SearchBudget::default(), Some(order)),
        Some(HashSet::from([2]))
    );
    f.priorities.insert(2, 1);
    assert_eq!(
        f.select_with_order(&incoming, &mut SearchBudget::default(), Some(order)),
        Some(HashSet::from([1]))
    );
}

#[test]
fn topology_victims_enable_normal_scheduler_placement() {
    for topology_mode in ["tree", "block"] {
        let mut f = Fixture::new(4);
        let template = f.nodes[0].clone();
        for name in ["n2", "n3"] {
            let mut node = template.clone();
            node.name = name.into();
            f.nodes.push(node);
        }
        f.hold_on(1, 1, ResourceAllocations::with_scalar(4, 0), false);
        f.hold_on(2, 2, ResourceAllocations::with_scalar(1, 0), false);
        f.hold_on(2, 3, ResourceAllocations::with_scalar(3, 0), true);
        let topology = spur_core::topology::TopologyTree::from_blocks(
            &["n1".into(), "n2".into(), "n3".into()],
            2,
        );
        let state = ClusterState {
            topology: Some(&topology),
            ..f.state()
        };
        let mut incoming = request(1, 0);
        incoming.spec.num_nodes = 2;
        incoming.spec.num_tasks = 2;
        incoming.spec.topology = Some(topology_mode.into());
        let victims = satisfiable_victim_set(
            &incoming,
            &VictimPool {
                holders: &f.holders,
                evictable: &f.eligible,
                priority: &f.priorities,
                max_priority: None,
                order: None,
            },
            &state,
            DateTime::from_timestamp(1_800_000_000, 0).expect("fixture time"),
            &mut SearchBudget::default(),
        )
        .expect("bounded search")
        .expect("sufficient victim set");
        assert_eq!(victims, HashSet::from([3]));
        let mut released = f.nodes.clone();
        for holder in f.holders.iter().filter(|job| victims.contains(&job.job_id)) {
            for node in &mut released {
                if let Some(slice) = holder.per_node_alloc.get(&node.name) {
                    node.alloc_resources.subtract(slice);
                }
            }
        }
        let projected = ClusterState {
            nodes: &released,
            ..state
        };
        let assigned = spur_sched::traits::Scheduler::schedule(
            &mut BackfillScheduler::new(1),
            &[incoming],
            &projected,
        );
        assert_eq!(assigned.len(), 1, "{topology_mode}");
        assert_eq!(
            HashSet::from_iter(assigned[0].nodes.iter().map(String::as_str)),
            HashSet::from(["n1", "n3"])
        );
        assert_eq!(released[1].alloc_resources.cpus, 4);
        assert_eq!(released[2].alloc_resources.cpus, 1);
    }
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
fn exclusive_gpu_total_selects_only_a_node_with_enough_gpus() {
    for (big_first, fallback) in [(true, false), (false, false), (true, true), (false, true)] {
        let mut f = Fixture::new(4);
        let mut other = f.nodes[0].clone();
        other.name = "n2".into();
        f.nodes.push(other);
        let big = usize::from(!big_first);
        let small = 1 - big;
        f.nodes[big].total_resources.gpus.truncate(2);
        f.nodes[small].total_resources.gpus.truncate(1);
        for index in 0..2 {
            let allocation =
                spur_core::resource::build_exclusive_allocation(&f.nodes[index].total_resources, 0);
            f.hold_on(index, index as u32 + 1, allocation, true);
        }
        if fallback {
            for node in &mut f.nodes {
                node.partitions = vec!["default".into()];
            }
            f.partitions.push(spur_core::partition::Partition {
                name: "default".into(),
                is_default: true,
                max_mem_per_node: Some(100),
                ..Default::default()
            });
        }
        let mut job = request(1, 0);
        job.spec.exclusive = true;
        job.spec.gpus = Some(GpuRequest::new(2, None));
        assert_eq!(selected(&f, &job), Some(HashSet::from([big as u32 + 1])));
        f.eligible.remove(&(big as u32 + 1));
        assert_eq!(selected(&f, &job), None);
    }
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
fn input_work_and_call_limits_return_no_victims() {
    let mut f = Fixture::new(4);
    f.hold(1, 4, 0, &[], true);
    let mut budget = SearchBudget {
        calls_left: 0,
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
fn direct_selection_handles_many_necessary_victims() {
    let mut f = Fixture::new(64);
    for id in 1..=64 {
        f.hold(id, 1, 0, &[], true);
    }
    assert_eq!(selected(&f, &request(64, 0)), Some((1..=64).collect()));
    assert_eq!(f.nodes[0].alloc_resources.cpus, 64);
}

#[test]
fn a_cycle_budget_is_shared_across_requests() {
    let mut f = Fixture::new(4);
    f.hold(1, 4, 0, &[], true);
    let mut budget = SearchBudget {
        calls_left: 1,
        work_left: 2_000_000,
    };
    assert_eq!(
        f.select(&request(1, 0), &mut budget),
        Some(HashSet::from([1]))
    );
    assert_eq!(f.select(&request(1, 0), &mut budget), None);
}

#[test]
fn irrelevant_eligible_ids_do_not_block_a_sufficient_victim() {
    let mut f = Fixture::new(4);
    f.hold(1, 4, 0, &[], true);
    f.eligible.extend(1..=MAX_VICTIMS as u32 + 1);
    assert_eq!(selected(&f, &request(1, 0)), Some(HashSet::from([1])));
}

#[test]
fn oversized_node_domain_defers() {
    let mut f = Fixture::new(4);
    f.hold(1, 4, 0, &[], true);
    let mut job = request(1, 0);
    job.spec.num_nodes = MAX_NODES as u32 + 1;
    assert_eq!(selected(&f, &job), None);
}

#[test]
fn large_candidate_pool_selects_only_the_shortfall() {
    for fallback in [false, true] {
        let mut f = Fixture::new(MAX_VICTIMS as u32 + 2);
        f.hold(10_000, 1, 0, &[], false);
        for id in 1..=MAX_VICTIMS as u32 + 1 {
            f.hold(id, 1, 0, &[], true);
            f.priorities.insert(id, 1);
        }
        let cheapest = MAX_VICTIMS as u32 + 1;
        f.priorities.insert(cheapest, 0);
        if fallback {
            f.nodes[0].partitions = vec!["default".into()];
            f.partitions.push(spur_core::partition::Partition {
                name: "default".into(),
                is_default: true,
                max_mem_per_node: Some(100),
                ..Default::default()
            });
        }
        assert_eq!(
            selected(&f, &request(1, 0)),
            Some(HashSet::from([cheapest]))
        );
        assert_eq!(f.nodes[0].alloc_resources.cpus, MAX_VICTIMS as u32 + 2);
        assert!(f.holders.iter().all(|job| job.state == JobState::Running));
    }
}

#[test]
fn selected_victim_limit_still_defers_without_eviction() {
    let count = MAX_VICTIMS as u32 + 1;
    let mut f = Fixture::new(count);
    for id in 1..=count {
        f.hold(id, 1, 0, &[], true);
    }
    assert_eq!(
        satisfiable_victim_set(
            &request(count, 0),
            &VictimPool {
                evictable: &f.eligible,
                holders: &f.holders,
                priority: &f.priorities,
                max_priority: None,
                order: None,
            },
            &f.state(),
            DateTime::from_timestamp(1_800_000_000, 0).expect("fixture time"),
            &mut SearchBudget::default(),
        ),
        Err(SelectionDeferred)
    );
    assert_eq!(f.nodes[0].alloc_resources.cpus, count);
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
            order: None,
        };
        let mut times = Vec::new();
        let mut count = 0usize;
        for _ in 0..256 {
            let begin = std::time::Instant::now();
            let outcome =
                satisfiable_victim_set(&incoming, &pool, &state, now, &mut SearchBudget::default());
            times.push(begin.elapsed().as_nanos());
            count = std::hint::black_box(outcome)
                .ok()
                .flatten()
                .map_or(0, |ids| ids.len());
        }
        times.sort_unstable();
        eprintln!(
            "PROFILE {label} median_ns={} p95_ns={} victims={count}",
            times[128], times[243]
        );
    }
}

fn profile_mixed_workload(needed: u32) {
    let mut f = Fixture::new(4);
    let template = f.nodes[0].clone();
    f.nodes = (0..128)
        .map(|i| {
            let mut node = template.clone();
            node.name = format!("node{i:04}");
            node
        })
        .collect();
    for i in 0..f.nodes.len() {
        f.hold_on(
            i,
            10_000 + i as u32,
            ResourceAllocations::with_scalar(1, 20),
            false,
        );
        for slot in 0..2 {
            let mut slice = ResourceAllocations::with_scalar(1, 20);
            slice.add(&ResourceAllocations::from_device_ids("gpu", &[1000 + slot]));
            f.hold_on(i, 1 + i as u32 * 2 + slot as u32, slice, true);
        }
    }
    let mut first = 0;
    while first < f.nodes.len() {
        let width = match first % 64 {
            48 => 16,
            24 | 40 => 8,
            n if n % 4 == 2 => 2,
            _ => 1,
        };
        let id = 1_000 + first as u32;
        let mut job = Job::new(
            id,
            JobSpec {
                num_nodes: width as u32,
                ..Default::default()
            },
        );
        job.state = JobState::Running;
        for index in first..first + width {
            let mut slice = ResourceAllocations::with_scalar(1, 20);
            slice.add(&ResourceAllocations::from_device_ids("gpu", &[1002]));
            f.nodes[index].alloc_resources.add(&slice);
            let name = f.nodes[index].name.clone();
            job.allocated_nodes.push(name.clone());
            job.per_node_alloc.insert(name, slice);
        }
        f.holders.push(job);
        f.eligible.insert(id);
        first += width;
    }
    let mut incoming = request(1, 20);
    incoming.spec.num_nodes = needed;
    incoming.spec.num_tasks = needed;
    incoming.spec.gpus_per_node = Some(GpuRequest::new(1, None));
    let now = DateTime::from_timestamp(1_800_000_000, 0).expect("fixture time");
    let facts = crate::victim_order::victim_facts(&f.holders, now);
    let order = VictimOrder::new(
        &spur_core::partition::DEFAULT_VICTIM_ORDER,
        &facts,
        incoming.job_id,
    );
    let mut times = Vec::new();
    let mut calls = Vec::new();
    let mut work = Vec::new();
    let mut victims = 0;
    for _ in 0..64 {
        let mut budget = SearchBudget::default();
        let start = std::time::Instant::now();
        let chosen = f
            .select_with_order(&incoming, &mut budget, Some(order))
            .expect("mixed workload makes progress");
        times.push(start.elapsed().as_micros());
        victims = chosen.len();
        assert!(chosen.is_subset(&f.eligible));
        calls.push(SearchBudget::default().calls_left - budget.calls_left);
        work.push(SearchBudget::default().work_left - budget.work_left);
    }
    times.sort_unstable();
    eprintln!("MIXED nodes=128 incoming={needed} candidates={} median_us={} p95_us={} victims={victims} max_calls={} max_work={}",
        f.eligible.len(), times[32], times[60], calls.iter().max().expect("calls"), work.iter().max().expect("work"));
}

#[test]
#[ignore = "manual mixed-workload timing and process memory profile"]
fn mixed_workload_profile_one_node() {
    profile_mixed_workload(1);
}

#[test]
#[ignore = "manual mixed-workload timing and process memory profile"]
fn mixed_workload_profile_two_nodes() {
    profile_mixed_workload(2);
}

#[test]
#[ignore = "manual mixed-workload timing and process memory profile"]
fn mixed_workload_profile_eight_nodes() {
    profile_mixed_workload(8);
}

#[test]
#[ignore = "manual mixed-workload timing and process memory profile"]
fn mixed_workload_profile_sixteen_nodes() {
    profile_mixed_workload(16);
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
    cluster.check_node_health_at(
        90,
        MarkDownPolicy::Only(HashSet::new()),
        now,
        std::time::Instant::now(),
    );

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
                "x".repeat(spur_sched::immediate::MAX_TEXT / 2)
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

#[test]
fn many_node_victims_need_one_placement_verification() {
    for (nodes, per_node) in [(5, 12), (16, 64), (1, MAX_VICTIMS)] {
        let mut f = Fixture::new(per_node as u32 + 1);
        let template = f.nodes[0].clone();
        f.nodes = (0..nodes)
            .map(|i| {
                let mut node = template.clone();
                node.name = format!("node{i:04}");
                node
            })
            .collect();
        for i in 0..nodes {
            f.hold_on(
                i,
                10_000 + i as u32,
                ResourceAllocations::with_scalar(1, 0),
                false,
            );
            for j in 0..per_node {
                f.hold_on(
                    i,
                    (i * per_node + j + 1) as u32,
                    ResourceAllocations::with_scalar(1, 0),
                    true,
                );
            }
        }
        let mut incoming = request(per_node as u32, 0);
        incoming.spec.num_nodes = nodes as u32;
        incoming.spec.num_tasks = nodes as u32;
        let mut budget = SearchBudget::default();
        assert_eq!(f.select(&incoming, &mut budget), Some(f.eligible.clone()));
        assert_eq!(budget.calls_left, SearchBudget::default().calls_left - 1);
        f.partitions.push(spur_core::partition::Partition {
            name: "default".into(),
            is_default: true,
            max_tres_per_node: [("cpu".into(), per_node as u64 + 1)].into(),
            ..Default::default()
        });
        for node in &mut f.nodes {
            node.partitions = vec!["default".into()];
        }
        for holder in &mut f.holders {
            holder.effective_partition = Some("default".into());
        }
        f.usage = partition_usage_from_jobs(
            &f.holders,
            &f.partitions,
            DateTime::from_timestamp(1_800_000_000, 0).expect("fixture time"),
            ReleaseWaits::of(&Default::default()),
        );
        let mut budget = SearchBudget::default();
        assert_eq!(f.select(&incoming, &mut budget), Some(f.eligible.clone()));
        assert_eq!(budget.calls_left, SearchBudget::default().calls_left - 1);
    }
}

#[test]
fn full_gpu_cluster_fits_snapshot_and_selection_limits() {
    use crate::raft::StateMachineApply;
    use spur_core::wal::WalOperation;
    let dir = tempfile::TempDir::new().expect("state directory");
    let cluster = ClusterManager::new(
        spur_core::config::SlurmConfig::load_from_str(
            "cluster_name = \"test\"\n[controller]\nhostname = \"test\"\n",
        )
        .expect("config"),
        dir.path(),
    )
    .expect("cluster");
    let mut f = Fixture::new(8);
    let mut template = f.nodes[0].clone();
    template.partitions = vec!["default".into()];
    template.features = vec!["gpu".into(), "compute".into()];
    template.total_resources.gpus = (0..8)
        .map(|i| {
            let mut gpu = template.total_resources.gpus[0].clone();
            gpu.device_id = i;
            gpu.stable_id = u64::from(i) + 1000;
            gpu.gpu_type = "MI300X".into();
            gpu
        })
        .collect();
    f.nodes = (0..MAX_NODES)
        .map(|i| {
            let mut node = template.clone();
            node.name = format!("node{i:04}");
            let register: WalOperation =
                serde_json::from_value(serde_json::json!({"NodeRegister": {
                    "name": node.name, "resources": node.total_resources, "address": ""
                }}))
                .expect("registration");
            cluster.apply_operation(&register);
            node
        })
        .collect();
    let now = DateTime::from_timestamp(1_800_000_000, 0).expect("fixture time");
    let mut budget = SearchBudget::default();
    let snapshot =
        Snapshot::capture(&cluster, &f.state(), &[], now, &mut budget).expect("full snapshot");
    assert_eq!(snapshot.nodes.len(), MAX_NODES);
    f.nodes = snapshot.nodes;
    for i in 0..MAX_NODES {
        f.hold_on(
            i,
            i as u32 + 1,
            ResourceAllocations::with_scalar(8, 0),
            true,
        );
    }
    assert_eq!(
        f.select(&request(1, 0), &mut budget),
        Some(HashSet::from([1]))
    );
}

#[test]
fn selection_deferral_is_distinct_from_insufficient_capacity() {
    let mut f = Fixture::new(4);
    f.hold(1, 1, 0, &[], true);
    f.hold(2, 3, 0, &[], false);
    let pool = VictimPool {
        evictable: &f.eligible,
        holders: &f.holders,
        priority: &f.priorities,
        max_priority: None,
        order: None,
    };
    let incoming = request(2, 0);
    let now = DateTime::from_timestamp(1_800_000_000, 0).expect("fixture time");
    assert_eq!(
        satisfiable_victim_set(
            &incoming,
            &pool,
            &f.state(),
            now,
            &mut SearchBudget::default()
        ),
        Ok(None)
    );
    assert_eq!(
        satisfiable_victim_set(
            &incoming,
            &pool,
            &f.state(),
            now,
            &mut SearchBudget {
                calls_left: 0,
                work_left: 0
            }
        ),
        Err(SelectionDeferred)
    );
}

#[test]
fn wide_protected_holder_preserves_budget_and_completed_node_accounting() {
    use crate::raft::StateMachineApply;
    use spur_core::wal::WalOperation;

    let dir = tempfile::TempDir::new().expect("state directory");
    let cluster = ClusterManager::new(
        spur_core::config::SlurmConfig::load_from_str(
            r#"cluster_name = "test"
[controller]
hostname = "test"
[[partitions]]
name = "default"
nodes = "ALL"
default = true
max_tres_per_node = { cpu = 2 }
"#,
        )
        .expect("config"),
        dir.path(),
    )
    .expect("cluster");
    let apply = |value: serde_json::Value| {
        cluster.apply_operation(&serde_json::from_value::<WalOperation>(value).expect("operation"));
    };
    let mut f = Fixture::new(2);
    let template = f.nodes[0].clone();
    f.nodes = (0..1024)
        .map(|i| {
            let mut node = template.clone();
            node.name = format!("node{i:04}");
            node.partitions = vec!["default".into()];
            apply(serde_json::json!({"NodeRegister": {
                "name": node.name, "resources": node.total_resources, "address": ""
            }}));
            node
        })
        .collect();
    f.partitions.push(spur_core::partition::Partition {
        name: "default".into(),
        is_default: true,
        max_tres_per_node: [("cpu".into(), 2)].into(),
        ..Default::default()
    });
    let slice = ResourceAllocations::with_scalar(1, 0);
    let wide_nodes: Vec<_> = f.nodes.iter().map(|node| node.name.clone()).collect();
    let wide_alloc: HashMap<_, _> = wide_nodes
        .iter()
        .map(|name| (name.clone(), slice.clone()))
        .collect();
    for (id, nodes, allocations) in [
        (1, wide_nodes.clone(), wide_alloc.clone()),
        (
            2,
            vec!["node0000".into()],
            [("node0000".into(), slice.clone())].into(),
        ),
    ] {
        let spec = JobSpec {
            num_nodes: nodes.len() as u32,
            ..Default::default()
        };
        apply(serde_json::json!({"JobSubmit": {"job_id": id, "spec": spec}}));
        apply(serde_json::json!({"JobStart": {
            "job_id": id, "nodes": nodes,
            "resources": ResourceAllocations::with_scalar(nodes.len() as u32, 0),
            "per_node_alloc": allocations, "effective_partition": "default"
        }}));
        apply(serde_json::json!({"JobStateChange": {
            "job_id": id, "old_state": "PENDING", "new_state": "RUNNING"
        }}));
    }
    let assignment = Assignment {
        job_id: 1,
        nodes: wide_nodes,
        per_node_alloc: wide_alloc,
        partition: Some("default".into()),
        memory_adjustment: None,
    };
    let now = DateTime::from_timestamp(1_800_000_000, 0).expect("fixture time");
    let mut incoming = request(1, 0);
    incoming.spec.nodelist = Some("node0000".into());
    for completed in [false, true] {
        if completed {
            apply(serde_json::json!({"JobNodeComplete": {
                "job_id": 1, "node_name": "node0001", "exit_code": 0, "signal": 0
            }}));
        }
        let assigned = if completed {
            &[][..]
        } else {
            std::slice::from_ref(&assignment)
        };
        let mut budget = SearchBudget::default();
        let snapshot = Snapshot::capture(&cluster, &f.state(), assigned, now, &mut budget)
            .expect("wide holder snapshot fits the default budget");
        let wide = snapshot
            .holders
            .iter()
            .find(|job| job.job_id == 1)
            .expect("protected job");
        assert_eq!(wide.allocated_nodes.len(), 1024 - usize::from(completed));
        assert_eq!(wide.per_node_alloc.contains_key("node0001"), !completed);
        assert_eq!(
            snapshot.nodes[1].alloc_resources.cpus,
            u32::from(!completed)
        );
        assert_eq!(
            snapshot.usage["default"].contains_key("node0001"),
            !completed
        );
        assert_eq!(
            satisfiable_victim_set(
                &incoming,
                &VictimPool {
                    evictable: &HashSet::from([2]),
                    holders: &snapshot.holders,
                    priority: &HashMap::new(),
                    max_priority: None,
                    order: None,
                },
                &snapshot.state(&f.state()),
                now,
                &mut budget,
            ),
            Ok(Some(HashSet::from([2])))
        );
        assert_eq!(snapshot.nodes[0].alloc_resources.cpus, 2);
    }
}

#[tokio::test]
async fn reclaim_without_borrowers_skips_snapshot_work() {
    use crate::raft::StateMachineApply;
    use spur_core::wal::WalOperation;
    let dir = tempfile::TempDir::new().expect("state directory");
    let cluster = std::sync::Arc::new(
        ClusterManager::new(
            spur_core::config::SlurmConfig::load_from_str(
                "cluster_name = \"test\"\n[controller]\nhostname = \"test\"\n",
            )
            .expect("config"),
            dir.path(),
        )
        .expect("cluster"),
    );
    let f = Fixture::new(4);
    let register: WalOperation = serde_json::from_value(serde_json::json!({"NodeRegister": {
        "name": "n1", "resources": f.nodes[0].total_resources, "address": ""
    }}))
    .expect("registration");
    cluster.apply_operation(&register);
    let incoming = request(1, 0);
    let mut budget = SearchBudget::default();
    assert_eq!(
        super::super::reclaim_for_unplaced(
            &cluster,
            &[&incoming],
            &HashSet::new(),
            &f.state(),
            &Default::default(),
            &[],
            &mut budget,
            |_| true,
        )
        .await,
        (Vec::new(), None)
    );
    assert_eq!(budget.work_left, SearchBudget::default().work_left);
    assert_eq!(budget.calls_left, SearchBudget::default().calls_left);
}
