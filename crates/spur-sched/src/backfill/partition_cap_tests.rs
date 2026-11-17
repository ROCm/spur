// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use super::tests::Recorder;
use super::*;
use crate::partition_cap::{PartitionNodeUsage, RunningCharge};
use spur_core::job::JobSpec;
use spur_core::node::NodeState;
use spur_core::resource::{AllocatedDevice, GpuLinkType, GpuResource};

fn now() -> chrono::DateTime<Utc> {
    chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap()
}

fn hours(h: i64) -> chrono::DateTime<Utc> {
    now() + Duration::hours(h)
}

fn node(name: &str, cpus: u32, memory_mb: u64, partitions: &[&str]) -> Node {
    let mut node = Node::new(
        name.into(),
        ResourceSet {
            cpus,
            memory_mb,
            ..Default::default()
        },
    );
    node.state = NodeState::Idle;
    node.partitions = partitions.iter().map(|p| p.to_string()).collect();
    node
}

/// Eight GPUs: stable ids 0-3 are `a100`, 4-7 are `mi300x`.
fn gpu_node(name: &str, partitions: &[&str]) -> Node {
    let mut n = node(name, 64, 256_000, partitions);
    n.total_resources.gpus = (0..8u32)
        .map(|i| GpuResource {
            device_id: i,
            gpu_type: if i < 4 { "a100" } else { "mi300x" }.into(),
            memory_mb: 0,
            peer_gpus: vec![],
            link_type: GpuLinkType::PCIe,
            topology: None,
            stable_id: u64::from(i),
        })
        .collect();
    n
}

fn partition(name: &str, caps: &[(&str, u64)]) -> Partition {
    Partition {
        name: name.into(),
        max_tres_per_node: caps.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
        ..Default::default()
    }
}

fn job(id: u32, partition: &str, cpus: u32, hours: i64) -> Job {
    Job::new(
        id,
        JobSpec {
            name: format!("job{id}"),
            partition: (!partition.is_empty()).then(|| partition.to_string()),
            user: "test".into(),
            num_nodes: 1,
            num_tasks: 1,
            cpus_per_task: cpus,
            time_limit: Some(Duration::hours(hours)),
            ..Default::default()
        },
    )
}

fn gpu_job(id: u32, partition: &str, gres: &str) -> Job {
    let mut j = job(id, partition, 1, 1);
    j.spec.gres = vec![gres.to_string()];
    j
}

fn mem_job(id: u32, partition: &str, memory_mb: u64) -> Job {
    let mut j = job(id, partition, 1, 1);
    j.spec.memory_per_node_mb = Some(memory_mb);
    j
}

#[derive(Default)]
struct Fixture {
    nodes: Vec<Node>,
    partitions: Vec<Partition>,
    busy_until: HashMap<String, chrono::DateTime<Utc>>,
    usage: PartitionNodeUsage,
}

impl Fixture {
    fn new(nodes: Vec<Node>, partitions: Vec<Partition>) -> Self {
        Self {
            nodes,
            partitions,
            ..Default::default()
        }
    }

    /// A running job of `partition` holding `charge` on `node` until `until`.
    fn running(
        mut self,
        partition: &str,
        node: &str,
        alloc: ResourceAllocations,
        charge: TresCharge,
        until: chrono::DateTime<Utc>,
    ) -> Self {
        if let Some(n) = self.nodes.iter_mut().find(|n| n.name == node) {
            n.alloc_resources.add(&alloc);
        }
        let end = self.busy_until.entry(node.to_string()).or_insert(until);
        *end = (*end).max(until);
        self.usage
            .entry(partition.to_string())
            .or_default()
            .entry(node.to_string())
            .or_default()
            .push(RunningCharge {
                job_id: 0,
                tres: charge,
                until,
            });
        self
    }

    fn running_cpus(self, partition: &str, node: &str, cpus: u32, until_h: i64) -> Self {
        self.running(
            partition,
            node,
            ResourceAllocations::with_scalar(cpus, 0),
            TresCharge::new(u64::from(cpus), 0, 0, None),
            hours(until_h),
        )
    }

    fn schedule(&self, sched: &mut BackfillScheduler, jobs: &[Job]) -> Vec<Assignment> {
        self.schedule_admitting(sched, jobs, &mut AdmitAll)
    }

    fn schedule_admitting(
        &self,
        sched: &mut BackfillScheduler,
        jobs: &[Job],
        admission: &mut dyn StartAdmission,
    ) -> Vec<Assignment> {
        let cluster = ClusterState {
            nodes: &self.nodes,
            partitions: &self.partitions,
            reservations: &[],
            topology: None,
            fabric: None,
            busy_until: &self.busy_until,
            preemption_handoffs: &Default::default(),
            partition_node_usage: &self.usage,
        };
        sched.schedule_with(jobs, &cluster, now(), admission)
    }
}

fn started(assignments: &[Assignment]) -> Vec<JobId> {
    assignments.iter().map(|a| a.job_id).collect()
}

/// The future slot held for `job_id`: (nodes, start), if any.
fn reserved(
    sched: &BackfillScheduler,
    job_id: JobId,
) -> Option<(Vec<String>, chrono::DateTime<Utc>)> {
    let mut nodes = Vec::new();
    let mut start = None;
    for tl in &sched.timelines {
        for iv in tl.intervals.iter().filter(|iv| iv.job_id == Some(job_id)) {
            if iv.start > now() {
                nodes.push(tl.node_name.clone());
                start = Some(start.map_or(iv.start, |s: chrono::DateTime<Utc>| s.max(iv.start)));
            }
        }
    }
    start.map(|s| (nodes, s))
}

fn outcome(sched: &BackfillScheduler, job_id: JobId) -> Option<UnplacedKind> {
    sched.last_outcome.get(&job_id).map(|o| o.kind)
}

fn overshoot(sched: &BackfillScheduler, job_id: JobId) -> Option<&PartitionTresOvershoot> {
    sched
        .partition_cap_blocks()
        .get(&job_id)
        .and_then(|b| b.overshoots.first())
}

