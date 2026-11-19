// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The controller admits a job by counts alone (`Node::can_satisfy_request`);
//! the agent's ledger then has to find real CPUs for it. These tests drive both
//! through every short job sequence on small node shapes and check they agree.

use std::collections::{BTreeMap, HashSet};

use spur_core::core_layout::{CoreLayout, NodeCoreSettings};
use spur_core::cpu_topology::{CpuRecord, CpuTopology};
use spur_core::node::Node;
use spur_core::resource::{
    build_node_allocation, GpuLinkType, GpuResource, ResourceAllocations, ResourceSet,
};
use spur_sched::cons_tres::{AllocError, CapacityChange, NodeAllocation};

const CORES_PER_SOCKET: u32 = 4;
/// Probes free CPUs; never a real job id.
const NOBODY: u32 = u32::MAX;

#[derive(Clone, Copy, Debug)]
enum Shape {
    Cpu(u32),
    /// GPUs, then CPUs as a multiple of one GPU's reserved CPUs plus an offset.
    Gpu(u32, u32, u32),
}

const SHAPES: [Shape; 8] = [
    Shape::Cpu(1),
    Shape::Cpu(2),
    Shape::Cpu(4),
    Shape::Gpu(1, 0, 1),
    Shape::Gpu(1, 1, 0),
    Shape::Gpu(1, 1, 1),
    Shape::Gpu(2, 2, 0),
    Shape::Gpu(2, 2, 2),
];

#[derive(Clone, Copy, Debug)]
enum Change {
    RUp,
    RDown,
    SpecUp,
    SpecDown,
    /// Drops the highest-id GPU no job holds.
    DropGpu,
    AddGpu,
}

#[derive(Clone, Copy, Debug)]
enum Event {
    Start(Shape),
    FinishOldest,
    RestartAgent,
    ChangeLayout(Change),
    Heartbeat,
}

#[derive(Clone)]
struct Running {
    id: u32,
    alloc: ResourceAllocations,
    cpu_ids: Vec<u32>,
}

/// One node as both the controller and the agent see it.
#[derive(Clone)]
struct Bed {
    node: Node,
    layout: CoreLayout,
    resources: ResourceSet,
    ledger: NodeAllocation,
    running: Vec<Running>,
    next_id: u32,
    /// The settings the current layout was computed from.
    settings: NodeCoreSettings,
    /// A heartbeat reported blocking since the last layout change.
    reported: bool,
}

fn topology(sockets: u32, smt: bool) -> CpuTopology {
    let threads = if smt { 2 } else { 1 };
    let per_thread = sockets * CORES_PER_SOCKET;
    let mut records = Vec::new();
    for socket in 0..sockets {
        for core in 0..CORES_PER_SOCKET {
            for thread in 0..threads {
                records.push(CpuRecord {
                    cpu: socket * CORES_PER_SOCKET + core + thread * per_thread,
                    socket,
                    core_id: core,
                    numa_node: Some(socket),
                });
            }
        }
    }
    CpuTopology::from_cpus(&records)
}

fn gpus(count: u32, sockets: u32) -> Vec<GpuResource> {
    (0..count)
        .map(|i| GpuResource {
            device_id: i,
            gpu_type: "mi300x".into(),
            memory_mb: 0,
            peer_gpus: vec![],
            link_type: GpuLinkType::XGMI,
            topology: None,
            stable_id: u64::from(i) + 1,
            numa_node: Some(i % sockets),
            affinity_cpus: vec![],
        })
        .collect()
}

