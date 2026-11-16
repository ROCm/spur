// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use spur_core::job::{Job, JobId, JobState};
use spur_core::node::Node;
use spur_core::resource::ResourceAllocations;
use spur_sched::backfill::BackfillScheduler;
use spur_sched::immediate::{
    consumable_allocation, consumable_inventory, job_work, placement_job, InputSize,
    PlacementRequest, SearchBudget, MAX_NODES, MAX_VICTIMS,
};
use spur_sched::partition_cap::{job_typed_gpu, PartitionNodeUsage, RunningCharge, TresCharge};
use spur_sched::traits::{Assignment, ClusterState};

use super::{partition_usage_from_jobs, ClusterManager, ReleaseWaits};

#[cfg(test)]
mod tests;

pub(super) struct Snapshot {
    pub nodes: Vec<Node>,
    pub holders: Vec<Job>,
    pub usage: PartitionNodeUsage,
}

impl Snapshot {
    pub fn capture(
        cluster: &ClusterManager,
        state: &ClusterState<'_>,
        assigned: &[Assignment],
        now: DateTime<Utc>,
        budget: &mut SearchBudget,
    ) -> Option<Self> {
        if state.nodes.len() > MAX_NODES {
            return None;
        }
        cluster.with_allocation_records(|jobs, live_nodes| {
            let mut size = InputSize::default();
            for node in state.nodes {
                budget.spend(1)?;
                size.entries(6)?;
                let live = live_nodes.get(&node.name)?;
                size.text(&node.name)?;
                size.strings(&live.partitions)?;
                size.strings(&live.features)?;
                size.strings(
                    [&live.layout_error, &live.layout_enforce_error]
                        .into_iter()
                        .flatten(),
                )?;
                size.inventory(&live.total_resources)?;
                size.inventory(&node.total_resources)?;
                let current = &live.total_resources;
                let observed = &node.total_resources;
                if current.cpus != observed.cpus
                    || current.memory_mb != observed.memory_mb
                    || current.generation != observed.generation
                    || current.generic != observed.generic
                    || current.gpus.len() != observed.gpus.len()
                    || current.gpus.iter().zip(&observed.gpus).any(|(a, b)| {
                        a.stable_id != b.stable_id
                            || a.device_id != b.device_id
                            || a.gpu_type != b.gpu_type
                            || a.memory_mb != b.memory_mb
                    })
                {
                    return None;
                }
                size.allocation(&live.alloc_resources)?;
            }
            let holds = |job: &&Job| {
                job.occupies_resources()
                    || matches!(job.state, JobState::Suspended | JobState::Completing)
            };
            for job in jobs.values() {
                budget.spend(1)?;
                if !holds(&job) {
                    continue;
                }
                budget.spend(job_work(job)?)?;
                size.job(job)?;
                size.strings(&job.allocated_nodes)?;
                budget.spend(
                    job.allocated_nodes.len().checked_mul(
                        job.allocated_nodes
                            .len()
                            .checked_add(job.per_node_alloc.len())?,
                    )?,
                )?;
                size.strings(
                    [&job.effective_partition, &job.preempt_mode]
                        .into_iter()
                        .flatten(),
                )?;
                for (node, allocation) in &job.per_node_alloc {
                    budget.spend(1)?;
                    size.text(node)?;
                    size.allocation(allocation)?;
                }
            }
            for assignment in assigned {
                budget.spend(1)?;
                let job = jobs.get(&assignment.job_id)?;
                budget.spend(job_work(job)?)?;
                size.job(job)?;
                size.strings(&assignment.nodes)?;
                size.strings([&assignment.partition].into_iter().flatten())?;
                for (node, allocation) in &assignment.per_node_alloc {
                    size.text(node)?;
                    size.allocation(allocation)?;
                }
            }
            let mut nodes = Vec::new();
            nodes.try_reserve_exact(state.nodes.len()).ok()?;
            for node in state.nodes {
                let live = live_nodes.get(&node.name)?;
                let mut copy = Node::new(node.name.clone(), Default::default());
                copy.total_resources = consumable_inventory(&live.total_resources);
                copy.alloc_resources = consumable_allocation(&live.alloc_resources);
                copy.state = live.state;
                copy.partitions = live.partitions.clone();
                copy.features = live.features.clone();
                copy.weight = live.weight;
                copy.k0s_role = live.k0s_role;
                copy.layout_error = live.layout_error.clone();
                copy.layout_enforce_error = live.layout_enforce_error.clone();
                nodes.push(copy);
            }
            let mut holders = Vec::new();
            for job in jobs.values().filter(holds) {
                let mut copy = placement_job(job);
                copy.state = job.state;
                copy.start_time = job.start_time;
                copy.suspended_at = job.suspended_at;
                copy.suspended_secs = job.suspended_secs;
                copy.run_attempt = job.run_attempt;
                copy.preempt_requeue_count = job.preempt_requeue_count;
                copy.idle_fill = job.idle_fill;
                copy.effective_partition = job.effective_partition.clone();
                copy.preempted_by = job.preempted_by;
                copy.preempt_mode = job.preempt_mode.clone();
                copy.preempt_pending =
                    job.preempt_grace()
                        .map(|grace| spur_core::job::PreemptPending {
                            preemption_claim: None,
                            preempt_qos: None,
                            signaled_at: grace.signaled_at,
                            deadline: grace.deadline,
                            preempted_by: grace.preempted_by,
                            mode: grace.mode,
                            run_attempt: grace.run_attempt,
                            requeue_hold_secs: grace.requeue_hold_secs,
                        });
                copy.allocated_nodes = job.held_nodes();
                for (node, alloc) in job
                    .per_node_alloc
                    .iter()
                    .filter(|(node, _)| job.is_held_on(node))
                {
                    copy.per_node_alloc
                        .insert(node.clone(), consumable_allocation(alloc));
                }
                holders.push(copy);
            }
            let mut usage = partition_usage_from_jobs(
                &holders,
                state.partitions,
                now,
                ReleaseWaits::of(&cluster.config().scheduler),
            );
            for assignment in assigned {
                budget.spend(
                    holders
                        .len()
                        .checked_add(assignment.nodes.len().checked_mul(assignment.nodes.len())?)?,
                )?;
                let job = jobs.get(&assignment.job_id)?;
                if let Some(holder) = holders.iter().find(|job| job.job_id == assignment.job_id) {
                    if !matches!(holder.state, JobState::Pending | JobState::Running)
                        || holder.allocated_nodes.len() != assignment.nodes.len()
                        || holder.effective_partition != assignment.partition
                        || assignment.nodes.iter().any(|node| {
                            !holder.allocated_nodes.contains(node)
                                || holder.per_node_alloc.get(node)
                                    != assignment.per_node_alloc.get(node)
                        })
                    {
                        return None;
                    }
                    continue;
                }
                if job.state != JobState::Pending {
                    return None;
                }
                let typed_gpu = job_typed_gpu(job);
                for node in &assignment.nodes {
                    let index = nodes.iter().position(|n| n.name == *node)?;
                    let alloc = assignment.per_node_alloc.get(node)?;
                    let held = &mut nodes[index].alloc_resources;
                    budget.spend(allocation_merge_work(held, alloc)?)?;
                    add_checked(held, alloc)?;
                    if let Some(partition) = assignment.partition.as_ref() {
                        usage
                            .entry(partition.clone())
                            .or_default()
                            .entry(node.clone())
                            .or_default()
                            .push(RunningCharge {
                                job_id: assignment.job_id,
                                tres: TresCharge::from_allocation(alloc, typed_gpu.as_deref()),
                                until: now + spur_sched::UNLIMITED_JOB_DURATION,
                            });
                    }
                }
            }
            Some(Self {
                nodes,
                holders,
                usage,
            })
        })
    }

