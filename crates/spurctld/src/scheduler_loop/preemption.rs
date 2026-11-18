// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use spur_core::job::{Job, JobId, JobState};
use spur_core::node::Node;
use spur_core::resource::ResourceAllocations;
use spur_sched::backfill::{job_resource_request, BackfillScheduler};
use spur_sched::immediate::{
    consumable_allocation, job_work, placement_job, placement_node, InputSize, PlacementProjection,
    SearchBudget, MAX_NODES, MAX_TRIALS, MAX_VICTIMS,
};
use spur_sched::partition_cap::{job_typed_gpu, PartitionNodeUsage, RunningCharge, TresCharge};
use spur_sched::traits::{Assignment, ClusterState};

use super::{partition_usage_from_jobs, ClusterManager, ReleaseWaits, VictimSearch};
use crate::victim_order::VictimOrder;

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
                let live = live_nodes.get(&node.name)?;
                size.node(live)?;
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
            }
            let holds = |job: &&Job| job.holds_allocation();
            for job in jobs.values() {
                budget.spend(job.allocated_nodes.len().checked_add(1)?)?;
                if !holds(&job) {
                    continue;
                }
                budget.spend(job_work(job)?)?;
                size.job(job)?;
                size.strings(&job.allocated_nodes)?;
                budget.spend(held_allocation_work(job)?)?;
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
                nodes.push(placement_node(live));
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
                for (node, alloc) in job.held_allocations() {
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
            budget.spend(state.nodes.len())?;
            let node_index: HashMap<_, _> = state
                .nodes
                .iter()
                .enumerate()
                .map(|(index, node)| (node.name.as_str(), index))
                .collect();
            for assignment in assigned {
                budget.spend(holders.len().checked_add(assignment.nodes.len())?)?;
                let job = jobs.get(&assignment.job_id)?;
                if let Some(holder) = holders.iter().find(|job| job.job_id == assignment.job_id) {
                    budget.spend(held_allocation_work(holder)?)?;
                    let held_nodes: HashSet<_> = holder.allocated_nodes.iter().collect();
                    if !matches!(holder.state, JobState::Pending | JobState::Running)
                        || holder.allocated_nodes.len() != assignment.nodes.len()
                        || holder.effective_partition != assignment.partition
                        || assignment.nodes.iter().any(|node| {
                            !held_nodes.contains(node)
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
                    let index = *node_index.get(node.as_str())?;
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
            script_placement_default: state.script_placement_default,
        }
    }
}

pub(super) struct VictimPool<'a> {
    pub evictable: &'a HashSet<JobId>,
    pub holders: &'a [Job],
    pub priority: &'a HashMap<JobId, i32>,
    pub max_priority: Option<i32>,
    pub order: Option<VictimOrder<'a>>,
}

fn held_allocation_work(job: &Job) -> Option<usize> {
    job.allocated_nodes
        .iter()
        .chain(job.per_node_alloc.keys())
        .try_fold(0usize, |work, name| {
            work.checked_add(name.len().checked_add(1)?)
        })?
        .checked_mul(2)
}

fn contains(held: &ResourceAllocations, slice: &ResourceAllocations) -> bool {
    held.cpus >= slice.cpus
        && held.reserved_cpus >= slice.reserved_cpus
        && held.cpus.saturating_sub(held.reserved_cpus)
            >= slice.cpus.saturating_sub(slice.reserved_cpus)
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
    let reserved = held
        .reserved_cpus
        .checked_add(slice.reserved_under(held.layout_fingerprint))?;
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
    held.reserved_cpus = reserved;
    Some(())
}

fn subtract_charged(held: &mut ResourceAllocations, slice: &ResourceAllocations) {
    let reserved = held
        .reserved_cpus
        .saturating_sub(slice.reserved_under(held.layout_fingerprint));
    held.subtract(slice);
    held.reserved_cpus = reserved;
}

fn preferred_victims(
    request: &Job,
    ordered: &[&Job],
    pool: &VictimPool<'_>,
    state: &ClusterState<'_>,
    now: DateTime<Utc>,
    budget: &mut SearchBudget,
) -> Option<HashSet<JobId>> {
    let placement = spur_sched::node_match::NodePlacement::new(request);
    let required = job_resource_request(request);
    let needs_aware_agents = request
        .spec
        .needs_placement_aware_agents(state.script_placement_default);
    let mut on_node: HashMap<&str, Vec<JobId>> = HashMap::new();
    for job in ordered {
        for node in &job.allocated_nodes {
            budget.spend(node.len().checked_add(1)?)?;
            on_node.entry(node).or_default().push(job.job_id);
        }
    }
    for jobs in on_node.values_mut() {
        jobs.sort_unstable();
    }
    let mut empty = 0usize;
    let mut candidates = Vec::new();
    for node in state.nodes {
        budget.spend(1)?;
        if !placement.matches_for_reservation(node, state.reservations, now)
            || (needs_aware_agents && node.reads_placement_off_script())
            || !node.total_resources.can_satisfy(&required)
            || !state.handoff_allows(request, &node.name)
        {
            continue;
        }
        if let Some(jobs) = on_node.get(node.name.as_str()) {
            candidates.push((node, jobs.as_slice()));
        } else if node.can_satisfy_request(&required) {
            empty += 1;
        }
    }
    let short = (request.spec.num_nodes as usize)
        .max(1)
        .saturating_sub(empty)
        .max(1);
    if candidates.len() < short {
        return Some(HashSet::new());
    }
    let (preferred, _) =
        VictimSearch::new(&candidates, pool.priority, pool.order.as_ref()).run(short, budget)?;
    Some(preferred)
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct SelectionDeferred;

fn direct_victims(
    projection: &PlacementProjection<'_>,
    ordered: &[&Job],
    pool: &VictimPool<'_>,
    state: &ClusterState<'_>,
    now: DateTime<Utc>,
    budget: &mut SearchBudget,
) -> Option<Option<HashSet<JobId>>> {
    use spur_core::gpu_request::{resolve_gpu_demand, GpuDemand};
    use spur_sched::node_match::NodePlacement;

    let job = projection.job();
    let cpus = spur_core::partition_limits::node_cpu_counts(&job.spec);
    let uniform_gpu = match resolve_gpu_demand(&job.spec).ok()? {
        GpuDemand::None => true,
        GpuDemand::PerNode { counts, .. } => counts.windows(2).all(|pair| pair[0] == pair[1]),
        GpuDemand::Total { .. } => job.spec.num_nodes <= 1,
    };
    let placement = NodePlacement::new(job);
    if (!job.spec.exclusive && (!uniform_gpu || cpus.windows(2).any(|p| p[0] != p[1])))
        || placement.nodelist_is_additive()
        || ordered.iter().any(|job| job.allocated_nodes.len() != 1)
        || state
            .partitions
            .iter()
            .any(|p| p.has_memory_policy() || p.max_mem_per_node.is_some())
    {
        return Some(None);
    }
    let required = job_resource_request(job);
    let partitions = spur_sched::partition_cap::candidate_partitions(job, state.partitions);
    let typed_gpu = job_typed_gpu(job);
    let mut on_node: HashMap<&str, Vec<&Job>> = HashMap::new();
    for victim in ordered {
        let node = victim.allocated_nodes.first()?;
        budget.spend(node.len().checked_add(1)?)?;
        on_node.entry(node).or_default().push(victim);
    }
    let candidates = BackfillScheduler::new(1).immediate_candidates(projection, now);
    let mut groups = Vec::new();
    let mut free = 0usize;
    for i in candidates {
        let node = &state.nodes[i];
        let required = if job.spec.exclusive {
            &node.total_resources
        } else {
            &required
        };
        let device_work = node
            .total_resources
            .gpus
            .len()
            .checked_add(1)?
            .checked_mul(
                node.alloc_resources
                    .devices
                    .values()
                    .map(Vec::len)
                    .sum::<usize>()
                    .checked_add(1)?,
            )?;
        let mut used: HashMap<&str, TresCharge> = HashMap::new();
        let mut releases: HashMap<JobId, Vec<(&str, &TresCharge)>> = HashMap::new();
        for (partition, by_node) in state.partition_node_usage {
            for charge in by_node
                .get(&node.name)
                .into_iter()
                .flatten()
                .filter(|c| c.until > now)
            {
                budget.spend(charge.tres.keys().count().checked_add(1)?)?;
                used.entry(partition).or_default().add(&charge.tres);
                releases
                    .entry(charge.job_id)
                    .or_default()
                    .push((partition, &charge.tres));
            }
        }
        let adjust = |id: JobId,
                      used: &mut HashMap<&str, TresCharge>,
                      release: bool,
                      budget: &mut SearchBudget|
         -> Option<()> {
            for (partition, charge) in releases.get(&id).into_iter().flatten() {
                budget.spend(charge.keys().count().checked_add(1)?)?;
                if let Some(total) = used.get_mut(*partition) {
                    if release {
                        total.subtract(charge);
                    } else {
                        total.add(charge);
                    }
                }
            }
            Some(())
        };
        let work = device_work.checked_add(state.partitions.len().checked_mul(8)?)?;
        let holding: Vec<_> = partitions
            .iter()
            .filter(|p| node.partitions.contains(&p.name))
            .collect();
        let charge = spur_sched::backfill::least_fullest_node_charge(
            job,
            required,
            node,
            typed_gpu.as_deref(),
        );
        let admissible: Vec<_> = holding.iter().filter(|p| p.admits(&job.spec)).collect();
        let mut occupied = consumable_allocation(&node.alloc_resources);
        let pools = node.cpu_pools();
        let fits = |occupied: &ResourceAllocations, used: &HashMap<&str, TresCharge>| {
            node.total_resources
                .can_satisfy_with_allocated(occupied, required)
                && pools
                    .is_none_or(|p| p.admits(occupied, required.cpus, required.gpus.len() as u32))
                && (holding.is_empty()
                    || admissible.iter().any(|partition| {
                        spur_sched::partition_cap::partition_cap_ok(
                            partition,
                            &node.name,
                            used.get(partition.name.as_str())
                                .unwrap_or(&TresCharge::default()),
                            &charge,
                        )
                        .is_ok()
                    }))
        };
        budget.spend(work)?;
        if fits(&occupied, &used) {
            free += 1;
            continue;
        }
        let Some(victims) = on_node.get(node.name.as_str()) else {
            continue;
        };
        let mut chosen = Vec::new();
        for victim in victims {
            budget.spend(work)?;
            subtract_charged(&mut occupied, victim.per_node_alloc.get(&node.name)?);
            adjust(victim.job_id, &mut used, true, budget)?;
            chosen.push(*victim);
            if fits(&occupied, &used) {
                break;
            }
        }
        if !fits(&occupied, &used) {
            continue;
        }
        let mut necessary = Vec::new();
        for victim in chosen.into_iter().rev() {
            budget.spend(work)?;
            let slice = victim.per_node_alloc.get(&node.name)?;
            add_checked(&mut occupied, slice)?;
            adjust(victim.job_id, &mut used, false, budget)?;
            if !fits(&occupied, &used) {
                subtract_charged(&mut occupied, slice);
                adjust(victim.job_id, &mut used, true, budget)?;
                necessary.push(victim.job_id);
            }
        }
        necessary.sort_unstable();
        groups.push((node, necessary));
    }
    let short = (job.spec.num_nodes as usize).max(1).saturating_sub(free);
    if short == 0 {
        return Some(Some(HashSet::new()));
    }
    if groups.len() < short {
        return Some(None);
    }
    let candidates: Vec<_> = groups
        .iter()
        .map(|(node, jobs)| (*node, jobs.as_slice()))
        .collect();
    let (victims, _) =
        VictimSearch::new(&candidates, pool.priority, pool.order.as_ref()).run(short, budget)?;
    Some(Some(victims))
}

pub(super) fn satisfiable_victim_set(
    reclaimer: &Job,
    pool: &VictimPool<'_>,
    state: &ClusterState<'_>,
    now: DateTime<Utc>,
    budget: &mut SearchBudget,
) -> Result<Option<HashSet<JobId>>, SelectionDeferred> {
    let selected = select_victims(reclaimer, pool, state, now, budget).ok_or(SelectionDeferred)?;
    if selected
        .as_ref()
        .is_some_and(|victims| victims.len() > MAX_VICTIMS)
    {
        return Err(SelectionDeferred);
    }
    Ok(selected)
}

fn select_victims(
    reclaimer: &Job,
    pool: &VictimPool<'_>,
    state: &ClusterState<'_>,
    now: DateTime<Utc>,
    budget: &mut SearchBudget,
) -> Option<Option<HashSet<JobId>>> {
    let mut ordered = Vec::new();
    let mut size = InputSize::default();
    for holder in pool.holders {
        budget.spend(1)?;
        budget.spend(job_work(holder)?)?;
        size.job(holder)?;
        size.strings(&holder.allocated_nodes)?;
        budget.spend(held_allocation_work(holder)?)?;
        for (node, alloc) in &holder.per_node_alloc {
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
            size.entries(1)?;
            ordered.try_reserve(1).ok()?;
            ordered.push(holder);
        }
    }
    let mut projection = PlacementProjection::new(reclaimer, state, budget, size)?;
    let node_index: HashMap<_, _> = state
        .nodes
        .iter()
        .map(|node| (node.name.as_str(), node))
        .collect();
    let mut known: HashMap<&str, ResourceAllocations> = HashMap::new();
    for holder in pool.holders {
        for (node, alloc) in holder.held_allocations() {
            let held = known.entry(node).or_insert_with(|| ResourceAllocations {
                layout_fingerprint: node_index
                    .get(node.as_str())
                    .map_or(0, |node| node.alloc_resources.layout_fingerprint),
                ..Default::default()
            });
            budget.spend(allocation_merge_work(held, alloc)?)?;
            add_checked(held, alloc)?;
        }
    }
    for job in &ordered {
        for name in job.per_node_alloc.keys() {
            budget.spend(name.len().checked_add(1)?)?;
            if let (Some(node), Some(all)) =
                (node_index.get(name.as_str()), known.get(name.as_str()))
            {
                budget.spend(allocation_merge_work(&node.alloc_resources, all)?)?;
            }
        }
    }
    let valid = |job: &&Job| {
        job.per_node_alloc.iter().all(|(name, slice)| {
            node_index.get(name.as_str()).is_none_or(|node| {
                (slice.generation == 0 || slice.generation == node.total_resources.generation)
                    && known
                        .get(name.as_str())
                        .is_some_and(|all| contains(&node.alloc_resources, all))
            })
        })
    };
    ordered.retain(valid);
    if ordered.is_empty() {
        return Some(None);
    }
    budget.spend(
        ordered
            .len()
            .checked_mul(ordered.len().checked_ilog2().unwrap_or(0) as usize + 1)?,
    )?;
    ordered.sort_by_cached_key(|job| {
        let node = job
            .allocated_nodes
            .iter()
            .min()
            .map(String::as_str)
            .unwrap_or("");
        let key = pool.order.as_ref().map(|order| {
            let freed = job
                .allocated_nodes
                .len()
                .min(reclaimer.spec.num_nodes.max(1) as usize);
            order.key(&order.rank(order.hash(node), &[job.job_id], freed as u32))
        });
        (
            pool.priority.get(&job.job_id).copied().unwrap_or(0),
            key,
            node.to_owned(),
            job.job_id,
        )
    });
    let mut scheduler = BackfillScheduler::new(1);
    let direct = direct_victims(&projection, &ordered, pool, state, now, budget)?;
    let preferred = if direct.is_none() {
        preferred_victims(projection.job(), &ordered, pool, state, now, budget)?
    } else {
        HashSet::new()
    };
    ordered.sort_by_key(|job| {
        (
            pool.priority.get(&job.job_id).copied().unwrap_or(0),
            !preferred.contains(&job.job_id),
        )
    });
    let mut calls = 0usize;
    let mut fits = |selected: &HashSet<JobId>| -> Option<bool> {
        calls = calls.checked_add(1)?;
        if calls > MAX_TRIALS {
            return None;
        }
        budget.spend(pool.holders.len().checked_add(state.nodes.len())?)?;
        projection.reset();
        for job in ordered.iter().filter(|job| selected.contains(&job.job_id)) {
            for (name, slice) in &job.per_node_alloc {
                budget.spend(name.len().checked_add(1)?)?;
                projection.release(name, slice);
            }
        }
        projection.release_charges(selected);
        scheduler
            .place_releasing(&projection, now, budget)
            .map(|assignment| assignment.is_some())
    };
    if let Some(selected) = direct {
        if fits(&selected)? {
            return Some((!selected.is_empty()).then_some(selected));
        }
    }
    if fits(&HashSet::new())? {
        return Some(None);
    }
    let all: HashSet<_> = ordered.iter().map(|job| job.job_id).collect();
    if !fits(&all)? {
        return Some(None);
    }
    // Capacity need not be monotone under placement heuristics; keep the proven upper set.
    let mut low = 0;
    let mut high = ordered.len();
    let mut selected = all;
    while low + 1 < high {
        let middle = low + (high - low) / 2;
        let trial = ordered[..middle].iter().map(|job| job.job_id).collect();
        if fits(&trial)? {
            high = middle;
            selected = trial;
        } else {
            low = middle;
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
            return Some((!selected.is_empty()).then_some(selected));
        }
    }
}