#[test]
fn cpu_cap_binds_exactly_at_the_limit_on_an_idle_node() {
    let fx = Fixture::new(
        vec![node("n1", 64, 256_000, &["cpu"])],
        vec![partition("cpu", &[("cpu", 8)])],
    );

    let mut sched = BackfillScheduler::new(100);
    let a = fx.schedule(&mut sched, &[job(1, "cpu", 8, 1)]);
    assert_eq!(started(&a), [1]);
    assert_eq!(a[0].partition.as_deref(), Some("cpu"));

    let mut sched = BackfillScheduler::new(100);
    let a = fx.schedule(&mut sched, &[job(2, "cpu", 9, 1)]);
    assert!(a.is_empty(), "64 idle CPUs must not override a cap of 8");
    assert_eq!(
        outcome(&sched, 2),
        Some(UnplacedKind::PartitionCapNeverFits)
    );
    assert_eq!(reserved(&sched, 2), None);
    let o = overshoot(&sched, 2).expect("cap block recorded");
    assert_eq!(
        (o.resource.as_str(), o.used, o.requested, o.limit),
        ("cpu", 0, 9, 8)
    );
}

#[test]
fn a_refused_start_leaves_its_partition_cap_to_the_next_job() {
    let fx = Fixture::new(
        vec![node("n1", 64, 256_000, &["cpu"])],
        vec![partition("cpu", &[("cpu", 8)])],
    );
    let mut sched = BackfillScheduler::new(100);
    let mut admission = Recorder::refusing(1);
    let a = fx.schedule_admitting(
        &mut sched,
        &[job(1, "cpu", 8, 1), job(2, "cpu", 8, 1)],
        &mut admission,
    );
    assert_eq!(started(&a), [2]);
    assert_eq!(outcome(&sched, 1), Some(UnplacedKind::RefusedAtStart));
}

#[test]
fn a_refused_het_job_leaves_its_partition_cap_to_the_next_job() {
    let fx = Fixture::new(
        vec![node("n1", 64, 256_000, &["cpu"])],
        vec![partition("cpu", &[("cpu", 8)])],
    );
    let mut first = job(1, "cpu", 4, 1);
    first.het_group = Some(0);
    let mut second = job(2, "cpu", 4, 1);
    second.het_group = Some(1);
    second.het_job_id = Some(1);
    let mut sched = BackfillScheduler::new(100);
    let mut admission = Recorder::refusing(2);
    let a = fx.schedule_admitting(
        &mut sched,
        &[first, second, job(3, "cpu", 8, 1)],
        &mut admission,
    );
    assert_eq!(started(&a), [3]);
}

#[test]
fn cpu_cap_is_cumulative_within_one_partition() {
    let fx = Fixture::new(
        vec![node("n1", 64, 256_000, &["cpu"])],
        vec![partition("cpu", &[("cpu", 8)])],
    );
    let mut sched = BackfillScheduler::new(100);
    let jobs = [
        job(1, "cpu", 4, 1),
        job(2, "cpu", 4, 1),
        job(3, "cpu", 4, 1),
    ];
    let a = fx.schedule(&mut sched, &jobs);
    assert_eq!(started(&a), [1, 2]);
    assert_eq!(reserved(&sched, 3), Some((vec!["n1".into()], hours(1))));
    let o = overshoot(&sched, 3).expect("cap block recorded");
    assert_eq!((o.used, o.requested, o.limit), (8, 4, 8));
    assert_eq!(sched.timelines[0].charged_at("cpu", now()).get("cpu"), 8);
}

#[test]
fn running_usage_counts_toward_the_cap() {
    let fx = Fixture::new(
        vec![node("n1", 64, 256_000, &["cpu"])],
        vec![partition("cpu", &[("cpu", 8)])],
    )
    .running_cpus("cpu", "n1", 6, 2);
    let mut sched = BackfillScheduler::new(100);
    let a = fx.schedule(&mut sched, &[job(1, "cpu", 4, 1), job(2, "cpu", 2, 1)]);
    assert_eq!(started(&a), [2]);
    assert_eq!(reserved(&sched, 1).map(|r| r.1), Some(hours(2)));
}

#[test]
fn caps_are_independent_across_partitions_sharing_a_node() {
    let fx = Fixture::new(
        vec![node("n1", 64, 256_000, &["cpu", "gpu"])],
        vec![
            partition("cpu", &[("cpu", 8)]),
            partition("gpu", &[("cpu", 32)]),
        ],
    )
    .running_cpus("cpu", "n1", 8, 2);
    let mut sched = BackfillScheduler::new(100);
    let a = fx.schedule(&mut sched, &[job(1, "cpu", 1, 1), job(2, "gpu", 32, 1)]);
    assert_eq!(started(&a), [2]);
    assert_eq!(a[0].partition.as_deref(), Some("gpu"));
    assert_eq!(overshoot(&sched, 1).map(|o| o.used), Some(8));
}

#[test]
fn mem_tres_cap_is_cumulative_within_a_partition() {
    let fx = Fixture::new(
        vec![node("n1", 64, 262_144, &["cpu"])],
        vec![partition("cpu", &[("mem", 131_072)])],
    );
    let mut sched = BackfillScheduler::new(100);
    let jobs = [
        mem_job(1, "cpu", 65_536),
        mem_job(2, "cpu", 65_536),
        mem_job(3, "cpu", 65_536),
    ];
    let a = fx.schedule(&mut sched, &jobs);
    assert_eq!(started(&a), [1, 2]);
    let o = overshoot(&sched, 3).expect("cap block recorded");
    assert_eq!(
        (o.resource.as_str(), o.used, o.requested, o.limit),
        ("mem", 131_072, 65_536, 131_072)
    );
}

#[test]
fn max_mem_per_node_alone_does_not_accumulate() {
    let mut p = partition("cpu", &[]);
    p.max_mem_per_node = Some(65_536);
    let fx = Fixture::new(vec![node("n1", 64, 262_144, &["cpu"])], vec![p]);
    let mut sched = BackfillScheduler::new(100);
    let jobs: Vec<Job> = (1..=4).map(|id| mem_job(id, "cpu", 65_536)).collect();
    assert_eq!(started(&fx.schedule(&mut sched, &jobs)), [1, 2, 3, 4]);
}

#[test]
fn per_job_vs_cumulative_memory_worked_example() {
    let mut cpu = partition("cpu", &[("mem", 131_072)]);
    cpu.max_mem_per_node = Some(65_536);
    let fx = Fixture::new(
        vec![node("n1", 64, 262_144, &["cpu", "gpu"])],
        vec![cpu, partition("gpu", &[])],
    );
    let mut sched = BackfillScheduler::new(100);
    let jobs = [
        mem_job(1, "cpu", 65_536),
        mem_job(2, "cpu", 65_536),
        mem_job(3, "cpu", 65_536),
        mem_job(4, "gpu", 131_072),
    ];
    let a = fx.schedule(&mut sched, &jobs);
    assert_eq!(
        started(&a),
        [1, 2, 4],
        "the other 128 GB stays free for gpu"
    );
}