    pub fn state<'a>(&'a self, state: &'a ClusterState<'_>) -> ClusterState<'a> {
        ClusterState {
            nodes: &self.nodes,
            partition_node_usage: &self.usage,
            partitions: state.partitions,
            reservations: state.reservations,
            topology: state.topology,
            fabric: state.fabric,
            busy_until: state.busy_until,
            preemption_handoffs: state.preemption_handoffs,
        }
    }
}

pub(super) struct VictimPool<'a> {
    pub evictable: &'a HashSet<JobId>,
    pub holders: &'a [Job],
    pub priority: &'a HashMap<JobId, i32>,
    pub max_priority: Option<i32>,
}

fn contains(held: &ResourceAllocations, slice: &ResourceAllocations) -> bool {
    held.cpus >= slice.cpus
        && held.memory_mb >= slice.memory_mb
        && slice.devices.iter().all(|(name, devices)| {
            devices.iter().all(|device| {
                held.devices.get(name).is_some_and(|held| {
                    held.iter().any(|entry| {
                        entry.device_id == device.device_id && entry.count >= device.count
                    })
                })
            })
        })
}

fn allocation_merge_work(held: &ResourceAllocations, slice: &ResourceAllocations) -> Option<usize> {
    let held_count = held.devices.values().map(Vec::len).sum::<usize>();
    let slice_count = slice.devices.values().map(Vec::len).sum::<usize>();
    held_count
        .checked_add(slice_count)?
        .checked_add(1)?
        .checked_mul(slice_count.checked_add(1)?)?
        .checked_mul(4)
}

