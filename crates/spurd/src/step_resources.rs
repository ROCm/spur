// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! A numbered step's own share of its job on one node: what it asked for,
//! checked against the job's share, and which of the job's GPUs it takes.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use spur_core::config::CgroupConfig;
use spur_core::step::StepId;
use spur_proto::proto::StepResources;

/// What a step narrows its job's limits to on this node, applied to the step's
/// own cgroup leaf. `None` leaves that resource shared with the rest of the job.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepLimits {
    #[serde(default)]
    pub cpus: Option<u32>,
    #[serde(default)]
    pub memory_mb: Option<u64>,
    /// The step's own GPUs (stable ids), a subset of the job's; empty means none.
    #[serde(default)]
    pub gpus: Option<Vec<u64>>,
    /// The job CPUs the step is pinned to; empty when it is not pinned.
    #[serde(default)]
    pub cpu_ids: Vec<u32>,
    /// An `--overlap` step: limited like any typed step but holds nothing.
    #[serde(default)]
    pub overlap: bool,
}

impl StepLimits {
    pub fn is_empty(&self) -> bool {
        self.cpus.is_none() && self.memory_mb.is_none() && self.gpus.is_none()
    }
}

/// The job's share of this node that a step's request is carved from.
pub struct JobShare<'a> {
    pub cpus: u32,
    pub memory_mb: u64,
    /// The node's schedulable memory, checked against a typed step's own memory
    /// ask when the job itself has no budget (`memory_mb == 0`); 0 = unknown.
    pub memory_ceiling_mb: u64,
    pub gpus: &'a [u64],
    /// Each job GPU's type, for steps that ask for one type.
    pub gpu_types: &'a [(u64, String)],
}

/// Where this node runs the step: its tasks here, out of how many nodes.
#[derive(Clone, Copy)]
pub struct StepPlacement {
    pub tasks_on_node: u32,
    pub step_nodes: u32,
}

/// A step's request resolved for one node, before any GPU is picked.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct NodeRequest {
    pub cpus: Option<u32>,
    pub memory_mb: Option<u64>,
    pub gpus: Option<usize>,
    /// The job GPUs the step may take: all of them, or those of its type.
    pub gpu_pool: Vec<u64>,
}

impl NodeRequest {
    pub fn is_empty(&self) -> bool {
        self.cpus.is_none() && self.memory_mb.is_none() && self.gpus.is_none()
    }
}

/// Resolve `request` for this node. The error names what does not fit the job.
pub fn resolve(
    request: &StepResources,
    placement: StepPlacement,
    job: &JobShare<'_>,
) -> Result<NodeRequest, String> {
    spur_core::step_shape::check(request, Some(placement.step_nodes))?;
    let gpus = resolve_gpu_count(request, placement)?;
    let gpu_pool = gpu_pool(&request.gpu_type, job)?;
    if let Some(count) = gpus {
        if count > gpu_pool.len() {
            return Err(format!(
                "the step asks for {count} GPUs on this node but its job holds {}",
                gpu_pool.len()
            ));
        }
    }
    let cpus = resolve_step_cpus(request, placement, gpus, job.cpus)?;
    let memory_mb = resolve_step_memory(
        request,
        placement,
        cpus,
        gpus,
        job.memory_mb,
        job.memory_ceiling_mb,
    )?;
    Ok(NodeRequest {
        cpus,
        memory_mb,
        gpus,
        gpu_pool,
    })
}

/// The job's GPUs a step may take: all of them, or only those of `gpu_type`.
fn gpu_pool(gpu_type: &str, job: &JobShare<'_>) -> Result<Vec<u64>, String> {
    if gpu_type.is_empty() {
        return Ok(job.gpus.to_vec());
    }
    let type_of = |gpu: &u64| {
        job.gpu_types
            .iter()
            .find(|(id, _)| id == gpu)
            .map(|(_, ty)| ty.as_str())
    };
    let pool: Vec<u64> = job
        .gpus
        .iter()
        .copied()
        .filter(|gpu| type_of(gpu).is_some_and(|ty| ty.eq_ignore_ascii_case(gpu_type)))
        .collect();
    if pool.is_empty() {
        let mut held: Vec<&str> = job.gpus.iter().filter_map(type_of).collect();
        held.sort_unstable();
        held.dedup();
        let held = if held.is_empty() {
            "none".to_string()
        } else {
            held.join(", ")
        };
        return Err(format!(
            "the step asks for {gpu_type} GPUs but its job holds none here (it holds: {held})"
        ));
    }
    Ok(pool)
}

/// Shape (both-set, zero) is `step_shape::check`'s job; this only scales the
/// chosen form and checks it fits the job's own share on this node.
fn resolve_step_cpus(
    request: &StepResources,
    placement: StepPlacement,
    gpus: Option<usize>,
    job_cpus: u32,
) -> Result<Option<u32>, String> {
    let cpus = match (request.cpus_per_task, request.cpus_per_gpu) {
        (Some(per_task), None) => per_task.saturating_mul(placement.tasks_on_node.max(1)),
        (None, Some(per_gpu)) => {
            let gpus = step_gpus_for("--cpus-per-gpu", gpus)?;
            per_gpu.saturating_mul(gpus)
        }
        _ => return Ok(None),
    };
    if job_cpus > 0 && cpus > job_cpus {
        return Err(format!(
            "the step asks for {cpus} CPUs on this node but its job holds {job_cpus}"
        ));
    }
    Ok(Some(cpus))
}