#[test]
fn typed_gpu_request_charges_both_typed_and_generic_keys() {
    let fx = Fixture::new(
        vec![gpu_node("n1", &["gpu"])],
        vec![partition("gpu", &[("gres/gpu:a100", 2)])],
    );
    let mut sched = BackfillScheduler::new(100);
    let a = fx.schedule(
        &mut sched,
        &[
            gpu_job(1, "gpu", "gpu:a100:2"),
            gpu_job(2, "gpu", "gpu:a100:1"),
        ],
    );
    assert_eq!(started(&a), [1]);
    let o = overshoot(&sched, 2).expect("cap block recorded");
    assert_eq!(
        (o.resource.as_str(), o.used, o.requested, o.limit),
        ("gres/gpu:a100", 2, 1, 2)
    );
    let charged = sched.timelines[0].charged_at("gpu", now());
    assert_eq!(
        (charged.get("gres/gpu:a100"), charged.get("gres/gpu")),
        (2, 2)
    );

    let fx = Fixture::new(
        vec![gpu_node("n1", &["gpu"])],
        vec![partition("gpu", &[("gres/gpu", 2)])],
    );
    let mut sched = BackfillScheduler::new(100);
    let a = fx.schedule(
        &mut sched,
        &[gpu_job(1, "gpu", "gpu:a100:2"), gpu_job(2, "gpu", "gpu:1")],
    );
    assert_eq!(started(&a), [1], "typed GPUs count against the generic key");
    assert_eq!(
        overshoot(&sched, 2).map(|o| o.resource.as_str()),
        Some("gres/gpu")
    );
}

#[test]
fn generic_gpu_request_bypasses_a_typed_cap() {
    let fx = Fixture::new(
        vec![gpu_node("n1", &["gpu"])],
        vec![partition("gpu", &[("gres/gpu:a100", 2)])],
    );
    let mut sched = BackfillScheduler::new(100);
    let a = fx.schedule(
        &mut sched,
        &[gpu_job(1, "gpu", "gpu:a100:2"), gpu_job(2, "gpu", "gpu:4")],
    );
    assert_eq!(started(&a), [1, 2]);
    assert_eq!(
        sched.timelines[0]
            .charged_at("gpu", now())
            .get("gres/gpu:a100"),
        2,
        "typed usage accrues from typed requests only"
    );
}

#[test]
fn gres_gpu_zero_forbids_all_gpus_in_the_partition() {
    let fx = Fixture::new(
        vec![gpu_node("n1", &["cpuonly"])],
        vec![partition("cpuonly", &[("gres/gpu", 0)])],
    );
    let mut sched = BackfillScheduler::new(100);
    let jobs = [
        gpu_job(1, "cpuonly", "gpu:1"),
        gpu_job(2, "cpuonly", "gpu:a100:1"),
        job(3, "cpuonly", 32, 1),
    ];
    let a = fx.schedule(&mut sched, &jobs);
    assert_eq!(started(&a), [3]);
    for id in [1, 2] {
        assert_eq!(
            outcome(&sched, id),
            Some(UnplacedKind::PartitionCapNeverFits)
        );
        assert_eq!(reserved(&sched, id), None);
    }
}

#[test]
fn backfill_honors_every_capped_key_for_normal_placement() {
    let caps = [
        ("cpu", 8),
        ("mem", 1_000),
        ("gres/gpu", 2),
        ("gres/gpu:a100", 1),
    ];
    let fx = Fixture::new(vec![gpu_node("n1", &["p"])], vec![partition("p", &caps)]);
    let within = {
        let mut j = gpu_job(1, "p", "gpu:a100:1");
        j.spec.cpus_per_task = 8;
        j.spec.memory_per_node_mb = Some(1_000);
        j
    };
    let mut sched = BackfillScheduler::new(100);
    assert_eq!(started(&fx.schedule(&mut sched, &[within])), [1]);

    let over_cpu = job(2, "p", 9, 1);
    let over_mem = mem_job(3, "p", 1_001);
    let over_gpu = gpu_job(4, "p", "gpu:3");
    let over_typed = gpu_job(5, "p", "gpu:a100:2");
    for (j, key) in [
        (over_cpu, "cpu"),
        (over_mem, "mem"),
        (over_gpu, "gres/gpu"),
        (over_typed, "gres/gpu:a100"),
    ] {
        let id = j.job_id;
        let mut sched = BackfillScheduler::new(100);
        assert!(
            fx.schedule(&mut sched, &[j]).is_empty(),
            "{key} cap ignored"
        );
        assert_eq!(
            overshoot(&sched, id).map(|o| o.resource.as_str()),
            Some(key)
        );
    }
}

#[test]
fn idle_fill_placements_honor_the_cap() {
    let fx = Fixture::new(
        vec![node("n1", 64, 256_000, &["cpu"])],
        vec![partition("cpu", &[("cpu", 8)])],
    )
    .running_cpus("cpu", "n1", 6, 2);
    let mut over = job(1, "cpu", 4, 1);
    over.idle_fill = true;
    let mut within = job(2, "cpu", 2, 1);
    within.idle_fill = true;

    let mut sched = BackfillScheduler::new(100);
    let a = fx.schedule(&mut sched, &[over, within]);
    assert_eq!(started(&a), [2]);
    assert_eq!(
        reserved(&sched, 1),
        None,
        "a borrowed run never holds a future slot"
    );
    assert_eq!(outcome(&sched, 1), Some(UnplacedKind::NoCapacityAtStart));
    assert!(sched.partition_cap_blocks().contains_key(&1));
}

#[test]
fn idle_fill_usage_counts_toward_the_cap() {
    let fx = Fixture::new(
        vec![node("n1", 64, 256_000, &["cpu"])],
        vec![partition("cpu", &[("cpu", 8)])],
    );
    let mut borrowed = job(1, "cpu", 4, 1);
    borrowed.idle_fill = true;

    let mut sched = BackfillScheduler::new(100);
    let a = fx.schedule(&mut sched, &[borrowed, job(2, "cpu", 8, 1)]);
    assert_eq!(started(&a), [1]);
    assert_eq!(reserved(&sched, 2).map(|r| r.1), Some(hours(1)));
}