impl Bed {
    /// `None` when the shape cannot hold the layout (too few cores per NUMA).
    fn new(sockets: u32, smt: bool, gpu_count: u32, r: u32, spec: u32) -> Option<Self> {
        let topology = topology(sockets, smt);
        let gpus = gpus(gpu_count, sockets);
        let settings = NodeCoreSettings {
            core_spec_count: (spec > 0).then_some(spec),
            restricted_cores_per_gpu: Some(r),
            ..Default::default()
        };
        let layout = CoreLayout::compute(Some(&topology), &settings, &gpus, 0, &[]).ok()??;
        let resources = ResourceSet {
            cpus: topology.online_cpus().len() as u32,
            memory_mb: 1024,
            gpus,
            topology: Some(topology),
            ..Default::default()
        };
        let mut total = resources.clone();
        total.cpus -= layout.spec_cpus.len() as u32;
        total.topology = None;
        let mut node = Node::new("n1".into(), total);
        node.core_layout = Some(layout.clone());
        node.reserved_picks = layout.gpu_reserved.clone();
        node.sync_reserved_tag();
        let mut bed = Bed {
            ledger: NodeAllocation::new("n1".into(), &resources),
            node,
            layout,
            resources,
            running: Vec::new(),
            next_id: 1,
            settings,
            reported: false,
        };
        bed.apply_layout_to_ledger();
        Some(bed)
    }

    fn apply_layout_to_ledger(&mut self) {
        self.ledger.set_spec_exclusions(&self.layout.spec_cpus);
        self.ledger.set_gpu_reserved(&self.layout.gpu_reserved);
    }

    fn reserved_cpus_per_gpu(&self) -> u32 {
        self.node.cpu_pools().map_or(0, |p| p.reserved_per_gpu)
    }

    /// The job is placed as the scheduler does it: counts, then a stamped allocation.
    fn request(&self, shape: Shape) -> ResourceSet {
        let rc = self.reserved_cpus_per_gpu();
        let (gpus, cpus) = match shape {
            Shape::Cpu(n) => (0, n),
            Shape::Gpu(g, mult, extra) => (g, mult * rc + extra),
        };
        let mut gpu_list = self.node.total_resources.gpus.clone();
        gpu_list.truncate(gpus as usize);
        for gpu in &mut gpu_list {
            gpu.gpu_type = "any".into();
        }
        ResourceSet {
            cpus,
            gpus: gpu_list,
            ..Default::default()
        }
    }

    fn start(&mut self, shape: Shape) -> Result<(), String> {
        let request = self.request(shape);
        if request.gpus.len() > self.node.total_resources.gpus.len()
            || !self.node.can_satisfy_request(&request)
        {
            return Ok(());
        }
        let mut used = self.node.alloc_resources.clone();
        if let Some(blocked) = self.node.layout_blocked_alloc() {
            used.add(&blocked);
        }
        let mut alloc = build_node_allocation(&self.node.total_resources, &used, &request);
        self.layout.stamp(&mut alloc);
        let gpu_ids = alloc.device_ids("gpu");
        let id = self.next_id;
        self.next_id += 1;
        match self.ledger.allocate_for_job(id, 1, alloc.cpus, 0, &gpu_ids) {
            Ok(result) => {
                self.check_pick(&alloc, &gpu_ids, &result.cpu_ids)?;
                self.ledger.commit_job(id, 1);
                self.node.hold(&alloc);
                self.running.push(Running {
                    id,
                    alloc,
                    cpu_ids: result.cpu_ids,
                });
                Ok(())
            }
            Err(AllocError::ReservedCpusHeld) if !self.reported && self.blocks_any(&gpu_ids) => {
                Ok(())
            }
            Err(e) => Err(format!("admitted {shape:?} but the ledger said {e:?}")),
        }
    }

    fn check_pick(
        &self,
        alloc: &ResourceAllocations,
        gpu_ids: &[u64],
        picked: &[u32],
    ) -> Result<(), String> {
        let spec: HashSet<u32> = self.layout.spec_cpus.iter().copied().collect();
        let own: HashSet<u32> = self.reserved_of(|id| gpu_ids.contains(&id));
        let others: HashSet<u32> = self.reserved_of(|id| !gpu_ids.contains(&id));
        if picked.len() != alloc.cpus as usize {
            return Err(format!("wanted {} CPUs, got {picked:?}", alloc.cpus));
        }
        if let Some(cpu) = picked.iter().find(|c| spec.contains(c)) {
            return Err(format!("handed out spec CPU {cpu}"));
        }
        if let Some(cpu) = picked.iter().find(|c| others.contains(c)) {
            return Err(format!("handed out CPU {cpu} reserved for another GPU"));
        }
        let from_own = picked.iter().filter(|c| own.contains(c)).count() as u32;
        if from_own != alloc.reserved_cpus {
            return Err(format!(
                "{from_own} CPUs from its GPUs' reserved cores, controller charged {}",
                alloc.reserved_cpus
            ));
        }
        Ok(())
    }