/// Shape (form count, zero) is `step_shape::check`'s job; this only scales the
/// chosen form and checks it fits the job's own share on this node.
fn resolve_step_memory(
    request: &StepResources,
    placement: StepPlacement,
    cpus: Option<u32>,
    gpus: Option<usize>,
    job_memory_mb: u64,
    job_memory_ceiling_mb: u64,
) -> Result<Option<u64>, String> {
    let memory_mb = if let Some(mb) = request.memory_mb {
        mb
    } else if let Some(per_cpu) = request.memory_per_cpu_mb {
        let cpus = cpus.unwrap_or(placement.tasks_on_node.max(1));
        per_cpu.saturating_mul(u64::from(cpus))
    } else if let Some(per_gpu) = request.memory_per_gpu_mb {
        let gpus = step_gpus_for("--mem-per-gpu", gpus)?;
        per_gpu.saturating_mul(u64::from(gpus))
    } else {
        return Ok(None);
    };
    // A job with no budget of its own is still capped at the node's ceiling
    // (Slurm parity), so a typed step is checked against that instead.
    let budget = if job_memory_mb > 0 {
        job_memory_mb
    } else {
        job_memory_ceiling_mb
    };
    if budget > 0 && memory_mb > budget {
        return Err(format!(
            "the step asks for {memory_mb} MiB on this node but its job holds {budget} MiB"
        ));
    }
    Ok(Some(memory_mb))
}

/// The step's GPUs on this node, which a per-GPU option needs to be sized.
/// Shape (whether a GPU form was even given) is `step_shape::check`'s job;
/// this only reads the count `resolve_gpu_count` already resolved.
fn step_gpus_for(option: &str, gpus: Option<usize>) -> Result<u32, String> {
    match gpus {
        Some(count) if count > 0 => Ok(u32::try_from(count).unwrap_or(u32::MAX)),
        _ => Err(format!(
            "{option} needs the step to ask for GPUs on this node"
        )),
    }
}

/// Form count and `--gpus` divisibility are `step_shape::check`'s job; this
/// only turns the chosen form into a per-node count.
fn resolve_gpu_count(
    request: &StepResources,
    placement: StepPlacement,
) -> Result<Option<usize>, String> {
    let per_node = if let Some(per_node) = request.gpus_per_node {
        per_node
    } else if let Some(per_task) = request.gpus_per_task {
        per_task.saturating_mul(placement.tasks_on_node.max(1))
    } else if let Some(total) = request.gpus {
        total / placement.step_nodes.max(1)
    } else {
        return Ok(None);
    };
    Ok(Some(per_node as usize))
}

/// Why this node cannot enforce `request`, if it cannot. A step that asked for
/// its own limits is refused rather than run with the job's.
pub fn unenforceable(
    request: &NodeRequest,
    cgroup: &CgroupConfig,
    spurd_is_root: bool,
) -> Option<&'static str> {
    if !cgroup.enabled {
        return Some("cgroups are disabled ([cgroup] enabled = false)");
    }
    if !spurd_is_root {
        return Some("spurd is not running as root");
    }
    if request.memory_mb.is_some() && !(cgroup.constrain_ram_space || cgroup.constrain_swap) {
        return Some("memory limits are disabled ([cgroup] constrain_ram_space = false)");
    }
    if request.gpus.is_some() && !cgroup.constrain_devices {
        return Some("device limits are disabled ([cgroup] constrain_devices = false)");
    }
    None
}

/// Identifies one step launch: job, run attempt, step.
pub type StepKey = (u32, u32, StepId);

/// The step does not fit beside what the job's other typed steps hold now.
#[derive(Debug, PartialEq, Eq)]
pub struct ShareExhausted(pub String);

impl std::fmt::Display for ShareExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Shares that launching steps have taken but not yet handed to a supervisor,
/// whose descriptor holds them from then on.
#[derive(Clone, Debug, Default)]
pub struct PendingStepShares(Arc<PendingStepSharesInner>);

/// A token per entry, so a reservation superseded by a fresh one for the same
/// key (the earlier launch lost a race, or a claim was retaken) can never
/// erase its successor's entry on drop.
#[derive(Debug, Default)]
struct PendingStepSharesInner {
    pending: Mutex<HashMap<StepKey, (StepLimits, u64)>>,
    next_token: std::sync::atomic::AtomicU64,
}

/// Where a pinned step's CPUs may come from: the job's CPUs on this node,
/// each GPU's reserved CPUs (core layout), and the CPUs near each GPU.
#[derive(Debug, Default)]
pub struct PinSource {
    pub job_cpus: Vec<u32>,
    pub reserved_of: Vec<(u64, Vec<u32>)>,
    pub near_of: Vec<(u64, Vec<u32>)>,
}

/// `count` of the job's CPUs not in `taken`: the step GPUs' reserved CPUs
/// first, then CPUs near those GPUs, then the rest. Empty when too few are free:
/// the step then holds its CPUs by count and shares the job's cpuset.
pub fn pick_step_cpus(pin: &PinSource, gpus: &[u64], taken: &HashSet<u32>, count: u32) -> Vec<u32> {
    let of = |table: &[(u64, Vec<u32>)]| -> HashSet<u32> {
        table
            .iter()
            .filter(|(gpu, _)| gpus.contains(gpu))
            .flat_map(|(_, cpus)| cpus.iter().copied())
            .collect()
    };
    let (reserved, near) = (of(&pin.reserved_of), of(&pin.near_of));
    let mut free: Vec<u32> = pin
        .job_cpus
        .iter()
        .copied()
        .filter(|cpu| !taken.contains(cpu))
        .collect();
    if free.len() < count as usize {
        return Vec::new();
    }
    free.sort_by_key(|cpu| (!reserved.contains(cpu), !near.contains(cpu), *cpu));
    let mut picked = free[..count as usize].to_vec();
    picked.sort_unstable();
    picked
}