#[test]
fn starvation_is_avoided_by_a_cap_aware_reservation() {
    let fx = Fixture::new(
        vec![node("n1", 64, 256_000, &["cpu"])],
        vec![partition("cpu", &[("cpu", 8)])],
    )
    .running_cpus("cpu", "n1", 4, 2);

    let mut sched = BackfillScheduler::new(100);
    let a = fx.schedule(&mut sched, &[job(1, "cpu", 8, 1), job(2, "cpu", 4, 3)]);
    assert!(
        a.is_empty(),
        "the long low-priority job would overlap the reserved window"
    );
    assert_eq!(reserved(&sched, 1), Some((vec!["n1".into()], hours(2))));
    let charged_in_window = sched.timelines[0].charged_at("cpu", hours(2));
    assert_eq!(
        charged_in_window.get("cpu"),
        8,
        "the reservation consumes cap budget"
    );

    let mut sched = BackfillScheduler::new(100);
    let a = fx.schedule(&mut sched, &[job(1, "cpu", 8, 1), job(3, "cpu", 4, 2)]);
    assert_eq!(
        started(&a),
        [3],
        "a job ending by the reserved start may backfill"
    );
    assert_eq!(reserved(&sched, 1).map(|r| r.1), Some(hours(2)));
}

#[test]
fn running_usage_is_charged_until_its_own_job_ends_not_the_node() {
    let fx = Fixture::new(
        vec![node("n1", 64, 256_000, &["cpu", "other"])],
        vec![partition("cpu", &[("cpu", 8)]), partition("other", &[])],
    )
    .running_cpus("cpu", "n1", 4, 1)
    .running_cpus("other", "n1", 16, 24);

    let mut sched = BackfillScheduler::new(100);
    let a = fx.schedule(&mut sched, &[job(1, "cpu", 8, 1), job(2, "cpu", 4, 5)]);
    assert!(a.is_empty(), "the 5h job would overlap the reserved window");
    assert_eq!(reserved(&sched, 1).map(|r| r.1), Some(hours(1)));
}

#[test]
fn a_capacity_wait_is_not_reported_as_a_cap_block() {
    let fx = Fixture::new(
        vec![node("n1", 16, 256_000, &["cpu", "other"])],
        vec![partition("cpu", &[("cpu", 64)]), partition("other", &[])],
    )
    .running_cpus("other", "n1", 16, 2);
    let mut sched = BackfillScheduler::new(100);
    assert!(fx.schedule(&mut sched, &[job(1, "cpu", 8, 1)]).is_empty());
    assert_eq!(reserved(&sched, 1).map(|r| r.1), Some(hours(2)));
    assert!(sched.partition_cap_blocks().is_empty());
}

#[test]
fn a_cap_block_names_only_the_chosen_partition() {
    let mut wide = partition("wide", &[]);
    wide.priority_tier = 2;
    let fx = Fixture::new(
        vec![
            node("n1", 64, 256_000, &["cpu"]),
            node("n2", 64, 256_000, &["wide"]),
        ],
        vec![partition("cpu", &[("cpu", 8)]), wide],
    )
    .running_cpus("cpu", "n1", 8, 3)
    .running_cpus("wide", "n2", 64, 2);
    let mut sched = BackfillScheduler::new(100);
    assert!(fx
        .schedule(&mut sched, &[job(1, "cpu,wide", 8, 1)])
        .is_empty());
    assert_eq!(reserved(&sched, 1), Some((vec!["n2".into()], hours(2))));
    assert!(
        sched.partition_cap_blocks().is_empty(),
        "the job waits for capacity in wide, not for cpu's cap"
    );
}

#[test]
fn multi_partition_job_moves_to_the_partition_that_frees_first() {
    let parts = vec![partition("a", &[("cpu", 8)]), partition("b", &[("cpu", 8)])];
    let nodes = || {
        vec![
            node("n1", 64, 256_000, &["a"]),
            node("n2", 64, 256_000, &["b"]),
        ]
    };
    let pending = [job(1, "a,b", 8, 1)];

    let pass1 = Fixture::new(nodes(), parts.clone())
        .running_cpus("a", "n1", 8, 3)
        .running_cpus("b", "n2", 8, 1);
    let mut sched = BackfillScheduler::new(100);
    assert!(pass1.schedule(&mut sched, &pending).is_empty());
    assert_eq!(reserved(&sched, 1), Some((vec!["n2".into()], hours(1))));

    let pass2 = Fixture::new(nodes(), parts).running_cpus("b", "n2", 8, 1);
    let a = pass2.schedule(&mut sched, &pending);
    assert_eq!(started(&a), [1]);
    assert_eq!(a[0].nodes, ["n1"]);
    assert_eq!(a[0].partition.as_deref(), Some("a"));
}

#[test]
fn feasibility_filter_keeps_a_merely_full_partition_as_candidate() {
    let parts = vec![
        partition("small", &[("cpu", 4)]),
        partition("full", &[("cpu", 8)]),
    ];
    let fx = Fixture::new(
        vec![
            node("n1", 64, 256_000, &["small"]),
            node("n2", 64, 256_000, &["full"]),
        ],
        parts,
    )
    .running_cpus("full", "n2", 8, 2);

    let mut sched = BackfillScheduler::new(100);
    assert!(fx
        .schedule(&mut sched, &[job(1, "small,full", 8, 1)])
        .is_empty());
    assert_eq!(reserved(&sched, 1), Some((vec!["n2".into()], hours(2))));
    let block = &sched.partition_cap_blocks()[&1];
    assert_eq!(block.partition, "full");

    let mut sched = BackfillScheduler::new(100);
    assert!(fx.schedule(&mut sched, &[job(2, "small", 8, 1)]).is_empty());
    assert_eq!(
        outcome(&sched, 2),
        Some(UnplacedKind::PartitionCapNeverFits)
    );
    assert_eq!(reserved(&sched, 2), None);
}

/// `hi` (tier 2) on n1 and `lo` (tier 0) on n2, both capped at 8 CPUs.
fn tiered_fixture() -> Fixture {
    let mut hi = partition("hi", &[("cpu", 8)]);
    hi.priority_tier = 2;
    Fixture::new(
        vec![
            node("n1", 64, 256_000, &["hi"]),
            node("n2", 64, 256_000, &["lo"]),
        ],
        vec![hi, partition("lo", &[("cpu", 8)])],
    )
}

#[test]
fn a_lower_tier_that_can_start_now_beats_a_busy_higher_tier() {
    let fx = tiered_fixture().running_cpus("hi", "n1", 8, 2);
    for list in ["hi,lo", "lo,hi"] {
        let mut sched = BackfillScheduler::new(100);
        let a = fx.schedule(&mut sched, &[job(1, list, 8, 1)]);
        assert_eq!(started(&a), [1], "{list}");
        assert_eq!(a[0].partition.as_deref(), Some("lo"), "{list}");
        assert_eq!(a[0].nodes, ["n2"]);
    }
}