    fn reserved_of(&self, pick: impl Fn(u64) -> bool) -> HashSet<u32> {
        self.layout
            .gpu_reserved
            .iter()
            .filter(|g| pick(g.stable_id))
            .flat_map(|g| g.cpus.iter().copied())
            .collect()
    }

    /// Until a heartbeat reports it, a job placed under an older layout (or
    /// before any) may sit on a GPU's reserved CPUs and refuse that GPU's job.
    fn blocks_any(&self, gpu_ids: &[u64]) -> bool {
        self.ledger
            .blocked_reserved_cpus(&self.layout.gpu_reserved)
            .keys()
            .any(|id| gpu_ids.contains(id))
    }

    /// Recompute the layout from the node's previous picks, as the controller
    /// does; running jobs keep their CPUs, as on the agent. A layout that
    /// cannot be computed keeps the last good one, as the controller does.
    fn change_layout(&mut self, change: Change) -> Result<(), String> {
        let mut settings = self.settings.clone();
        let mut gpus = self.resources.gpus.clone();
        let r = settings.restricted_cores_per_gpu.unwrap_or(0);
        let spec = settings.core_spec_count.unwrap_or(0);
        match change {
            Change::RUp => settings.restricted_cores_per_gpu = Some(r + 1),
            Change::RDown if r > 1 => settings.restricted_cores_per_gpu = Some(r - 1),
            Change::SpecUp => settings.core_spec_count = Some(spec + 1),
            Change::SpecDown if spec > 0 => {
                settings.core_spec_count = (spec > 1).then_some(spec - 1)
            }
            Change::DropGpu if gpus.len() > 1 => {
                let held: HashSet<u64> = self
                    .running
                    .iter()
                    .flat_map(|job| job.alloc.device_ids("gpu"))
                    .collect();
                let Some(pos) = gpus
                    .iter()
                    .enumerate()
                    .filter(|(_, g)| !held.contains(&g.stable_id))
                    .max_by_key(|(_, g)| g.stable_id)
                    .map(|(i, _)| i)
                else {
                    return Ok(());
                };
                gpus.remove(pos);
            }
            Change::AddGpu if gpus.len() < 4 => {
                let sockets = self
                    .resources
                    .topology
                    .as_ref()
                    .and_then(|t| t.cores.iter().map(|c| c.socket).max())
                    .map_or(1, |s| s + 1);
                let id = gpus.iter().map(|g| g.stable_id).max().unwrap_or(0) + 1;
                let mut gpu = gpus[0].clone();
                gpu.stable_id = id;
                gpu.device_id = (id - 1) as u32;
                gpu.numa_node = Some((id - 1) as u32 % sockets);
                gpus.push(gpu);
            }
            _ => return Ok(()),
        }
        let Ok(Some(layout)) = CoreLayout::compute(
            self.resources.topology.as_ref(),
            &settings,
            &gpus,
            0,
            &self.node.reserved_picks,
        ) else {
            return Ok(());
        };
        check_sticky(&self.layout, &layout)?;
        if matches!(change, Change::DropGpu | Change::AddGpu) {
            self.resources.gpus = gpus.clone();
            if let CapacityChange::AllocatedDevicesLost(lost) =
                self.ledger.update_capacity(&self.resources)
            {
                return Err(format!("dropped a held GPU: {lost:?}"));
            }
            self.node.total_resources.gpus = gpus;
        }
        self.node.total_resources.cpus = self.resources.cpus - layout.spec_cpus.len() as u32;
        self.node.core_layout = Some(layout.clone());
        self.node.reserved_picks = layout.gpu_reserved.clone();
        self.node.sync_reserved_tag();
        self.node.layout_blocked_cpus.clear();
        self.layout = layout;
        self.settings = settings;
        self.reported = false;
        self.apply_layout_to_ledger();
        Ok(())
    }