impl PendingStepShares {
    pub fn reserve(
        &self,
        key: StepKey,
        job: &JobShare<'_>,
        request: &NodeRequest,
        held: impl IntoIterator<Item = (StepKey, StepLimits)>,
    ) -> Result<StepShareReservation, ShareExhausted> {
        self.reserve_pinned(key, job, request, held, None)
    }

    /// Take `request` beside `held` and other pending launches of the same
    /// attempt (see crate AGENTS.md "PendingStepShares"). Caller must keep
    /// `held`'s source locked across this call, so no launch moves unseen.
    pub fn reserve_pinned(
        &self,
        key: StepKey,
        job: &JobShare<'_>,
        request: &NodeRequest,
        held: impl IntoIterator<Item = (StepKey, StepLimits)>,
        pin: Option<&PinSource>,
    ) -> Result<StepShareReservation, ShareExhausted> {
        let mut pending = self.0.pending.lock().unwrap_or_else(|e| e.into_inner());
        let (job_id, run_attempt, _) = key;
        let mut by_step: HashMap<StepKey, StepLimits> = held
            .into_iter()
            .filter(|(other, limits)| *other != key && !limits.overlap)
            .collect();
        by_step.extend(
            pending
                .iter()
                .filter(|(other, _)| other.0 == job_id && other.1 == run_attempt && **other != key)
                .map(|(other, (limits, _token))| (*other, limits.clone())),
        );
        let taken: Vec<StepLimits> = by_step.into_values().collect();
        check_room(
            "CPUs",
            request.cpus.map(u64::from),
            u64::from(job.cpus),
            false,
            || taken.iter().filter_map(|l| l.cpus).map(u64::from).sum(),
        )?;
        // A job with no memory budget of its own still shares the node's
        // ceiling among its steps (Slurm parity with `resolve_step_memory`).
        let memory_total = if job.memory_mb > 0 {
            job.memory_mb
        } else {
            job.memory_ceiling_mb
        };
        check_room(
            "MiB of memory",
            request.memory_mb,
            memory_total,
            job.memory_mb == 0,
            || taken.iter().filter_map(|l| l.memory_mb).sum(),
        )?;
        let gpus = match request.gpus {
            Some(count) => Some(free_gpus(&request.gpu_pool, &taken, count)?),
            None => None,
        };
        let cpu_ids = match (pin, request.cpus, gpus.as_deref()) {
            (Some(pin), Some(cpus), Some(step_gpus)) if !step_gpus.is_empty() => {
                let taken: HashSet<u32> = taken
                    .iter()
                    .flat_map(|l| l.cpu_ids.iter().copied())
                    .collect();
                pick_step_cpus(pin, step_gpus, &taken, cpus)
            }
            _ => Vec::new(),
        };
        let limits = StepLimits {
            cpus: request.cpus,
            memory_mb: request.memory_mb,
            gpus,
            cpu_ids,
            overlap: false,
        };
        let token = self
            .0
            .next_token
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        pending.insert(key, (limits.clone(), token));
        Ok(StepShareReservation {
            pending: self.clone(),
            key,
            limits,
            token,
        })
    }
}

/// `wanted` of a resource fits beside what other steps hold; a job without a
/// budget for it (0) bounds nothing. `capped` says `total` came from the
/// node's ceiling rather than the job's own budget, for the refusal message.
fn check_room(
    what: &str,
    wanted: Option<u64>,
    total: u64,
    capped: bool,
    held: impl FnOnce() -> u64,
) -> Result<(), ShareExhausted> {
    let Some(wanted) = wanted.filter(|_| total > 0) else {
        return Ok(());
    };
    let free = total.saturating_sub(held());
    if wanted > free {
        if capped {
            return Err(ShareExhausted(format!(
                "the step asks for {wanted} {what} but only {free} of the {total} its job may \
                 use on this node (the node's schedulable memory) are not held by other steps"
            )));
        }
        return Err(ShareExhausted(format!(
            "the step asks for {wanted} {what} but only {free} of its job's {total} on this node \
             are not held by other steps"
        )));
    }
    Ok(())
}

fn free_gpus(pool: &[u64], taken: &[StepLimits], count: usize) -> Result<Vec<u64>, ShareExhausted> {
    let held: HashSet<u64> = taken
        .iter()
        .filter_map(|l| l.gpus.as_ref())
        .flatten()
        .copied()
        .collect();
    let free: Vec<u64> = pool.iter().copied().filter(|g| !held.contains(g)).collect();
    if free.len() < count {
        return Err(ShareExhausted(format!(
            "the step asks for {count} GPUs but only {} of its job's {} GPUs on this node are \
             not held by other steps",
            free.len(),
            pool.len()
        )));
    }
    Ok(free[..count].to_vec())
}