#[test]
fn when_both_can_start_now_the_higher_tier_wins() {
    let fx = tiered_fixture();
    for list in ["hi,lo", "lo,hi"] {
        let mut sched = BackfillScheduler::new(100);
        let a = fx.schedule(&mut sched, &[job(1, list, 8, 1)]);
        assert_eq!(a[0].partition.as_deref(), Some("hi"), "{list}");
        assert_eq!(a[0].nodes, ["n1"]);
    }
}

#[test]
fn when_neither_can_start_now_the_earliest_cap_aware_start_is_reserved() {
    let fx = tiered_fixture()
        .running_cpus("hi", "n1", 8, 3)
        .running_cpus("lo", "n2", 8, 1);
    for list in ["hi,lo", "lo,hi"] {
        let mut sched = BackfillScheduler::new(100);
        assert!(fx.schedule(&mut sched, &[job(1, list, 8, 1)]).is_empty());
        assert_eq!(
            reserved(&sched, 1),
            Some((vec!["n2".into()], hours(1))),
            "{list}"
        );
    }

    let tie = tiered_fixture()
        .running_cpus("hi", "n1", 8, 2)
        .running_cpus("lo", "n2", 8, 2);
    let mut sched = BackfillScheduler::new(100);
    assert!(tie
        .schedule(&mut sched, &[job(1, "lo,hi", 8, 1)])
        .is_empty());
    assert_eq!(
        reserved(&sched, 1),
        Some((vec!["n1".into()], hours(2))),
        "an equal start goes to the higher tier"
    );
}

#[test]
fn same_tier_partitions_ignore_request_order() {
    let same_tier = vec![partition("x", &[("cpu", 8)]), partition("y", &[("cpu", 8)])];
    let fx = Fixture::new(
        vec![
            node("n1", 64, 256_000, &["x"]),
            node("n2", 64, 256_000, &["y"]),
        ],
        same_tier,
    )
    .running_cpus("x", "n1", 8, 2);
    for list in ["x,y", "y,x"] {
        let mut sched = BackfillScheduler::new(100);
        let a = fx.schedule(&mut sched, &[job(1, list, 8, 1)]);
        assert_eq!(
            a[0].partition.as_deref(),
            Some("y"),
            "list order ignored ({list})"
        );
    }
}

#[test]
fn no_partition_requested_resolves_to_the_default_partition() {
    let mut dflt = partition("dflt", &[("cpu", 4)]);
    dflt.is_default = true;
    let fx = Fixture::new(
        vec![
            node("n1", 64, 256_000, &["dflt"]),
            node("n2", 64, 256_000, &["other"]),
        ],
        vec![dflt, partition("other", &[])],
    );

    let mut sched = BackfillScheduler::new(100);
    let a = fx.schedule(&mut sched, &[job(1, "", 4, 1)]);
    assert_eq!(a[0].partition.as_deref(), Some("dflt"));
    assert_eq!(a[0].nodes, ["n1"]);

    let mut sched = BackfillScheduler::new(100);
    assert!(fx.schedule(&mut sched, &[job(2, "", 8, 1)]).is_empty());
    assert_eq!(
        outcome(&sched, 2),
        Some(UnplacedKind::PartitionCapNeverFits)
    );
}

#[test]
fn absent_cap_keeps_placement_unchanged_and_attributes_the_partition() {
    let mut hi = partition("hi", &[]);
    hi.priority_tier = 3;
    let fx = Fixture::new(
        vec![node("n1", 64, 256_000, &["lo", "hi"])],
        vec![partition("lo", &[]), hi],
    );
    let mut sched = BackfillScheduler::new(100);
    let a = fx.schedule(&mut sched, &[job(1, "lo,hi", 64, 1)]);
    assert_eq!(started(&a), [1]);
    assert_eq!(a[0].partition.as_deref(), Some("hi"));
    assert!(sched.partition_cap_blocks().is_empty());
}

#[test]
fn zero_cpu_limit_blocks_every_job_in_the_partition() {
    let fx = Fixture::new(
        vec![node("n1", 64, 256_000, &["off"])],
        vec![partition("off", &[("cpu", 0)])],
    );
    let mut sched = BackfillScheduler::new(100);
    assert!(fx.schedule(&mut sched, &[job(1, "off", 1, 1)]).is_empty());
    assert_eq!(
        outcome(&sched, 1),
        Some(UnplacedKind::PartitionCapNeverFits)
    );
}

#[test]
fn multi_node_job_charges_each_nodes_share_to_its_partition() {
    let fx = Fixture::new(
        vec![
            node("n1", 64, 256_000, &["cpu"]),
            node("n2", 64, 256_000, &["cpu"]),
        ],
        vec![partition("cpu", &[("cpu", 8)])],
    );
    let mut wide = job(1, "cpu", 8, 1);
    wide.spec.num_nodes = 2;
    wide.spec.num_tasks = 2;

    let mut sched = BackfillScheduler::new(100);
    let a = fx.schedule(&mut sched, &[wide, job(2, "cpu", 1, 1)]);
    assert_eq!(started(&a), [1]);
    for tl in &sched.timelines {
        assert_eq!(
            tl.charged_at("cpu", now()).get("cpu"),
            8,
            "{}",
            tl.node_name
        );
    }
    assert_eq!(sched.partition_cap_blocks()[&2].overshoots.len(), 2);
}

#[test]
fn heterogeneous_shares_are_checked_against_the_cap() {
    let uneven = |id| {
        let mut j = job(id, "cpu", 1, 1);
        j.spec.num_nodes = 2;
        j.spec.num_tasks = 7;
        j
    };
    let nodes = || {
        vec![
            node("n1", 64, 256_000, &["cpu"]),
            node("n2", 64, 256_000, &["cpu"]),
        ]
    };

    let tight = Fixture::new(nodes(), vec![partition("cpu", &[("cpu", 3)])]);
    let mut sched = BackfillScheduler::new(100);
    assert!(tight.schedule(&mut sched, &[uneven(1)]).is_empty());
    assert_eq!(
        outcome(&sched, 1),
        Some(UnplacedKind::PartitionCapNeverFits),
        "the busiest node's 4-CPU share can never fit a cap of 3"
    );
    assert_eq!(
        overshoot(&sched, 1).map(|o| (o.resource.as_str(), o.requested)),
        Some(("cpu", 4))
    );

    let fits = Fixture::new(nodes(), vec![partition("cpu", &[("cpu", 4)])]);
    let mut sched = BackfillScheduler::new(100);
    assert_eq!(started(&fits.schedule(&mut sched, &[uneven(2)])), [2]);
}

