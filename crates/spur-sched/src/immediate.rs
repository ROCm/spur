// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Bounded inputs for immediate, read-only placement trials.

use std::collections::{HashMap, HashSet};

use spur_core::job::{Job, JobSpec};
use spur_core::node::{Node, NodeSource};
use spur_core::resource::{ResourceAllocations, ResourceSet};
use spur_core::script_placement::ResolvedPlacement;

use crate::node_match::NodePlacement;

pub const MAX_NODES: usize = 4096;
pub const MAX_VICTIMS: usize = 1024;
pub const MAX_ENTRIES: usize = (MAX_NODES + MAX_VICTIMS) * 64;
pub const MAX_TEXT: usize = (MAX_NODES + MAX_VICTIMS) * 1024;
pub const MAX_TRIALS: usize = 2 * MAX_VICTIMS + 16;
pub const MAX_WORK: usize = 2_000_000;
pub const MAX_HOSTS: usize = 8192;
pub const MAX_NAME: usize = 255;

pub fn job_work(job: &Job) -> Option<usize> {
    (job.spec.num_nodes as usize)
        .checked_add(job.spec.gres.len())?
        .checked_add(job.preferred_nodes.len())?
        .checked_add(1)?
        .checked_mul(8)
}

/// Shared across requests within one reclaim or QOS preemption pass.
#[derive(Debug)]
pub struct SearchBudget {
    pub calls_left: usize,
    pub work_left: usize,
}

impl Default for SearchBudget {
    fn default() -> Self {
        Self {
            calls_left: MAX_TRIALS,
            work_left: MAX_WORK,
        }
    }
}

impl SearchBudget {
    pub fn spend(&mut self, work: usize) -> Option<()> {
        self.work_left = self.work_left.checked_sub(work)?;
        Some(())
    }

    pub fn trial(&mut self, work: usize) -> Option<()> {
        self.calls_left = self.calls_left.checked_sub(1)?;
        self.spend(work)
    }
}

/// Structural limits for copied placement metadata; not an allocator-size estimate.
#[derive(Default)]
pub struct InputSize {
    entries: usize,
    text: usize,
}

impl InputSize {
    pub fn entries(&mut self, count: usize) -> Option<()> {
        self.entries = self.entries.checked_add(count)?;
        (self.entries <= MAX_ENTRIES).then_some(())
    }

    pub fn text(&mut self, value: &str) -> Option<()> {
        self.text_bytes(value.len())
    }

    fn text_bytes(&mut self, len: usize) -> Option<()> {
        self.text = self.text.checked_add(len)?;
        (self.text <= MAX_TEXT).then_some(())
    }