    /// What the agent reports: per GPU, its reserved CPUs held by a job not
    /// holding it, from the ledger.
    fn heartbeat(&mut self) {
        self.node.layout_blocked_cpus =
            self.ledger.blocked_reserved_cpus(&self.layout.gpu_reserved);
        self.reported = true;
    }

    fn finish_oldest(&mut self) {
        if self.running.is_empty() {
            return;
        }
        let done = self.running.remove(0);
        self.node.release(&done.alloc);
        self.ledger.release_job(done.id);
    }

    fn restart_agent(&mut self) -> Result<(), String> {
        self.ledger = NodeAllocation::new("n1".into(), &self.resources);
        self.apply_layout_to_ledger();
        for job in &self.running {
            let gpu_ids = job.alloc.device_ids("gpu");
            self.ledger
                .restore_for_job(job.id, 1, &job.cpu_ids, 0, &gpu_ids)
                .map_err(|e| format!("replay of job {} failed: {e:?}", job.id))?;
        }
        Ok(())
    }

    /// The controller must never think more general CPUs are free than are.
    fn check_general_counts(&self) -> Result<(), String> {
        let general = self.node.cpu_pools().map_or(0, |p| p.general);
        let used = &self.node.alloc_resources;
        let charged = used
            .cpus
            .saturating_sub(used.reserved_cpus.min(self.reserved_total()));
        let believed_free = general.saturating_sub(charged);
        let actually_free = self
            .ledger
            .claimable_cpu_ids(NOBODY, &self.layout.general_cpus)
            .len() as u32;
        if believed_free > actually_free {
            return Err(format!(
                "controller sees {believed_free} general CPUs free, only {actually_free} are"
            ));
        }
        Ok(())
    }

    fn reserved_total(&self) -> u32 {
        self.node.cpu_pools().map_or(0, |p| p.reserved_total)
    }

    fn run(&mut self, events: &[Event]) -> Result<(), String> {
        for event in events {
            match *event {
                Event::Start(shape) => self.start(shape)?,
                Event::FinishOldest => self.finish_oldest(),
                Event::RestartAgent => self.restart_agent()?,
                Event::ChangeLayout(change) => self.change_layout(change)?,
                Event::Heartbeat => self.heartbeat(),
            }
            self.check_general_counts()?;
        }
        Ok(())
    }

    /// A CPU-only job placed first-fit by an agent that did not yet know the
    /// layout, charged as general with no layout tag.
    fn place_legacy_job(&mut self, cpus: u32) {
        let id = self.next_id;
        self.next_id += 1;
        self.ledger.set_spec_exclusions(&[]);
        self.ledger.set_gpu_reserved(&[]);
        let Ok(result) = self.ledger.allocate_for_job(id, 1, cpus, 0, &[]) else {
            return;
        };
        self.ledger.commit_job(id, 1);
        self.apply_layout_to_ledger();
        let alloc = ResourceAllocations::with_scalar(cpus, 0);
        self.node.hold(&alloc);
        self.running.push(Running {
            id,
            alloc,
            cpu_ids: result.cpu_ids,
        });
    }
}

/// A GPU present before and after keeps every pick that did not become spec,
/// and a smaller R keeps only previous picks.
fn check_sticky(before: &CoreLayout, after: &CoreLayout) -> Result<(), String> {
    let spec: HashSet<u32> = after.spec_cpus.iter().copied().collect();
    for old in &before.gpu_reserved {
        let Some(new) = after
            .gpu_reserved
            .iter()
            .find(|g| g.stable_id == old.stable_id)
        else {
            continue;
        };
        let sticky = if new.cpus.len() >= old.cpus.len() {
            old.cpus
                .iter()
                .filter(|c| !spec.contains(c))
                .all(|c| new.cpus.contains(c))
        } else {
            new.cpus.iter().all(|c| old.cpus.contains(c))
        };
        if !sticky {
            return Err(format!(
                "GPU {} moved from {:?} to {:?}",
                old.stable_id, old.cpus, new.cpus
            ));
        }
    }
    Ok(())
}