/// Keeps a launching step's share out of other steps' reach until dropped.
#[derive(Debug)]
pub struct StepShareReservation {
    pending: PendingStepShares,
    key: StepKey,
    limits: StepLimits,
    token: u64,
}

impl StepShareReservation {
    pub fn limits(&self) -> &StepLimits {
        &self.limits
    }
}

impl Drop for StepShareReservation {
    fn drop(&mut self) {
        let mut pending = self
            .pending
            .0
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // A reservation for this key may have been superseded by a fresh one
        // (e.g. a claim retaken after this one was already handed off); only
        // remove the entry this reservation itself still owns.
        if let std::collections::hash_map::Entry::Occupied(entry) = pending.entry(self.key) {
            if entry.get().1 == self.token {
                entry.remove();
            }
        }
    }
}

/// How long a claimed share waits for its `RunCommand`; bounds what a
/// controller that died between the two phases can keep from a job's other
/// steps.
pub const STEP_CLAIM_TTL: std::time::Duration = std::time::Duration::from_secs(120);

/// Upper bound on a controller-requested claim TTL, so a misbehaving or
/// compromised controller cannot pin a share indefinitely.
pub const MAX_STEP_CLAIM_TTL: std::time::Duration = std::time::Duration::from_secs(1800);

/// Clamps a controller-requested claim TTL to `[STEP_CLAIM_TTL, MAX_STEP_CLAIM_TTL]`;
/// 0 (no request) clamps up to the default.
pub fn claim_ttl(requested_secs: u32) -> std::time::Duration {
    std::time::Duration::from_secs(requested_secs.into()).clamp(STEP_CLAIM_TTL, MAX_STEP_CLAIM_TTL)
}

/// Typed-step shares claimed ahead of launch (two-phase dispatch), held
/// until `RunCommand` consumes them, released, or expired.
#[derive(Clone, Debug, Default)]
pub struct StepClaims(Arc<Mutex<HashMap<StepKey, (StepShareReservation, std::time::Instant)>>>);

impl StepClaims {
    pub fn hold(
        &self,
        key: StepKey,
        reservation: StepShareReservation,
        now: std::time::Instant,
        ttl: std::time::Duration,
    ) {
        let replaced = self.lock().insert(key, (reservation, now + ttl));
        drop(replaced);
    }

    pub fn take(&self, key: StepKey) -> Option<StepShareReservation> {
        self.lock().remove(&key).map(|(r, _)| r)
    }

    /// Drops every claim of `(job_id, step_id)`; `run_attempt == 0` names any attempt.
    pub fn release(&self, job_id: u32, run_attempt: u32, step_id: StepId) {
        let dropped: Vec<_> = {
            let mut claims = self.lock();
            let keys: Vec<StepKey> = claims
                .keys()
                .copied()
                .filter(|(j, a, s)| {
                    *j == job_id && *s == step_id && (run_attempt == 0 || *a == run_attempt)
                })
                .collect();
            keys.into_iter().filter_map(|k| claims.remove(&k)).collect()
        };
        drop(dropped);
    }

    /// The instant a held claim expires at, for asserting a controller-sized TTL.
    #[cfg(test)]
    pub fn deadline(&self, key: StepKey) -> Option<std::time::Instant> {
        self.lock().get(&key).map(|(_, until)| *until)
    }

    pub fn expire(&self, now: std::time::Instant) {
        let expired: Vec<_> = {
            let mut claims = self.lock();
            let keys: Vec<StepKey> = claims
                .iter()
                .filter(|(_, (_, until))| *until <= now)
                .map(|(k, _)| *k)
                .collect();
            keys.into_iter().filter_map(|k| claims.remove(&k)).collect()
        };
        drop(expired);
    }