    pub fn strings<'a>(&mut self, values: impl IntoIterator<Item = &'a String>) -> Option<()> {
        for value in values {
            self.entries(1)?;
            self.text(value)?;
        }
        Some(())
    }

    pub fn allocation(&mut self, alloc: &ResourceAllocations) -> Option<()> {
        self.entries(1)?;
        for (name, devices) in &alloc.devices {
            self.text(name)?;
            self.entries(devices.len().checked_add(1)?)?;
            if name == "gpu" && devices.iter().any(|device| device.count != 1) {
                return None;
            }
            devices
                .iter()
                .try_fold(0u64, |total, device| total.checked_add(device.count))?;
        }
        Some(())
    }

    pub fn inventory(&mut self, inventory: &ResourceSet) -> Option<()> {
        self.entries(inventory.gpus.len().checked_add(1)?)?;
        for gpu in &inventory.gpus {
            self.text(&gpu.gpu_type)?;
        }
        self.strings(inventory.generic.keys())
    }

    pub fn node(&mut self, node: &Node) -> Option<()> {
        self.entries(6)?;
        if let Some(layout) = &node.core_layout {
            self.entries(layout.spec_cpus.len())?;
            self.entries(layout.general_cpus.len())?;
            self.entries(layout.trimmed_cpus.len())?;
            self.entries(layout.gpu_reserved.len())?;
            for gpu in &layout.gpu_reserved {
                self.entries(gpu.cpus.len())?;
            }
        }
        self.entries(node.layout_blocked_cpus.len())?;
        self.text(&node.name)?;
        self.strings(&node.partitions)?;
        self.strings(&node.features)?;
        self.strings(&node.capabilities)?;
        if let NodeSource::Kubernetes { namespace } = &node.source {
            self.text(namespace)?;
        }
        self.strings(
            [&node.layout_error, &node.layout_enforce_error]
                .into_iter()
                .flatten(),
        )?;
        self.inventory(&node.total_resources)?;
        self.allocation(&node.alloc_resources)
    }

    pub fn job(&mut self, job: &Job) -> Option<()> {
        self.entries(8)?;
        let spec = &job.spec;
        self.text(&spec.user)?;
        self.strings(
            [
                &spec.partition,
                &spec.account,
                &spec.qos,
                &spec.reservation,
                &spec.nodelist,
                &spec.exclude,
                &spec.constraint,
                &spec.distribution,
                &spec.topology,
                &spec.mpi,
            ]
            .into_iter()
            .flatten(),
        )?;
        self.strings(&spec.gres)?;
        self.strings(&job.preferred_nodes)?;
        for request in [&spec.gpus, &spec.gpus_per_node, &spec.gpus_per_task]
            .into_iter()
            .flatten()
        {
            if let Some(name) = &request.gpu_type {
                self.text(name)?;
            }
        }
        if spec.num_nodes as usize > MAX_NODES {
            return None;
        }
        for request in [&spec.gpus, &spec.gpus_per_node, &spec.gpus_per_task]
            .into_iter()
            .flatten()
        {
            if request.count as usize > MAX_ENTRIES {
                return None;
            }
        }
        if spec.gpus_per_task.as_ref().is_some_and(|request| {
            u64::from(request.count) * u64::from(spec.tasks_per_node.unwrap_or(spec.num_tasks))
                > MAX_ENTRIES as u64
        }) {
            return None;
        }
        let mut gres_gpus = 0u64;
        for gres in &spec.gres {
            if let Some((name, _, count)) = spur_core::resource::parse_gres(gres) {
                if name == "gpu" {
                    gres_gpus = gres_gpus.checked_add(u64::from(count))?;
                }
            }
        }
        if gres_gpus > MAX_ENTRIES as u64 {
            return None;
        }
        let demand = spur_core::gpu_request::resolve_gpu_demand(spec).ok()?;
        let type_len = demand.gpu_type().unwrap_or("any").len();
        let largest = match demand {
            spur_core::gpu_request::GpuDemand::None => 0,
            spur_core::gpu_request::GpuDemand::Total { count, .. } => count,
            spur_core::gpu_request::GpuDemand::PerNode { counts, .. } => {
                counts.into_iter().max().unwrap_or(0)
            }
        };
        self.entries(largest as usize)?;
        // Capacity probes copy the type into each placeholder before rejecting a node.
        self.text_bytes((largest as usize).checked_mul(type_len)?)
    }
}

fn canonicalize_hostlists(
    job: &mut Job,
    size: &mut InputSize,
    budget: &mut SearchBudget,
) -> Option<()> {
    for pattern in [&mut job.spec.nodelist, &mut job.spec.exclude] {
        if let Some(value) = pattern.as_deref().filter(|v| !v.is_empty()) {
            let names = spur_core::hostlist::expand_bounded(value, MAX_HOSTS, MAX_NAME).ok()?;
            let before = size.entries.checked_add(size.text)?;
            size.strings(&names)?;
            budget.spend(size.entries.checked_add(size.text)?.checked_sub(before)?)?;
            *pattern = Some(names.join(","));
        }
    }
    Some(())
}

/// Static placement scope, independent of resources that a victim could release.
pub struct PlacementScope {
    nodes: HashSet<String>,
    scan_work: usize,
}