fn add_checked(held: &mut ResourceAllocations, slice: &ResourceAllocations) -> Option<()> {
    held.cpus.checked_add(slice.cpus)?;
    held.memory_mb.checked_add(slice.memory_mb)?;
    for (name, devices) in &slice.devices {
        for (index, device) in devices.iter().enumerate() {
            if devices[..index]
                .iter()
                .any(|entry| entry.device_id == device.device_id)
            {
                return None;
            }
            if let Some(existing) = held.devices.get(name).and_then(|entries| {
                entries
                    .iter()
                    .find(|entry| entry.device_id == device.device_id)
            }) {
                existing.count.checked_add(device.count)?;
            }
        }
    }
    held.add(slice);
    Some(())
}

pub(super) fn satisfiable_victim_set(
    reclaimer: &Job,
    pool: &VictimPool<'_>,
    state: &ClusterState<'_>,
    now: DateTime<Utc>,
    budget: &mut SearchBudget,
) -> Option<HashSet<JobId>> {
    if pool.evictable.len() > MAX_VICTIMS {
        return None;
    }
    let mut ordered = Vec::new();
    ordered.try_reserve_exact(pool.evictable.len()).ok()?;
    let mut size = InputSize::default();
    for holder in pool.holders {
        budget.spend(1)?;
        budget.spend(job_work(holder)?)?;
        size.job(holder)?;
        size.strings(&holder.allocated_nodes)?;
        budget.spend(
            holder
                .allocated_nodes
                .len()
                .checked_mul(holder.per_node_alloc.len())?,
        )?;
        for (node, alloc) in holder
            .per_node_alloc
            .iter()
            .filter(|(n, _)| holder.is_held_on(n))
        {
            budget.spend(1)?;
            size.text(node)?;
            size.allocation(alloc)?;
        }
        if pool.evictable.contains(&holder.job_id)
            && pool
                .max_priority
                .is_none_or(|bound| pool.priority.get(&holder.job_id).copied().unwrap_or(0) < bound)
            && !holder.allocated_nodes.is_empty()
            && holder
                .allocated_nodes
                .iter()
                .all(|n| holder.per_node_alloc.contains_key(n))
        {
            if ordered.len() == MAX_VICTIMS {
                return None;
            }
            ordered.push(holder);
        }
    }
    let request = PlacementRequest::with_size(reclaimer, state, budget, size)?;
    let mut known: HashMap<&str, ResourceAllocations> = HashMap::new();
    for holder in pool.holders {
        for (node, alloc) in holder
            .per_node_alloc
            .iter()
            .filter(|(n, _)| holder.is_held_on(n))
        {
            let held = known.entry(node).or_default();
            budget.spend(allocation_merge_work(held, alloc)?)?;
            add_checked(held, alloc)?;
        }
    }
    ordered.sort_by_key(|job| {
        (
            pool.priority.get(&job.job_id).copied().unwrap_or(0),
            job.job_id,
        )
    });
    budget.spend(ordered.len().checked_mul(state.nodes.len())?)?;
    for job in &ordered {
        budget.spend(job.per_node_alloc.len().checked_mul(state.nodes.len())?)?;
        for (name, slice) in &job.per_node_alloc {
            if let (Some(node), Some(all)) = (
                state.nodes.iter().find(|n| n.name == *name),
                known.get(name.as_str()),
            ) {
                budget.spend(allocation_merge_work(&node.alloc_resources, all)?)?;
                budget.spend(slice.devices.len())?;
            }
        }
    }
    let valid = |job: &&Job| {
        job.per_node_alloc.iter().all(|(name, slice)| {
            state
                .nodes
                .iter()
                .find(|n| n.name == *name)
                .is_none_or(|node| {
                    (slice.generation == 0 || slice.generation == node.total_resources.generation)
                        && known
                            .get(name.as_str())
                            .is_some_and(|all| contains(&node.alloc_resources, all))
                })
        })
    };
    ordered.retain(valid);
    if ordered.is_empty() {
        return None;
    }
    let mut nodes: Vec<Node> = state
        .nodes
        .iter()
        .map(|n| {
            let mut node = Node::new(n.name.clone(), Default::default());
            node.total_resources = consumable_inventory(&n.total_resources);
            node.alloc_resources = consumable_allocation(&n.alloc_resources);
            node.state = n.state;
            node.partitions = n.partitions.clone();
            node.features = n.features.clone();
            node.weight = n.weight;
            node.k0s_role = n.k0s_role;
            node.layout_error = n.layout_error.clone();
            node.layout_enforce_error = n.layout_enforce_error.clone();
            node
        })
        .collect();
    let mut usage = state.partition_node_usage.clone();
    let mut scheduler = BackfillScheduler::new(1);
    let mut calls = 0usize;
    let mut fits = |selected: &HashSet<JobId>| -> Option<bool> {
        calls = calls.checked_add(1)?;
        if calls > 128 {
            return None;
        }
        budget.spend(
            pool.holders
                .len()
                .checked_add(nodes.len().checked_mul(ordered.len())?)?,
        )?;
        for (node, original) in nodes.iter_mut().zip(state.nodes) {
            node.alloc_resources.cpus = original.alloc_resources.cpus;
            node.alloc_resources.memory_mb = original.alloc_resources.memory_mb;
            node.alloc_resources
                .devices
                .clone_from(&original.alloc_resources.devices);
            for job in ordered.iter().filter(|job| selected.contains(&job.job_id)) {
                if let Some(slice) = job.per_node_alloc.get(&node.name) {
                    node.alloc_resources.subtract(slice);
                }
            }
        }
        usage.clone_from(state.partition_node_usage);
        for by_node in usage.values_mut() {
            for charges in by_node.values_mut() {
                charges.retain(|charge| !selected.contains(&charge.job_id));
            }
        }
        let trial = ClusterState {
            nodes: &nodes,
            partition_node_usage: &usage,
            ..*state
        };
        scheduler
            .place_now(&request, &trial, now, budget)
            .map(|assignment| assignment.is_some())
    };
    if fits(&HashSet::new())? {
        return None;
    }
    let all = ordered.iter().map(|job| job.job_id).collect();
    if !fits(&all)? {
        return None;
    }
    let mut selected = HashSet::new();
    for job in &ordered {
        selected.insert(job.job_id);
        if fits(&selected)? {
            break;
        }
    }
    loop {
        let mut changed = false;
        for job in ordered.iter().rev() {
            if !selected.remove(&job.job_id) {
                continue;
            }
            if fits(&selected)? {
                changed = true;
            } else {
                selected.insert(job.job_id);
            }
        }
        if !changed {
            return (!selected.is_empty()).then_some(selected);
        }
    }
}