    fn lock(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<StepKey, (StepShareReservation, std::time::Instant)>>
    {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(gpus: &[u64]) -> JobShare<'_> {
        JobShare {
            cpus: 16,
            memory_mb: 65536,
            memory_ceiling_mb: 0,
            gpus,
            gpu_types: &[],
        }
    }

    const ONE_NODE_TWO_TASKS: StepPlacement = StepPlacement {
        tasks_on_node: 2,
        step_nodes: 1,
    };

    #[test]
    fn an_empty_request_resolves_to_nothing_of_its_own() {
        let resolved =
            resolve(&StepResources::default(), ONE_NODE_TWO_TASKS, &job(&[1, 2])).unwrap();
        assert!(resolved.is_empty());
    }

    #[test]
    fn cpus_scale_with_the_tasks_on_this_node() {
        let request = StepResources {
            cpus_per_task: Some(3),
            ..Default::default()
        };
        let resolved = resolve(&request, ONE_NODE_TWO_TASKS, &job(&[])).unwrap();
        assert_eq!(resolved.cpus, Some(6));
    }

    #[test]
    fn a_request_larger_than_the_jobs_share_is_rejected() {
        let cpus = StepResources {
            cpus_per_task: Some(9),
            ..Default::default()
        };
        assert!(resolve(&cpus, ONE_NODE_TWO_TASKS, &job(&[]))
            .unwrap_err()
            .contains("18 CPUs"));

        let memory = StepResources {
            memory_mb: Some(65537),
            ..Default::default()
        };
        assert!(resolve(&memory, ONE_NODE_TWO_TASKS, &job(&[]))
            .unwrap_err()
            .contains("65537 MiB"));

        let gpus = StepResources {
            gpus_per_node: Some(3),
            ..Default::default()
        };
        assert!(resolve(&gpus, ONE_NODE_TWO_TASKS, &job(&[1, 2]))
            .unwrap_err()
            .contains("holds 2"));
    }

    #[test]
    fn a_job_without_a_budget_bounds_neither_cpus_nor_memory() {
        let request = StepResources {
            cpus_per_task: Some(64),
            memory_mb: Some(1 << 20),
            ..Default::default()
        };
        let unbounded = JobShare {
            cpus: 0,
            memory_mb: 0,
            memory_ceiling_mb: 0,
            gpus: &[],
            gpu_types: &[],
        };
        let resolved = resolve(&request, ONE_NODE_TWO_TASKS, &unbounded).unwrap();
        assert_eq!(resolved.cpus, Some(128));
        assert_eq!(resolved.memory_mb, Some(1 << 20));
    }

    #[test]
    fn a_job_without_a_budget_still_bounds_memory_by_the_node_ceiling() {
        let request = StepResources {
            memory_mb: Some(8192),
            ..Default::default()
        };
        let ceilinged = JobShare {
            cpus: 0,
            memory_mb: 0,
            memory_ceiling_mb: 4096,
            gpus: &[],
            gpu_types: &[],
        };
        assert!(resolve(&request, ONE_NODE_TWO_TASKS, &ceilinged)
            .unwrap_err()
            .contains("8192 MiB"));

        let within_ceiling = StepResources {
            memory_mb: Some(4096),
            ..Default::default()
        };
        let resolved = resolve(&within_ceiling, ONE_NODE_TWO_TASKS, &ceilinged).unwrap();
        assert_eq!(resolved.memory_mb, Some(4096));
    }

    #[test]
    fn zero_cpus_or_memory_is_rejected_rather_than_read_as_unlimited() {
        for request in [
            StepResources {
                cpus_per_task: Some(0),
                ..Default::default()
            },
            StepResources {
                memory_mb: Some(0),
                ..Default::default()
            },
        ] {
            assert!(resolve(&request, ONE_NODE_TWO_TASKS, &job(&[])).is_err());
        }
    }

    #[test]
    fn each_gpu_form_resolves_to_a_count_for_this_node() {
        let two_nodes = StepPlacement {
            tasks_on_node: 2,
            step_nodes: 2,
        };
        let gpus = [1, 2, 3, 4];
        let per_node = StepResources {
            gpus_per_node: Some(3),
            ..Default::default()
        };
        assert_eq!(
            resolve(&per_node, two_nodes, &job(&gpus)).unwrap().gpus,
            Some(3)
        );
        let per_task = StepResources {
            gpus_per_task: Some(2),
            ..Default::default()
        };
        assert_eq!(
            resolve(&per_task, two_nodes, &job(&gpus)).unwrap().gpus,
            Some(4)
        );
        let total = StepResources {
            gpus: Some(4),
            ..Default::default()
        };
        assert_eq!(
            resolve(&total, two_nodes, &job(&gpus)).unwrap().gpus,
            Some(2)
        );
    }

    #[test]
    fn zero_gpus_is_a_request_for_none() {
        let request = StepResources {
            gpus: Some(0),
            ..Default::default()
        };
        assert_eq!(
            resolve(&request, ONE_NODE_TWO_TASKS, &job(&[1, 2]))
                .unwrap()
                .gpus,
            Some(0)
        );
    }

    #[test]
    fn a_total_that_does_not_split_evenly_is_rejected() {
        let request = StepResources {
            gpus: Some(3),
            ..Default::default()
        };
        let two_nodes = StepPlacement {
            tasks_on_node: 1,
            step_nodes: 2,
        };
        let err = resolve(&request, two_nodes, &job(&[1, 2])).unwrap_err();
        assert!(err.contains("--gpus-per-node"), "{err}");
    }

    #[test]
    fn two_gpu_forms_are_rejected() {
        let request = StepResources {
            gpus: Some(1),
            gpus_per_node: Some(1),
            ..Default::default()
        };
        assert!(resolve(&request, ONE_NODE_TWO_TASKS, &job(&[1, 2])).is_err());
    }

    #[test]
    fn enforcement_needs_cgroups_root_and_the_requested_constraint() {
        let request = NodeRequest {
            memory_mb: Some(1024),
            gpus: Some(1),
            ..Default::default()
        };
        let enforcing = CgroupConfig::default();
        assert_eq!(unenforceable(&request, &enforcing, true), None);
        assert!(unenforceable(&request, &enforcing, false).is_some());

        let disabled = CgroupConfig {
            enabled: false,
            ..Default::default()
        };
        assert!(unenforceable(&request, &disabled, true).is_some());

        let no_memory = CgroupConfig {
            constrain_ram_space: false,
            ..Default::default()
        };
        assert!(unenforceable(&request, &no_memory, true)
            .unwrap()
            .contains("memory"));

        let no_devices = CgroupConfig {
            constrain_devices: false,
            ..Default::default()
        };
        assert!(unenforceable(&request, &no_devices, true)
            .unwrap()
            .contains("device"));
        let cpus_only = NodeRequest {
            cpus: Some(2),
            ..Default::default()
        };
        assert_eq!(unenforceable(&cpus_only, &no_devices, true), None);
    }

    fn wants(
        cpus: Option<u32>,
        memory_mb: Option<u64>,
        gpus: Option<usize>,
        pool: &[u64],
    ) -> NodeRequest {
        NodeRequest {
            cpus,
            memory_mb,
            gpus,
            gpu_pool: pool.to_vec(),
        }
    }

    fn holding(cpus: u32, gpus: &[u64]) -> StepLimits {
        StepLimits {
            cpus: Some(cpus),
            gpus: Some(gpus.to_vec()),
            ..Default::default()
        }
    }

    #[test]
    fn reservations_skip_held_and_pending_gpus_of_the_same_job_attempt() {
        let pending = PendingStepShares::default();
        let pool = [10, 11, 12, 13];
        let held = || [((7, 0, 9), holding(0, &[10]))];
        let first = pending
            .reserve(
                (7, 0, 1),
                &job(&pool),
                &wants(None, None, Some(1), &pool),
                held(),
            )
            .unwrap();
        assert_eq!(first.limits().gpus.as_deref(), Some(&[11][..]));
        let second = pending
            .reserve(
                (7, 0, 2),
                &job(&pool),
                &wants(None, None, Some(2), &pool),
                held(),
            )
            .unwrap();
        assert_eq!(second.limits().gpus.as_deref(), Some(&[12, 13][..]));
        let err = pending
            .reserve(
                (7, 0, 3),
                &job(&pool),
                &wants(None, None, Some(1), &pool),
                held(),
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("only 0 of its job's 4 GPUs"),
            "{err}"
        );
    }

    #[test]
    fn cpus_and_memory_are_held_like_gpus() {
        let pending = PendingStepShares::default();
        let first = pending
            .reserve(
                (7, 0, 1),
                &job(&[]),
                &wants(Some(10), Some(40_000), None, &[]),
                [],
            )
            .unwrap();
        let cpus = pending
            .reserve((7, 0, 2), &job(&[]), &wants(Some(7), None, None, &[]), [])
            .unwrap_err();
        assert!(cpus.to_string().contains("7 CPUs but only 6"), "{cpus}");
        let memory = pending
            .reserve(
                (7, 0, 2),
                &job(&[]),
                &wants(None, Some(30_000), None, &[]),
                [],
            )
            .unwrap_err();
        assert!(memory.to_string().contains("MiB of memory"), "{memory}");
        drop(first);
        pending
            .reserve(
                (7, 0, 2),
                &job(&[]),
                &wants(Some(16), Some(65_536), None, &[]),
                [],
            )
            .expect("fits once the first step ends");
    }

    #[test]
    fn a_job_without_a_budget_shares_the_node_ceiling_among_its_steps() {
        let pending = PendingStepShares::default();
        let ceilinged = JobShare {
            cpus: 0,
            memory_mb: 0,
            memory_ceiling_mb: 8000,
            gpus: &[],
            gpu_types: &[],
        };
        let _first = pending
            .reserve(
                (7, 0, 1),
                &ceilinged,
                &wants(None, Some(6000), None, &[]),
                [],
            )
            .unwrap();
        let err = pending
            .reserve(
                (7, 0, 2),
                &ceilinged,
                &wants(None, Some(6000), None, &[]),
                [],
            )
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("only 2000 of the 8000 its job may use on this node (the node's schedulable memory)"),
            "{err}"
        );
    }