impl PlacementScope {
    pub fn new(job: &Job, nodes: &[Node], budget: &mut SearchBudget) -> Option<Self> {
        if nodes.len() > MAX_NODES {
            return None;
        }
        budget.spend(job_work(job)?)?;
        let mut size = InputSize::default();
        size.job(job)?;
        for node in nodes {
            budget.spend(1)?;
            size.text(&node.name)?;
            size.strings(&node.partitions)?;
            size.strings(&node.features)?;
        }
        budget.spend(size.entries.checked_add(size.text)?)?;
        let mut job = placement_job(job);
        canonicalize_hostlists(&mut job, &mut size, budget)?;
        Self::from_prepared(&job, nodes, budget)
    }

    fn from_prepared(job: &Job, nodes: &[Node], budget: &mut SearchBudget) -> Option<Self> {
        let partitions =
            spur_core::partition::requested_partition_names(job.spec.partition.as_deref()).count();
        let features = spur_core::constraints::Constraints::parse(job.spec.constraint.as_deref())
            .map_or(1, |constraints| constraints.features.len());
        let work = nodes.iter().try_fold(0usize, |work, node| {
            node.partitions.iter().try_fold(
                work.checked_add(node.name.len().checked_add(1)?)?,
                |work, partition| work.checked_add(partition.len().checked_add(1)?),
            )
        })?;
        let feature_work = nodes.iter().try_fold(0usize, |work, node| {
            node.features.iter().try_fold(work, |work, feature| {
                work.checked_add(feature.len().checked_add(1)?)
            })
        })?;
        let scan_work = work
            .checked_mul(partitions.max(1))?
            .checked_add(feature_work.checked_mul(features)?)?;
        budget.spend(scan_work)?;
        let placement = NodePlacement::new(job);
        let mut targets = HashSet::new();
        for node in nodes {
            if placement.allows_name(&node.name)
                && (partitions == 0 || placement.in_partition(node))
                && placement.has_features(node)
            {
                targets.try_reserve(1).ok()?;
                targets.insert(node.name.clone());
            }
        }
        Some(Self {
            nodes: targets,
            scan_work,
        })
    }

    pub fn overlaps(&self, candidate: &Job, budget: &mut SearchBudget) -> Option<bool> {
        if candidate.allocated_nodes.len() > MAX_NODES {
            return Some(false);
        }
        let work = candidate
            .allocated_nodes
            .iter()
            .try_fold(0usize, |work, node| {
                work.checked_add(node.len().checked_add(1)?)
            })?;
        budget.spend(work)?;
        Some(
            candidate
                .held_node_names()
                .any(|node| self.nodes.contains(node)),
        )
    }
}

pub struct PlacementRequest {
    pub(crate) job: Job,
    pub(crate) trial_work: usize,
    scope: PlacementScope,
}

/// Keeps static inputs fixed while trials only release allocations and charges.
pub struct PlacementProjection<'a> {
    pub(crate) request: PlacementRequest,
    original: crate::traits::ClusterState<'a>,
    nodes: Vec<Node>,
    indices: HashMap<String, usize>,
    usage: crate::partition_cap::PartitionNodeUsage,
}