#[test]
fn scalar_alias_alone_behaves_like_the_cpu_tres_key() {
    let alias = spur_core::config::SlurmConfig::load_from_str(
        r#"
cluster_name = "test"
[[partitions]]
name = "cpu"
nodes = "n1"
max_cpus_per_node = 8
"#,
    )
    .expect("alias config parses")
    .build_partitions();
    let direct = spur_core::config::SlurmConfig::load_from_str(
        r#"
cluster_name = "test"
[[partitions]]
name = "cpu"
nodes = "n1"
[partitions.max_tres_per_node]
cpu = 8
"#,
    )
    .expect("map config parses")
    .build_partitions();
    assert_eq!(alias[0].max_tres_per_node, direct[0].max_tres_per_node);

    let jobs = [
        job(1, "cpu", 8, 1),
        job(2, "cpu", 4, 1),
        job(3, "cpu", 9, 1),
    ];
    let outcomes = |parts: Vec<Partition>| {
        let fx = Fixture::new(vec![node("n1", 64, 256_000, &["cpu"])], parts);
        let mut sched = BackfillScheduler::new(100);
        let a = started(&fx.schedule(&mut sched, &jobs));
        (a, reserved(&sched, 2), outcome(&sched, 3))
    };
    let got = outcomes(alias);
    assert_eq!(got.0, [1]);
    assert_eq!(got, outcomes(direct));
}

#[test]
fn typed_cap_for_a_type_absent_from_the_cluster_is_vacuous() {
    let fx = Fixture::new(
        vec![gpu_node("n1", &["gpu"])],
        vec![partition("gpu", &[("gres/gpu:h100", 1)])],
    );
    let mut sched = BackfillScheduler::new(100);
    let a = fx.schedule(
        &mut sched,
        &[
            gpu_job(1, "gpu", "gpu:mi300x:4"),
            gpu_job(2, "gpu", "gpu:4"),
        ],
    );
    assert_eq!(started(&a), [1, 2]);
}

#[test]
fn running_typed_usage_is_seeded_from_the_usage_map() {
    let mut alloc = ResourceAllocations::with_scalar(1, 0);
    alloc.devices.insert(
        "gpu".into(),
        (0u64..2).map(AllocatedDevice::injectable).collect(),
    );
    let fx = Fixture::new(
        vec![gpu_node("n1", &["gpu"])],
        vec![partition("gpu", &[("gres/gpu:a100", 2)])],
    )
    .running(
        "gpu",
        "n1",
        alloc,
        TresCharge::new(1, 0, 2, Some("a100")),
        hours(2),
    );
    let mut sched = BackfillScheduler::new(100);
    let a = fx.schedule(
        &mut sched,
        &[gpu_job(1, "gpu", "gpu:a100:1"), gpu_job(2, "gpu", "gpu:1")],
    );
    assert_eq!(started(&a), [2]);
    assert_eq!(reserved(&sched, 1).map(|r| r.1), Some(hours(2)));
}

fn per_cpu_limited(name: &str, limit: u64, caps: &[(&str, u64)]) -> Partition {
    Partition {
        max_mem_per_cpu: Some(limit),
        ..partition(name, caps)
    }
}

fn mem_per_cpu_job(id: u32, partitions: &str, cpus: u32, mem_per_cpu: u64) -> Job {
    let mut j = job(id, partitions, cpus, 1);
    j.spec.memory_per_cpu_mb = Some(mem_per_cpu);
    j
}

#[test]
fn each_candidate_partition_plans_its_own_memory_adjusted_request() {
    // -c 2 --mem-per-cpu=3G: 4 CPUs in `big` (2G/CPU limit), 6 in `small`
    // (1G/CPU limit). n2 has only 4 CPUs, so `small` could never hold it.
    let fx = Fixture::new(
        vec![
            node("n1", 8, 64_000, &["big"]),
            node("n2", 4, 64_000, &["small"]),
        ],
        vec![
            per_cpu_limited("big", 2048, &[]),
            per_cpu_limited("small", 1024, &[]),
        ],
    );
    let mut sched = BackfillScheduler::new(100);
    let a = fx.schedule(&mut sched, &[mem_per_cpu_job(1, "small,big", 2, 3072)]);
    assert_eq!(started(&a), [1]);
    assert_eq!(a[0].partition.as_deref(), Some("big"));
    assert_eq!(a[0].per_node_alloc["n1"].cpus, 4);
    let planned = a[0].memory_adjustment.as_ref().unwrap();
    assert_eq!(
        (planned.cpus_per_task, planned.memory_per_cpu_mb),
        (4, Some(1536))
    );
}

#[test]
fn a_single_partition_memory_policy_shapes_the_allocation() {
    let fx = Fixture::new(
        vec![node("n1", 8, 64_000, &["small"])],
        vec![per_cpu_limited("small", 1024, &[])],
    );
    let mut sched = BackfillScheduler::new(100);
    let a = fx.schedule(&mut sched, &[mem_per_cpu_job(1, "small", 2, 3072)]);
    assert_eq!(started(&a), [1]);
    assert_eq!(a[0].per_node_alloc["n1"].cpus, 6);
}

#[test]
fn a_partition_whose_max_mem_per_node_the_job_exceeds_is_not_chosen() {
    let fx = Fixture::new(
        vec![node("n1", 8, 64_000, &["tight", "roomy"])],
        vec![
            Partition {
                max_mem_per_node: Some(1024),
                priority_tier: 5,
                ..partition("tight", &[])
            },
            partition("roomy", &[]),
        ],
    );
    let mut sched = BackfillScheduler::new(100);
    let a = fx.schedule(&mut sched, &[mem_job(1, "tight,roomy", 2048)]);
    assert_eq!(started(&a), [1]);
    assert_eq!(a[0].partition.as_deref(), Some("roomy"));
}

#[test]
fn a_memory_driven_cpu_raise_counts_toward_the_cpu_cap() {
    let fx = Fixture::new(
        vec![node("n1", 16, 64_000, &["cpu"])],
        vec![per_cpu_limited("cpu", 1024, &[("cpu", 4)])],
    );
    let mut sched = BackfillScheduler::new(100);
    let a = fx.schedule(&mut sched, &[mem_per_cpu_job(1, "cpu", 2, 3072)]);
    assert!(a.is_empty());
    assert_eq!(
        outcome(&sched, 1),
        Some(UnplacedKind::PartitionCapNeverFits)
    );
    let o = overshoot(&sched, 1).expect("the cap is reported");
    assert_eq!((o.resource.as_str(), o.requested, o.limit), ("cpu", 6, 4));
}