    #[test]
    fn a_job_with_no_budget_and_no_ceiling_is_still_unchecked() {
        let pending = PendingStepShares::default();
        let unbounded = JobShare {
            cpus: 0,
            memory_mb: 0,
            memory_ceiling_mb: 0,
            gpus: &[],
            gpu_types: &[],
        };
        pending
            .reserve(
                (7, 0, 1),
                &unbounded,
                &wants(None, Some(u64::MAX), None, &[]),
                [],
            )
            .expect("no budget and no ceiling bounds nothing");
    }

    #[test]
    fn live_steps_hold_through_their_descriptors_but_overlap_steps_do_not() {
        let pending = PendingStepShares::default();
        let live = StepLimits {
            cpus: Some(12),
            ..Default::default()
        };
        let overlap = StepLimits {
            cpus: Some(16),
            overlap: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&live).unwrap();
        let restored: StepLimits = serde_json::from_str(&json).unwrap();
        assert!(pending
            .reserve(
                (7, 0, 2),
                &job(&[]),
                &wants(Some(5), None, None, &[]),
                [((7, 0, 1), restored)]
            )
            .is_err());
        assert!(pending
            .reserve(
                (7, 0, 3),
                &job(&[]),
                &wants(Some(16), None, None, &[]),
                [((7, 0, 1), overlap)]
            )
            .is_ok());
    }