impl<'a> PlacementProjection<'a> {
    pub fn new(
        job: &Job,
        state: &crate::traits::ClusterState<'a>,
        budget: &mut SearchBudget,
        size: InputSize,
    ) -> Option<Self> {
        let request = PlacementRequest::with_size(job, state, budget, size)?;
        let nodes: Vec<_> = state.nodes.iter().map(placement_node).collect();
        let indices = nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.name.clone(), i))
            .collect();
        Some(Self {
            request,
            original: crate::traits::ClusterState {
                nodes: state.nodes,
                ..*state
            },
            nodes,
            indices,
            usage: state.partition_node_usage.clone(),
        })
    }

    pub fn job(&self) -> &Job {
        self.request.job()
    }

    pub fn reset(&mut self) {
        for (node, original) in self.nodes.iter_mut().zip(self.original.nodes) {
            node.alloc_resources.cpus = original.alloc_resources.cpus;
            node.alloc_resources.memory_mb = original.alloc_resources.memory_mb;
            node.alloc_resources.reserved_cpus = original.alloc_resources.reserved_cpus;
            node.alloc_resources.layout_fingerprint = original.alloc_resources.layout_fingerprint;
            node.alloc_resources
                .devices
                .clone_from(&original.alloc_resources.devices);
        }
        self.usage.clone_from(self.original.partition_node_usage);
    }

    pub fn release(&mut self, name: &str, allocation: &ResourceAllocations) {
        if let Some(&i) = self.indices.get(name) {
            self.nodes[i].release(allocation);
        }
    }

    pub fn release_charges(&mut self, selected: &HashSet<spur_core::job::JobId>) {
        for by_node in self.usage.values_mut() {
            for charges in by_node.values_mut() {
                charges.retain(|charge| !selected.contains(&charge.job_id));
            }
        }
    }

    pub(crate) fn state(&self) -> crate::traits::ClusterState<'_> {
        crate::traits::ClusterState {
            nodes: &self.nodes,
            partition_node_usage: &self.usage,
            ..self.original
        }
    }
}

pub fn placement_node(node: &Node) -> Node {
    let mut copy = Node::new(node.name.clone(), Default::default());
    copy.total_resources = consumable_inventory(&node.total_resources);
    copy.alloc_resources = consumable_allocation(&node.alloc_resources);
    copy.state = node.state;
    copy.not_responding = node.not_responding;
    copy.partitions = node.partitions.clone();
    copy.features = node.features.clone();
    copy.source = node.source.clone();
    copy.capabilities = node.capabilities.clone();
    copy.weight = node.weight;
    copy.k0s_role = node.k0s_role;
    copy.layout_error = node.layout_error.clone();
    copy.layout_enforce_error = node.layout_enforce_error.clone();
    copy.core_layout = node.core_layout.clone();
    copy.layout_blocked_cpus = node.layout_blocked_cpus.clone();
    copy
}

impl PlacementRequest {
    pub fn job(&self) -> &Job {
        &self.job
    }

    pub fn scope(&self) -> &PlacementScope {
        &self.scope
    }

    pub fn new(
        job: &Job,
        state: &crate::traits::ClusterState<'_>,
        budget: &mut SearchBudget,
    ) -> Option<Self> {
        Self::with_size(job, state, budget, InputSize::default())
    }