fn node_shapes() -> impl Iterator<Item = (u32, bool, u32, u32, u32)> {
    let mut shapes = Vec::new();
    for sockets in 1..=2 {
        for smt in [false, true] {
            for gpus in 1..=4 {
                for r in 1..=3 {
                    for spec in 0..=1 {
                        shapes.push((sockets, smt, gpus, r, spec));
                    }
                }
            }
        }
    }
    shapes.into_iter()
}

fn alphabet() -> Vec<Event> {
    SHAPES
        .iter()
        .map(|s| Event::Start(*s))
        .chain([Event::FinishOldest, Event::RestartAgent])
        .collect()
}

fn layout_alphabet() -> Vec<Event> {
    [
        Event::Start(Shape::Cpu(2)),
        Event::Start(Shape::Gpu(1, 1, 0)),
        Event::Start(Shape::Gpu(1, 1, 1)),
        Event::FinishOldest,
        Event::Heartbeat,
    ]
    .into_iter()
    .chain(
        [
            Change::RUp,
            Change::RDown,
            Change::SpecUp,
            Change::SpecDown,
            Change::DropGpu,
            Change::AddGpu,
        ]
        .map(Event::ChangeLayout),
    )
    .collect()
}

/// Run every sequence of up to `depth` events from `bed`, sharing prefixes.
fn explore(
    bed: &Bed,
    trail: &mut Vec<Event>,
    depth: usize,
    alphabet: &[Event],
) -> Result<(), String> {
    if depth == 0 {
        return Ok(());
    }
    for &event in alphabet {
        let mut next = bed.clone();
        trail.push(event);
        next.run(&[event]).map_err(|e| format!("{trail:?}: {e}"))?;
        explore(&next, trail, depth - 1, alphabet)?;
        trail.pop();
    }
    Ok(())
}

#[test]
fn controller_admits_implies_agent_picks() {
    let mut beds = 0;
    for (sockets, smt, gpus, r, spec) in node_shapes() {
        let Some(bed) = Bed::new(sockets, smt, gpus, r, spec) else {
            continue;
        };
        beds += 1;
        if let Err(e) = explore(&bed, &mut Vec::new(), 3, &alphabet()) {
            panic!("sockets={sockets} smt={smt} gpus={gpus} r={r} spec={spec} {e}");
        }
    }
    assert!(
        beds > 20,
        "too few valid node shapes ({beds}) to mean anything"
    );
}

#[test]
fn jobs_from_before_the_layout_only_ever_delay_a_gpu_job() {
    for (sockets, smt, gpus, r, spec) in node_shapes() {
        for legacy_cpus in [1, 3] {
            let Some(mut bed) = Bed::new(sockets, smt, gpus, r, spec) else {
                break;
            };
            bed.place_legacy_job(legacy_cpus);
            if let Err(e) = explore(&bed, &mut Vec::new(), 2, &alphabet()) {
                panic!(
                    "legacy={legacy_cpus} sockets={sockets} smt={smt} gpus={gpus} r={r} \
                     spec={spec} {e}"
                );
            }
        }
    }
}