    #[test]
    fn a_descriptor_from_an_older_agent_still_loads() {
        let old: StepLimits = serde_json::from_str(r#"{"cpus":2,"gpus":[5]}"#).unwrap();
        assert!(old.cpu_ids.is_empty());
        assert!(!old.overlap);
    }

    #[test]
    fn another_job_or_attempt_does_not_hold_this_ones_share() {
        let pending = PendingStepShares::default();
        let pool = [10, 11];
        let all = wants(Some(16), None, Some(2), &pool);
        let _other_job = pending.reserve((8, 0, 1), &job(&pool), &all, []).unwrap();
        let _other_attempt = pending.reserve((7, 1, 1), &job(&pool), &all, []).unwrap();
        let mine = pending.reserve((7, 0, 1), &job(&pool), &all, []).unwrap();
        assert_eq!(mine.limits().gpus.as_deref(), Some(&[10, 11][..]));
    }

    #[test]
    fn per_gpu_options_size_from_the_steps_own_gpus() {
        let request = StepResources {
            gpus_per_node: Some(2),
            cpus_per_gpu: Some(4),
            memory_per_gpu_mb: Some(1000),
            ..Default::default()
        };
        let resolved = resolve(&request, ONE_NODE_TWO_TASKS, &job(&[1, 2, 3])).unwrap();
        assert_eq!(resolved.cpus, Some(8));
        assert_eq!(resolved.memory_mb, Some(2000));

        let no_gpus = StepResources {
            cpus_per_gpu: Some(4),
            ..Default::default()
        };
        assert!(resolve(&no_gpus, ONE_NODE_TWO_TASKS, &job(&[1]))
            .unwrap_err()
            .contains("needs the step to ask for GPUs"));
    }

    #[test]
    fn memory_per_cpu_scales_with_the_steps_cpus() {
        let request = StepResources {
            cpus_per_task: Some(3),
            memory_per_cpu_mb: Some(100),
            ..Default::default()
        };
        let resolved = resolve(&request, ONE_NODE_TWO_TASKS, &job(&[])).unwrap();
        assert_eq!(resolved.memory_mb, Some(600));
    }

    /// What srun's CPU inheritance (a typed step with no CPU flag takes the
    /// job's exported `-c`) relies on: `--mem-per-cpu` sizes from the step's
    /// resolved CPUs, whatever gave them.
    #[test]
    fn mem_per_cpu_sizes_by_the_inherited_cpus_per_task() {
        let request = StepResources {
            cpus_per_task: Some(4),
            memory_per_cpu_mb: Some(1024),
            ..Default::default()
        };
        let one_task_one_node = StepPlacement {
            tasks_on_node: 1,
            step_nodes: 1,
        };
        let resolved = resolve(&request, one_task_one_node, &job(&[])).unwrap();
        assert_eq!(resolved.memory_mb, Some(4096));
    }

    #[test]
    fn a_gpu_type_picks_only_the_jobs_gpus_of_that_type() {
        let types = [(1, "MI300X".to_string()), (2, "mi250".to_string())];
        let share = JobShare {
            cpus: 16,
            memory_mb: 65536,
            memory_ceiling_mb: 0,
            gpus: &[1, 2],
            gpu_types: &types,
        };
        let typed = |ty: &str| StepResources {
            gpus_per_node: Some(1),
            gpu_type: ty.into(),
            ..Default::default()
        };
        let resolved = resolve(&typed("mi300x"), ONE_NODE_TWO_TASKS, &share).unwrap();
        assert_eq!(resolved.gpu_pool, vec![1]);
        let err = resolve(&typed("h100"), ONE_NODE_TWO_TASKS, &share).unwrap_err();
        assert!(err.contains("MI300X, mi250"), "{err}");
    }

    fn pin_source() -> PinSource {
        PinSource {
            job_cpus: vec![0, 1, 2, 3, 4, 5, 6, 7],
            reserved_of: vec![(10, vec![6, 7]), (11, vec![0, 1])],
            near_of: vec![(10, vec![4, 5, 6, 7]), (11, vec![0, 1, 2, 3])],
        }
    }

    #[test]
    fn a_pinned_step_takes_its_gpus_reserved_cpus_then_nearby_then_the_rest() {
        let pin = pin_source();
        assert_eq!(pick_step_cpus(&pin, &[10], &HashSet::new(), 2), vec![6, 7]);
        assert_eq!(
            pick_step_cpus(&pin, &[10], &HashSet::new(), 4),
            vec![4, 5, 6, 7]
        );
        assert_eq!(
            pick_step_cpus(&pin, &[10], &HashSet::new(), 5),
            vec![0, 4, 5, 6, 7]
        );
    }

    #[test]
    fn pinned_steps_never_share_cpus_and_a_short_pick_does_not_pin() {
        let pin = pin_source();
        let taken: HashSet<u32> = [6, 7].into();
        assert_eq!(pick_step_cpus(&pin, &[10], &taken, 2), vec![4, 5]);
        let all_but_one: HashSet<u32> = (0..7).collect();
        assert!(pick_step_cpus(&pin, &[10], &all_but_one, 2).is_empty());
    }

    #[test]
    fn a_reserved_typed_gpu_step_is_pinned_apart_from_its_siblings() {
        let pending = PendingStepShares::default();
        let pool = [10, 11];
        let pin = pin_source();
        let one = wants(Some(2), None, Some(1), &pool);
        let first = pending
            .reserve_pinned((7, 0, 1), &job(&pool), &one, [], Some(&pin))
            .unwrap();
        assert_eq!(first.limits().cpu_ids, vec![6, 7]);
        let second = pending
            .reserve_pinned((7, 0, 2), &job(&pool), &one, [], Some(&pin))
            .unwrap();
        assert_eq!(second.limits().gpus.as_deref(), Some(&[11][..]));
        assert_eq!(second.limits().cpu_ids, vec![0, 1]);
    }

    #[test]
    fn dropping_a_reservation_frees_its_share() {
        let pending = PendingStepShares::default();
        let pool = [10];
        let one = wants(None, None, Some(1), &pool);
        let held = pending.reserve((7, 0, 1), &job(&pool), &one, []).unwrap();
        assert!(pending.reserve((7, 0, 2), &job(&pool), &one, []).is_err());
        drop(held);
        assert!(pending.reserve((7, 0, 2), &job(&pool), &one, []).is_ok());
    }

    #[test]
    fn a_zero_gpu_reservation_always_succeeds_and_holds_nothing() {
        let pending = PendingStepShares::default();
        let pool = [10];
        let _all = pending
            .reserve(
                (7, 0, 1),
                &job(&pool),
                &wants(None, None, Some(1), &pool),
                [],
            )
            .unwrap();
        let none = pending
            .reserve(
                (7, 0, 2),
                &job(&pool),
                &wants(None, None, Some(0), &pool),
                [],
            )
            .unwrap();
        assert_eq!(none.limits().gpus.as_deref(), Some(&[][..]));
    }

    #[test]
    fn a_superseded_reservations_drop_cannot_erase_its_successor() {
        let pending = PendingStepShares::default();
        let pool: [u64; 0] = [];
        let eight = wants(Some(8), None, None, &pool);
        let key = (7, 0, 1);
        let first = pending.reserve(key, &job(&pool), &eight, []).unwrap();
        let second = pending.reserve(key, &job(&pool), &eight, []).unwrap();
        drop(first);
        // If dropping `first` had erased the shared key's entry, this would
        // see the job's whole 16 CPUs free and succeed; `second`'s hold must
        // still count.
        let nine = wants(Some(9), None, None, &pool);
        assert!(pending.reserve((7, 0, 2), &job(&pool), &nine, []).is_err());
        drop(second);
    }

    #[test]
    fn a_claimed_share_blocks_other_steps_until_taken() {
        let pending = PendingStepShares::default();
        let claims = StepClaims::default();
        let pool: [u64; 0] = [];
        let job = job(&pool);
        let sixteen = wants(Some(16), None, None, &pool);
        let key1 = (7, 0, 1);
        let reservation = pending.reserve(key1, &job, &sixteen, []).unwrap();
        claims.hold(key1, reservation, std::time::Instant::now(), STEP_CLAIM_TTL);

        let one = wants(Some(1), None, None, &pool);
        assert!(pending.reserve((7, 0, 2), &job, &one, []).is_err());

        let held = claims
            .take(key1)
            .expect("the claim was held, not just checked");
        drop(held);
        assert!(pending.reserve((7, 0, 2), &job, &one, []).is_ok());
    }

    #[test]
    fn a_claim_expires_after_its_ttl() {
        let pending = PendingStepShares::default();
        let claims = StepClaims::default();
        let pool: [u64; 0] = [];
        let job = job(&pool);
        let sixteen = wants(Some(16), None, None, &pool);
        let key1 = (7, 0, 1);
        let now = std::time::Instant::now();
        let reservation = pending.reserve(key1, &job, &sixteen, []).unwrap();
        claims.hold(key1, reservation, now, STEP_CLAIM_TTL);

        claims.expire(now + STEP_CLAIM_TTL - std::time::Duration::from_millis(1));
        let one = wants(Some(1), None, None, &pool);
        assert!(
            pending.reserve((7, 0, 2), &job, &one, []).is_err(),
            "not yet expired"
        );

        claims.expire(now + STEP_CLAIM_TTL);
        assert!(
            pending.reserve((7, 0, 2), &job, &one, []).is_ok(),
            "an expired claim must free its share"
        );
    }

    #[test]
    fn release_with_attempt_zero_drops_every_attempt() {
        let pending = PendingStepShares::default();
        let claims = StepClaims::default();
        let pool: [u64; 0] = [];
        let job = job(&pool);
        let eight = wants(Some(8), None, None, &pool);
        let now = std::time::Instant::now();
        let r1 = pending.reserve((7, 1, 3), &job, &eight, []).unwrap();
        let r2 = pending.reserve((7, 2, 3), &job, &eight, []).unwrap();
        claims.hold((7, 1, 3), r1, now, STEP_CLAIM_TTL);
        claims.hold((7, 2, 3), r2, now, STEP_CLAIM_TTL);

        claims.release(7, 0, 3);

        assert!(claims.take((7, 1, 3)).is_none());
        assert!(claims.take((7, 2, 3)).is_none());
    }

    #[test]
    fn claim_ttl_clamps_to_the_default_and_the_maximum() {
        assert_eq!(claim_ttl(0), STEP_CLAIM_TTL);
        assert_eq!(claim_ttl(630), std::time::Duration::from_secs(630));
        assert_eq!(claim_ttl(u32::MAX), MAX_STEP_CLAIM_TTL);
    }

    #[test]
    fn a_claim_survives_until_its_controller_sized_ttl() {
        let pending = PendingStepShares::default();
        let claims = StepClaims::default();
        let pool: [u64; 0] = [];
        let job = job(&pool);
        let sixteen = wants(Some(16), None, None, &pool);
        let key1 = (7, 0, 1);
        let now = std::time::Instant::now();
        let ttl = claim_ttl(630);
        let reservation = pending.reserve(key1, &job, &sixteen, []).unwrap();
        claims.hold(key1, reservation, now, ttl);

        claims.expire(now + std::time::Duration::from_secs(629));
        let one = wants(Some(1), None, None, &pool);
        assert!(
            pending.reserve((7, 0, 2), &job, &one, []).is_err(),
            "not yet expired at 629s"
        );

        claims.expire(now + std::time::Duration::from_secs(630));
        assert!(
            pending.reserve((7, 0, 2), &job, &one, []).is_ok(),
            "a claim held with a 630s TTL must free its share at 630s"
        );
    }
}