    pub fn with_size(
        job: &Job,
        state: &crate::traits::ClusterState<'_>,
        budget: &mut SearchBudget,
        mut size: InputSize,
    ) -> Option<Self> {
        if state.nodes.len() > MAX_NODES || job.het_job_id.is_some() || job.het_group.is_some() {
            return None;
        }
        let before_request = size.entries.checked_add(size.text)?;
        budget.spend(job_work(job)?)?;
        size.job(job)?;
        let request_work = size
            .entries
            .checked_add(size.text)?
            .checked_sub(before_request)?;
        if spur_core::constraints::requires_gpu_properties(&job.spec) {
            return None;
        }
        for node in state.nodes {
            budget.spend(1)?;
            size.node(node)?;
        }
        let before_policy = size.entries.checked_add(size.text)?;
        for partition in state.partitions {
            budget.spend(1)?;
            size.text(&partition.name)?;
            size.strings(&partition.allow_accounts)?;
            size.strings(&partition.allow_groups)?;
            size.strings(&partition.allow_qos)?;
            size.strings(&partition.deny_accounts)?;
            size.strings(&partition.deny_qos)?;
            size.strings(partition.max_tres_per_node.keys())?;
        }
        for reservation in state.reservations {
            budget.spend(1)?;
            size.text(&reservation.name)?;
            size.strings(&reservation.nodes)?;
            size.strings(&reservation.users)?;
            size.strings(&reservation.accounts)?;
        }
        let policy_work = size
            .entries
            .checked_add(size.text)?
            .checked_sub(before_policy)?;
        for (node, owners) in state.preemption_handoffs {
            size.text(node)?;
            size.entries(owners.len())?;
        }
        for (partition, nodes) in state.partition_node_usage {
            size.text(partition)?;
            for (node, charges) in nodes {
                size.text(node)?;
                size.entries(charges.len())?;
                for charge in charges {
                    size.strings(charge.tres.keys())?;
                }
            }
        }
        if let Some(topology) = state
            .topology
            .filter(|_| matches!(job.spec.topology.as_deref(), Some("tree" | "block")))
        {
            for (name, switch) in &topology.switches {
                budget.spend(64)?;
                size.text(name)?;
                size.strings(&switch.nodes)?;
                size.strings(&switch.children)?;
                size.strings([&switch.parent].into_iter().flatten())?;
                let mut current = Some(name.as_str());
                for _ in 0..64 {
                    current = current
                        .and_then(|name| topology.switches.get(name))
                        .and_then(|switch| switch.parent.as_deref());
                    if current.is_none() {
                        break;
                    }
                }
                if current.is_some() {
                    return None;
                }
            }
            for (node, switch) in &topology.node_switch {
                size.text(node)?;
                size.text(switch)?;
            }
        }
        budget.spend(size.entries.checked_add(size.text)?)?;
        let mut job = request_job(job);
        let before_hostlists = size.entries.checked_add(size.text)?;
        canonicalize_hostlists(&mut job, &mut size, budget)?;
        let hostlist_work = size
            .entries
            .checked_add(size.text)?
            .checked_sub(before_hostlists)?;
        let scope = PlacementScope::from_prepared(&job, state.nodes, budget)?;
        let mut n = 0usize;
        let mut device_work = 0usize;
        for node in state.nodes {
            budget.spend(1)?;
            let devices = node
                .alloc_resources
                .devices
                .values()
                .map(Vec::len)
                .sum::<usize>();
            let gpus = node.total_resources.gpus.len();
            device_work = device_work.checked_add(
                node.layout_blocked_cpus
                    .len()
                    .checked_mul(gpus.checked_add(devices)?.checked_add(1)?)?,
            )?;
            if !scope.nodes.contains(&node.name) {
                continue;
            }
            n = n.checked_add(1)?;
            let gpu_work = gpus
                .checked_mul(gpus)?
                .checked_mul(if job.spec.gpus.is_some() {
                    gpus.max(1)
                } else {
                    1
                })?;
            device_work = device_work
                .checked_add(gpu_work)?
                .checked_add(devices.checked_mul(devices)?)?;
        }
        // Aggregate node and charge metadata is scanned once, not for every candidate.
        let matching_work = request_work
            .checked_add(policy_work)?
            .checked_add(hostlist_work)?;
        let mut charges_by_node: HashMap<&str, usize> = HashMap::new();
        for by_node in state.partition_node_usage.values() {
            for (node, charges) in by_node {
                let count = charges_by_node.entry(node).or_default();
                *count = count.checked_add(charges.len())?;
            }
        }
        let seed_work = charges_by_node.values().try_fold(0usize, |work, count| {
            work.checked_add(count.checked_add(1)?.checked_mul(count.checked_add(1)?)?)
        })?;
        let requested =
            spur_core::partition::requested_partition_names(job.spec.partition.as_deref()).count();
        let partition_work = state
            .partitions
            .iter()
            .try_fold(0usize, |work, partition| {
                work.checked_add(partition.name.len().checked_add(1)?)
            })?
            .checked_mul(requested.max(1))?;
        budget.spend(partition_work)?;
        let partitions = crate::partition_cap::candidate_partitions(&job, state.partitions).len();
        // Separate candidate work from the scans and copies every node incurs.
        let trial_work = n
            .checked_mul(n.checked_add(1)?.ilog2() as usize + 1)?
            .checked_add(size.entries)?
            .checked_add(size.text)?
            .checked_add(scope.scan_work)?
            .checked_add(partition_work)?
            .checked_add(seed_work)?
            .checked_add(device_work)?
            .checked_add(
                if state.topology.is_some()
                    && matches!(job.spec.topology.as_deref(), Some("tree" | "block"))
                {
                    n.checked_mul(64 * 64)?
                } else {
                    0
                },
            )?
            .checked_add(n.checked_mul(matching_work)?)?
            .checked_mul(partitions.max(1))?;
        Some(Self {
            job,
            trial_work,
            scope,
        })
    }
}