/// A layout change makes CPUs a legacy CPU-only job holds into a GPU's
/// reserved cores: the job waits for the GPU, not the general pool.
#[test]
fn a_gpu_job_waits_for_a_legacy_job_on_its_reserved_cpus_instead_of_spilling() {
    let mut bed = Bed::new(1, false, 1, 2, 0).expect("1 socket, 4 cores, 1 GPU x 2");
    assert_eq!(bed.layout.gpu_reserved[0].cpus, vec![0, 1]);
    bed.place_legacy_job(2);
    assert_eq!(bed.running[0].cpu_ids, vec![0, 1]);
    bed.check_general_counts()
        .expect("legacy job charged as general");

    let request = bed.request(Shape::Gpu(1, 1, 0));
    assert!(
        bed.node.can_satisfy_request(&request),
        "controller admits it"
    );
    let mut alloc = build_node_allocation(
        &bed.node.total_resources,
        &bed.node.alloc_resources,
        &request,
    );
    bed.layout.stamp(&mut alloc);
    assert_eq!(alloc.reserved_cpus, 2);
    assert_eq!(
        bed.ledger.allocate_for_job(99, 1, 2, 0, &[1]),
        Err(AllocError::ReservedCpusHeld),
        "requeues rather than spilling into general"
    );
    bed.check_general_counts().expect("nothing spilled");

    bed.finish_oldest();
    let picked = bed
        .ledger
        .allocate_for_job(99, 1, 2, 0, &[1])
        .expect("retry fits");
    assert_eq!(picked.cpu_ids, vec![0, 1]);
    bed.node.alloc_resources.add(&alloc);
    bed.check_general_counts()
        .expect("counts match after the retry");
    let free_general = bed
        .ledger
        .claimable_cpu_ids(NOBODY, &bed.layout.general_cpus)
        .len() as u32;
    assert_eq!(
        free_general, 2,
        "both general CPUs are free and counted free"
    );
}

/// A layout change turns CPUs a CPU-only job holds into a GPU's reserved
/// cores: once the agent reports it, the GPU job waits instead of being refused.
#[test]
fn after_a_layout_change_a_gpu_job_waits_for_the_reported_blocker() {
    let mut bed = Bed::new(1, false, 1, 1, 0).expect("1 socket, 4 cores, 1 GPU x 1");
    assert_eq!(bed.layout.gpu_reserved[0].cpus, vec![0]);
    bed.start(Shape::Cpu(3)).unwrap();
    assert_eq!(bed.running[0].cpu_ids, vec![1, 2, 3]);

    bed.change_layout(Change::RUp).unwrap();
    assert_eq!(
        bed.layout.gpu_reserved[0].cpus,
        vec![0, 1],
        "kept core 0, took core 1"
    );
    bed.heartbeat();
    assert_eq!(bed.node.layout_blocked_cpus, BTreeMap::from([(1, 1)]));
    let request = bed.request(Shape::Gpu(1, 1, 0));
    assert!(
        !bed.node.can_satisfy_request(&request),
        "waits, never refused"
    );

    bed.finish_oldest();
    bed.heartbeat();
    assert!(bed.node.layout_blocked_cpus.is_empty());
    bed.start(Shape::Gpu(1, 1, 0)).unwrap();
    assert_eq!(bed.running[0].cpu_ids, vec![0, 1]);
}

/// Across layout changes: general counts stay honest, picks stay sticky, and
/// once a heartbeat has reported blocking no admitted job is refused.
#[test]
fn layout_changes_never_refuse_a_job_admitted_after_a_heartbeat() {
    let alphabet = layout_alphabet();
    let mut beds = 0;
    for (sockets, smt, gpus, r, spec) in node_shapes().filter(|&(_, _, g, r, _)| g <= 2 && r <= 2) {
        let Some(mut bed) = Bed::new(sockets, smt, gpus, r, spec) else {
            continue;
        };
        beds += 1;
        // A CPU-only job from before any change, for later layouts to land on.
        bed.start(Shape::Cpu(2))
            .unwrap_or_else(|e| panic!("seed on sockets={sockets} smt={smt}: {e}"));
        if let Err(e) = explore(&bed, &mut Vec::new(), 4, &alphabet) {
            panic!("sockets={sockets} smt={smt} gpus={gpus} r={r} spec={spec} {e}");
        }
    }
    assert!(
        beds > 10,
        "too few valid node shapes ({beds}) to mean anything"
    );
}