#[test]
fn array_tasks_are_charged_one_by_one_against_the_cap() {
    let fx = Fixture::new(
        vec![node("n1", 64, 256_000, &["array"])],
        vec![partition("array", &[("cpu", 4)])],
    );
    let tasks: Vec<Job> = (0..10u32)
        .map(|task| {
            let mut j = job(100 + task, "array", 1, 1);
            j.spec.array_job_id = Some(99);
            j.spec.array_task_id = Some(task);
            j
        })
        .collect();

    let mut sched = BackfillScheduler::new(100);
    let a = fx.schedule(&mut sched, &tasks);
    assert_eq!(started(&a), [100, 101, 102, 103]);
    for id in 104..110 {
        let o = overshoot(&sched, id).expect("every waiting task names the cap");
        assert_eq!(
            (o.resource.as_str(), o.used, o.requested, o.limit),
            ("cpu", 4, 1, 4),
            "task {id}"
        );
    }
}

/// `hi` (tier 5, made ineligible by `ineligible`) and `lo` share idle n1;
/// only `lo` also has n2.
fn ineligible_hi_fixture(ineligible: impl FnOnce(&mut Partition)) -> Fixture {
    let mut hi = Partition {
        priority_tier: 5,
        def_mem_per_node: Some(1024),
        ..partition("hi", &[])
    };
    ineligible(&mut hi);
    Fixture::new(
        vec![
            node("n1", 64, 256_000, &["hi", "lo"]),
            node("n2", 64, 256_000, &["lo"]),
        ],
        vec![hi, partition("lo", &[])],
    )
}

#[test]
fn a_partition_that_is_not_up_is_never_chosen() {
    use spur_core::partition::PartitionState;
    for state in [
        PartitionState::Down,
        PartitionState::Drain,
        PartitionState::Inactive,
    ] {
        let fx = ineligible_hi_fixture(|p| p.state = state);
        let mut sched = BackfillScheduler::new(100);
        let a = fx.schedule(&mut sched, &[job(1, "hi,lo", 4, 1)]);
        assert_eq!(started(&a), [1], "{state:?}");
        assert_eq!(a[0].partition.as_deref(), Some("lo"), "{state:?}");
    }
}

#[test]
fn a_candidate_whose_max_time_or_node_bounds_the_job_violates_is_never_chosen() {
    let over_time = ineligible_hi_fixture(|p| p.max_time_minutes = Some(30));
    let too_few_nodes = ineligible_hi_fixture(|p| p.min_nodes = 2);
    for fx in [over_time, too_few_nodes] {
        let mut sched = BackfillScheduler::new(100);
        let a = fx.schedule(&mut sched, &[job(1, "hi,lo", 4, 1)]);
        assert_eq!(started(&a), [1]);
        assert_eq!(a[0].partition.as_deref(), Some("lo"));
    }

    let mut fx = ineligible_hi_fixture(|p| p.max_nodes = Some(1));
    fx.nodes[1].partitions.push("hi".into());
    let mut wide = job(1, "hi,lo", 4, 1);
    wide.spec.num_nodes = 2;
    wide.spec.num_tasks = 2;
    let mut sched = BackfillScheduler::new(100);
    let a = fx.schedule(&mut sched, &[wide]);
    assert_eq!(started(&a), [1]);
    assert_eq!(a[0].partition.as_deref(), Some("lo"));
}

#[test]
fn an_uncapped_job_never_lands_on_a_node_only_a_down_partition_holds() {
    use spur_core::partition::PartitionState;
    let fx = Fixture::new(
        vec![
            node("n1", 64, 256_000, &["down"]),
            node("n2", 64, 256_000, &["up"]),
        ],
        vec![
            Partition {
                state: PartitionState::Down,
                priority_tier: 5,
                ..partition("down", &[])
            },
            partition("up", &[]),
        ],
    )
    .running_cpus("up", "n2", 64, 2);
    let mut sched = BackfillScheduler::new(100);
    let a = fx.schedule(&mut sched, &[job(1, "down,up", 4, 1)]);
    assert!(a.is_empty(), "n1 is only in the Down partition: {a:?}");
    assert_eq!(reserved(&sched, 1), Some((vec!["n2".into()], hours(2))));
}

fn total_gpu_job(id: u32, partition: &str, nodes: u32, gpus: u32, ty: Option<&str>) -> Job {
    let mut j = job(id, partition, 1, 1);
    j.spec.num_nodes = nodes;
    j.spec.num_tasks = nodes;
    j.spec.gpus = Some(spur_core::gpu_request::GpuRequest {
        count: gpus,
        gpu_type: ty.map(str::to_string),
    });
    j
}

fn gpu_counts(assignment: &Assignment) -> Vec<u64> {
    let mut nodes: Vec<_> = assignment.per_node_alloc.keys().collect();
    nodes.sort();
    nodes
        .into_iter()
        .map(|n| assignment.per_node_alloc[n].total_device_count("gpu"))
        .collect()
}

#[test]
fn a_gpu_total_is_spread_within_the_per_node_gpu_cap() {
    let fx = Fixture::new(
        vec![gpu_node("n1", &["gpu"]), gpu_node("n2", &["gpu"])],
        vec![partition("gpu", &[("gres/gpu", 2)])],
    );
    let mut sched = BackfillScheduler::new(100);
    let a = fx.schedule(&mut sched, &[total_gpu_job(1, "gpu", 2, 4, None)]);
    assert_eq!(started(&a), [1], "[2,2] fits the cap");
    assert_eq!(gpu_counts(&a[0]), [2, 2]);
}

#[test]
fn a_gpu_total_the_cap_can_never_spread_over_its_nodes_never_fits() {
    let fx = Fixture::new(
        vec![gpu_node("n1", &["gpu"]), gpu_node("n2", &["gpu"])],
        vec![partition("gpu", &[("gres/gpu", 1)])],
    );
    let mut sched = BackfillScheduler::new(100);
    let a = fx.schedule(&mut sched, &[total_gpu_job(1, "gpu", 2, 4, None)]);
    assert!(a.is_empty());
    assert_eq!(
        outcome(&sched, 1),
        Some(UnplacedKind::PartitionCapNeverFits),
        "every split of 4 GPUs over 2 nodes puts at least 2 on one"
    );
    assert_eq!(overshoot(&sched, 1).map(|o| o.requested), Some(2));
}