/// Copies placement and policy inputs without copying scripts, environment, or history.
pub fn placement_job(job: &Job) -> Job {
    let s = &job.spec;
    let spec = JobSpec {
        partition: s.partition.clone(),
        account: s.account.clone(),
        user: s.user.clone(),
        uid: s.uid,
        gid: s.gid,
        num_nodes: s.num_nodes,
        num_tasks: s.num_tasks,
        tasks_per_node: s.tasks_per_node,
        cpus_per_task: s.cpus_per_task,
        cpus_per_gpu: s.cpus_per_gpu,
        memory_per_node_mb: s.memory_per_node_mb,
        memory_per_cpu_mb: s.memory_per_cpu_mb,
        memory_per_gpu_mb: s.memory_per_gpu_mb,
        gres: s.gres.clone(),
        gpus: s.gpus.clone(),
        gpus_per_node: s.gpus_per_node.clone(),
        gpus_per_task: s.gpus_per_task.clone(),
        time_limit: s.time_limit,
        qos: s.qos.clone(),
        reservation: s.reservation.clone(),
        nodelist: s.nodelist.clone(),
        exclude: s.exclude.clone(),
        constraint: s.constraint.clone(),
        distribution: s.distribution.clone(),
        exclusive: s.exclusive,
        spread_job: s.spread_job,
        topology: s.topology.clone(),
        requeue: s.requeue,
        begin_time: s.begin_time,
        deadline: s.deadline,
        ..Default::default()
    };
    let mut copy = Job::new(job.job_id, spec);
    copy.submit_time = job.submit_time;
    copy.priority = job.priority;
    copy.preferred_nodes = job.preferred_nodes.clone();
    copy.het_job_id = job.het_job_id;
    copy.het_group = job.het_group;
    copy
}

/// [`placement_job`] for the job being placed, which also keeps what placement reads
/// off its batch script: the launcher scan, as a stand-in, but never the script.
fn request_job(job: &Job) -> Job {
    let s = &job.spec;
    let mut copy = placement_job(job);
    copy.spec.script = s.script.as_deref().map(|script| {
        ResolvedPlacement::scanned(Some(script))
            .stand_in_script()
            .into()
    });
    copy.spec.script_placement = s.script_placement;
    copy.spec.mpi = s.mpi.clone();
    copy.spec.srun_job = s.srun_job;
    copy.spec.interactive = s.interactive;
    copy
}

pub fn consumable_allocation(alloc: &ResourceAllocations) -> ResourceAllocations {
    ResourceAllocations {
        cpus: alloc.cpus,
        memory_mb: alloc.memory_mb,
        devices: alloc.devices.clone(),
        generation: alloc.generation,
        reserved_cpus: alloc.reserved_cpus,
        layout_fingerprint: alloc.layout_fingerprint,
        ..Default::default()
    }
}

pub fn consumable_inventory(inventory: &ResourceSet) -> ResourceSet {
    ResourceSet {
        cpus: inventory.cpus,
        memory_mb: inventory.memory_mb,
        gpus: inventory
            .gpus
            .iter()
            .map(|gpu| spur_core::resource::GpuResource {
                device_id: gpu.device_id,
                gpu_type: gpu.gpu_type.clone(),
                memory_mb: gpu.memory_mb,
                peer_gpus: Vec::new(),
                link_type: gpu.link_type,
                stable_id: gpu.stable_id,
                topology: None,
                numa_node: None,
                affinity_cpus: Vec::new(),
            })
            .collect(),
        generic: inventory.generic.clone(),
        generation: inventory.generation,
        ..Default::default()
    }
}