#[test]
fn a_gpu_total_is_spread_within_a_typed_caps_running_headroom() {
    let fx = Fixture::new(
        vec![gpu_node("n1", &["gpu"]), gpu_node("n2", &["gpu"])],
        vec![partition("gpu", &[("gres/gpu:a100", 3)])],
    )
    .running(
        "gpu",
        "n1",
        ResourceAllocations::default(),
        TresCharge::new(0, 0, 2, Some("a100")),
        hours(4),
    );
    let mut sched = BackfillScheduler::new(100);
    let a = fx.schedule(&mut sched, &[total_gpu_job(1, "gpu", 2, 4, Some("a100"))]);
    assert_eq!(started(&a), [1], "[1,3] fits the typed headroom");
    assert_eq!(gpu_counts(&a[0]), [1, 3]);
}

#[test]
fn default_memory_per_gpu_follows_each_nodes_gpu_share() {
    let mut small = gpu_node("n2", &["gpu"]);
    small.total_resources.gpus.truncate(1);
    let fx = Fixture::new(
        vec![gpu_node("n1", &["gpu"]), small],
        vec![Partition {
            def_mem_per_gpu: Some(8192),
            ..partition("gpu", &[])
        }],
    );
    let mut sched = BackfillScheduler::new(100);
    let a = fx.schedule(&mut sched, &[total_gpu_job(1, "gpu", 2, 4, None)]);
    assert_eq!(started(&a), [1]);
    assert_eq!(gpu_counts(&a[0]), [3, 1]);
    assert_eq!(a[0].per_node_alloc["n1"].memory_mb, 3 * 8192);
    assert_eq!(a[0].per_node_alloc["n2"].memory_mb, 8192);
}

#[test]
fn a_gpu_total_is_spread_within_max_mem_per_node_at_the_per_gpu_default() {
    let fx = Fixture::new(
        vec![gpu_node("n1", &["gpu"]), gpu_node("n2", &["gpu"])],
        vec![Partition {
            def_mem_per_gpu: Some(8192),
            max_mem_per_node: Some(2 * 8192),
            ..partition("gpu", &[])
        }],
    );
    let mut sched = BackfillScheduler::new(100);
    let a = fx.schedule(&mut sched, &[total_gpu_job(1, "gpu", 2, 4, None)]);
    assert_eq!(started(&a), [1]);
    assert_eq!(gpu_counts(&a[0]), [2, 2]);
}

fn exclusive(mut job: Job) -> Job {
    job.spec.exclusive = true;
    job
}

#[test]
fn an_exclusive_job_charges_the_whole_node_and_fits_a_cap_that_covers_it() {
    let fx = |cap| {
        Fixture::new(
            vec![node("n1", 16, 64_000, &["cpu"])],
            vec![partition("cpu", &[("cpu", cap)])],
        )
    };

    let mut sched = BackfillScheduler::new(100);
    let a = fx(16).schedule(&mut sched, &[exclusive(job(1, "cpu", 1, 1))]);
    assert_eq!(started(&a), [1]);

    let mut sched = BackfillScheduler::new(100);
    assert!(fx(15)
        .schedule(&mut sched, &[exclusive(job(1, "cpu", 1, 1))])
        .is_empty());
    assert_eq!(
        outcome(&sched, 1),
        Some(UnplacedKind::PartitionCapNeverFits)
    );
}

#[test]
fn an_exclusive_job_waits_until_its_partitions_charge_on_the_node_ends() {
    let fx = Fixture::new(
        vec![node("n1", 16, 64_000, &["cpu"])],
        vec![partition("cpu", &[("cpu", 16)])],
    )
    .running(
        "cpu",
        "n1",
        ResourceAllocations::default(),
        TresCharge::new(4, 0, 0, None),
        hours(2),
    );
    let mut sched = BackfillScheduler::new(100);
    let a = fx.schedule(&mut sched, &[exclusive(job(1, "cpu", 1, 1))]);

    assert!(a.is_empty());
    assert_eq!(overshoot(&sched, 1).map(|o| o.used), Some(4));
    assert_eq!(reserved(&sched, 1), Some((vec!["n1".into()], hours(2))));
}

#[test]
fn a_typed_exclusive_job_charges_every_gpu_on_a_mixed_node_to_its_type() {
    let job = || exclusive(gpu_job(1, "gpu", "gpu:a100:1"));
    let fx = |cap| {
        Fixture::new(
            vec![gpu_node("n1", &["gpu"])],
            vec![partition("gpu", &[("gres/gpu:a100", cap)])],
        )
    };

    let mut sched = BackfillScheduler::new(100);
    assert!(fx(4).schedule(&mut sched, &[job()]).is_empty());
    assert_eq!(
        outcome(&sched, 1),
        Some(UnplacedKind::PartitionCapNeverFits)
    );

    let mut sched = BackfillScheduler::new(100);
    assert_eq!(started(&fx(8).schedule(&mut sched, &[job()])), [1]);
}

#[test]
fn eviction_counts_a_node_only_if_the_gpu_totals_busiest_share_fits() {
    use crate::partition_cap::cap_admits_on;
    let n1 = gpu_node("n1", &["gpu"]);
    let partitions = [partition("gpu", &[("gres/gpu", 4)])];
    let held = |gpus| move |_: &Partition| TresCharge::new(0, 0, gpus, None);

    let job = total_gpu_job(1, "gpu", 2, 8, None);
    assert!(
        !cap_admits_on(&job, &partitions, &n1, held(2)),
        "8 GPUs over 2 nodes needs 4 on n1; 2 held + 4 > 4"
    );
    assert!(cap_admits_on(&job, &partitions, &n1, held(0)));
}

#[test]
fn eviction_ignores_a_partition_that_does_not_admit_the_job() {
    use crate::partition_cap::cap_admits_on;
    use spur_core::partition::PartitionState;
    let n1 = node("n1", 64, 256_000, &["cpu", "down"]);
    let partitions = [
        partition("cpu", &[("cpu", 4)]),
        Partition {
            state: PartitionState::Down,
            ..partition("down", &[])
        },
    ];
    let mut job = job(1, "cpu,down", 2, 1);
    job.spec.num_tasks = 1;
    let full = |p: &Partition| TresCharge::new(if p.name == "cpu" { 4 } else { 0 }, 0, 0, None);

    assert!(
        !cap_admits_on(&job, &partitions, &n1, full),
        "an uncapped but Down partition cannot run the job, so the full cap still blocks it"
    );
}
