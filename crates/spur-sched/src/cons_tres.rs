// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Consumable TRES resource selection.
//!
//! Tracks resources at core/socket/GPU granularity within a node.
//! This is the equivalent of Slurm's select/cons_tres plugin.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use spur_core::job::RunKey;
use spur_core::resource::{GpuResource, ResourceSet};

/// Why a caller is entitled to hand a run's slice back. A slice is released only
/// on a controller decision, so every ground names the decision it rests on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseGround {
    /// The controller committed the run's completion at this Raft index.
    Acknowledged(u64),
    /// The controller ordered the run to end. Its cancel is its own word that
    /// the run is over, so nothing further has to be acknowledged.
    ControllerCancelled,
    /// The reservation never had anything spawned against it, so there is no
    /// payload to answer for and nothing for the controller to acknowledge.
    NeverSpawned,
    /// The controller answered a claim Raft never had. Its answer is the whole
    /// of the acknowledgement, so there is no committed index to name.
    SettledUnrecordedClaim,
    /// The controller dispatched a newer attempt onto this job id, and that
    /// dispatch is the decision which ends the attempt being displaced.
    SupersededByNewerAttempt,
}

/// Licence to hand a run's slice back. Deliberately not `Clone` and built only
/// through a named ground, so a release cannot be written without saying why.
#[derive(Debug)]
#[must_use = "a warrant does nothing until it is spent on a release"]
pub struct ReleaseWarrant {
    run: RunKey,
    ground: ReleaseGround,
}

impl ReleaseWarrant {
    /// The controller has committed this run's completion. Mint this from the
    /// recorded acknowledgement, never from the agent's own reading of an exit.
    pub fn acknowledged(run: RunKey, release_raft_index: u64) -> Self {
        Self {
            run,
            ground: ReleaseGround::Acknowledged(release_raft_index),
        }
    }

    pub fn controller_cancelled(run: RunKey) -> Self {
        Self {
            run,
            ground: ReleaseGround::ControllerCancelled,
        }
    }

    pub fn never_spawned(run: RunKey) -> Self {
        Self {
            run,
            ground: ReleaseGround::NeverSpawned,
        }
    }

    /// The controller has answered a claim it holds no record of. Mint this only
    /// from that answer, so the audit never reads it as a committed completion.
    pub fn settled_unrecorded_claim(run: RunKey) -> Self {
        Self {
            run,
            ground: ReleaseGround::SettledUnrecordedClaim,
        }
    }

    /// A newer attempt is taking over this job id's reservation. Mint this only
    /// where that attempt has already been judged feasible.
    pub fn superseded_by_newer_attempt(run: RunKey) -> Self {
        Self {
            run,
            ground: ReleaseGround::SupersededByNewerAttempt,
        }
    }

    pub fn run(&self) -> RunKey {
        self.run
    }

    pub fn ground(&self) -> ReleaseGround {
        self.ground
    }
}

impl std::fmt::Display for ReleaseGround {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Acknowledged(index) => write!(f, "acknowledged at raft index {index}"),
            Self::ControllerCancelled => f.write_str("controller cancelled"),
            Self::NeverSpawned => f.write_str("never spawned"),
            Self::SettledUnrecordedClaim => f.write_str("settled an unrecorded claim"),
            Self::SupersededByNewerAttempt => f.write_str("superseded by a newer attempt"),
        }
    }
}

/// Why a reservation could not be made. Distinguished so the caller can map
/// each to the right gRPC status instead of reporting every failure as GPU
/// exhaustion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllocError {
    /// A controller-allocated GPU is unknown to this node or already in use.
    GpusUnavailable,
    /// A LaunchJob is already in flight for this job id (its reservation is
    /// still mid-launch, not yet committed or released). A second concurrent
    /// launch would double-count resources, so it is rejected.
    DuplicateJob,
    /// A newer run_attempt already owns this job id's reservation; this
    /// (older or duplicate) attempt lost the race and must not proceed.
    Superseded,
    /// A core being replayed does not exist on this node (the node's CPU count
    /// shrank since the allocation was made), or another job still holds it.
    CpusUnavailable,
    /// The request would take the node past its total memory.
    MemoryUnavailable,
}

/// Per-node resource allocation state.
/// Tracks which specific cores and GPUs are allocated.
#[derive(Debug, Clone)]
pub struct NodeAllocation {
    pub node_name: String,
    /// Total CPUs on this node.
    pub total_cpus: u32,
    /// Bitmap of allocated CPUs (bit N = core N).
    pub allocated_cpus: Vec<bool>,
    /// Total memory in MB.
    pub total_memory_mb: u64,
    /// Allocated memory in MB.
    pub allocated_memory_mb: u64,
    /// GPU allocations: device_id → allocated?
    pub gpu_allocated: Vec<bool>,
    /// GPU info for type matching.
    pub gpus: Vec<GpuResource>,
    /// Per-job ownership, so an allocation can be released by job id and
    /// orphans reconciled. Source of truth; the bitmaps above are a derived
    /// index for fast free-count queries.
    owners: HashMap<u32, Owned>,
    /// Reserved but not yet committed, with reserve time. Reconcile spares
    /// these until they exceed a TTL so a dropped launch can't pin resources.
    launching: HashMap<u32, Instant>,
}

impl NodeAllocation {
    pub fn new(name: String, resources: &ResourceSet) -> Self {
        let num_gpus = resources.gpus.len();
        Self {
            node_name: name,
            total_cpus: resources.cpus,
            allocated_cpus: vec![false; resources.cpus as usize],
            total_memory_mb: resources.memory_mb,
            allocated_memory_mb: 0,
            gpu_allocated: vec![false; num_gpus],
            gpus: resources.gpus.clone(),
            owners: HashMap::new(),
            launching: HashMap::new(),
        }
    }

    /// Available (unallocated) CPU count.
    pub fn free_cpus(&self) -> u32 {
        self.allocated_cpus.iter().filter(|&&a| !a).count() as u32
    }

    /// Available memory.
    pub fn free_memory_mb(&self) -> u64 {
        self.total_memory_mb
            .saturating_sub(self.allocated_memory_mb)
    }

    /// Device ids of all currently-allocated GPUs (for diagnostics).
    pub fn allocated_gpu_ids(&self) -> Vec<u32> {
        self.gpu_allocated
            .iter()
            .enumerate()
            .filter(|(_, &a)| a)
            .filter_map(|(i, _)| self.gpus.get(i).map(|g| g.device_id))
            .collect()
    }

    /// Job ids owning any of `device_ids`, in job-id order, excluding mid-launch
    /// owners (a launch in flight is a real duplicate, not a stale owner).
    pub fn conflicting_owners(&self, device_ids: &[u32]) -> Vec<u32> {
        let mut owners: Vec<u32> = self
            .owners
            .iter()
            .filter(|(id, _)| !self.launching.contains_key(id))
            .filter(|(_, owned)| owned.result.gpu_ids.iter().any(|g| device_ids.contains(g)))
            .map(|(id, _)| *id)
            .collect();
        // Ordered so a refusal names the same holder each time rather than
        // whichever one the map happened to yield first.
        owners.sort_unstable();
        owners
    }

    /// Committed owners in job-id order. A launch still in flight is a live
    /// duplicate rather than a claim the node cannot explain, so it is left out.
    pub fn committed_owners(&self) -> Vec<u32> {
        let mut owners: Vec<u32> = self
            .owners
            .keys()
            .copied()
            .filter(|id| !self.launching.contains_key(id))
            .collect();
        owners.sort_unstable();
        owners
    }

    /// The attempt and slice recorded for a job here, a launch still in flight
    /// included, so a refusal can describe the claim and not only name it.
    pub fn claim_of(&self, job_id: u32) -> Option<(u32, AllocationResult)> {
        self.owners
            .get(&job_id)
            .map(|owned| (owned.run_attempt, owned.result.clone()))
    }

    /// Available GPU count (optionally filtered by type).
    pub fn free_gpus(&self, gpu_type: Option<&str>) -> u32 {
        self.gpu_allocated
            .iter()
            .enumerate()
            .filter(|(i, allocated)| {
                if **allocated {
                    return false;
                }
                if let Some(gtype) = gpu_type {
                    if gtype != "any"
                        && self.gpus.get(*i).map(|g| g.gpu_type.as_str()) != Some(gtype)
                    {
                        return false;
                    }
                }
                true
            })
            .count() as u32
    }

    /// Free the bitmaps/counters an allocation held. Internal helper: callers
    /// go through `release_job` so per-job ownership stays consistent. GPU ids
    /// are device ids, matched against the node's GPU table the same way
    /// `allocate_for_job` records them.
    fn release(&mut self, alloc: &AllocationResult) {
        for &cpu in &alloc.cpu_ids {
            if let Some(a) = self.allocated_cpus.get_mut(cpu as usize) {
                *a = false;
            }
        }
        self.allocated_memory_mb = self.allocated_memory_mb.saturating_sub(alloc.memory_mb);
        for &device_id in &alloc.gpu_ids {
            if let Some(idx) = self.gpus.iter().position(|g| g.device_id == device_id) {
                self.gpu_allocated[idx] = false;
            }
        }
    }

    /// Reserve resources for a job, keyed by job id. GPU device ids are the
    /// hard gate (unknown or in-use → `GpusUnavailable`); a launch already in
    /// flight for the same job id → `DuplicateJob`. CPU is best-effort since the
    /// controller owns placement. Memory is always accounted so release stays
    /// symmetric. Marked `launching` until `commit_job`/`release_job` so
    /// reconcile spares an in-flight launch. `run_attempt` is recorded so a
    /// later `commit_job` for a different (superseded) attempt is rejected.
    pub fn allocate_for_job(
        &mut self,
        job_id: u32,
        run_attempt: u32,
        cpus: u32,
        memory_mb: u64,
        gpu_device_ids: &[u32],
    ) -> Result<AllocationResult, AllocError> {
        // A launch still in flight (reserved, not yet committed or released) is
        // a genuine concurrent duplicate: a second launch would double-count.
        if self.launching.contains_key(&job_id) {
            return Err(AllocError::DuplicateJob);
        }
        // A committed owner from the same or an older attempt is stale (a prior
        // run's teardown hasn't released yet) — supersede it. A NEWER attempt's
        // owner already won this job id; a late reservation must not clobber it.
        if let Some(existing) = self.owners.get(&job_id) {
            if existing.run_attempt > run_attempt {
                return Err(AllocError::Superseded);
            }
        }
        // Feasibility is judged against what this job would free, so a refusal
        // never drops the reservation it was about to supersede.
        let reclaimable = self
            .owners
            .get(&job_id)
            .map(|owned| owned.result.clone())
            .unwrap_or_else(|| AllocationResult {
                cpu_ids: Vec::new(),
                gpu_ids: Vec::new(),
                memory_mb: 0,
            });
        let free_cpus =
            self.allocated_cpus.iter().filter(|a| !**a).count() + reclaimable.cpu_ids.len();
        if free_cpus < cpus as usize {
            return Err(AllocError::CpusUnavailable);
        }
        // A node that could not read its own memory reports 0. Unknown is not
        // zero: enforcing a ceiling there refuses every job on the node.
        if self.total_memory_mb > 0
            && self
                .allocated_memory_mb
                .saturating_sub(reclaimable.memory_mb)
                .saturating_add(memory_mb)
                > self.total_memory_mb
        {
            return Err(AllocError::MemoryUnavailable);
        }
        let mut gpu_indices = Vec::with_capacity(gpu_device_ids.len());
        for &id in gpu_device_ids {
            let idx = self
                .gpus
                .iter()
                .position(|g| g.device_id == id)
                .ok_or(AllocError::GpusUnavailable)?;
            // A device the outgoing owner holds is free to this job: it is what
            // the reclaim above already counted as available.
            if (self.gpu_allocated[idx] && !reclaimable.gpu_ids.contains(&id))
                || gpu_indices.contains(&idx)
            {
                return Err(AllocError::GpusUnavailable);
            }
            gpu_indices.push(idx);
        }

        // Chosen before anything is marked, so a shortfall refuses instead of
        // serving a short list the caller reads as a full allocation.
        let cpu_ids: Vec<u32> = (0..self.allocated_cpus.len() as u32)
            .filter(|id| !self.allocated_cpus[*id as usize] || reclaimable.cpu_ids.contains(id))
            .take(cpus as usize)
            .collect();
        if cpu_ids.len() < cpus as usize {
            return Err(AllocError::CpusUnavailable);
        }

        // Nothing below can refuse, so a launch that is turned away never drops
        // the reservation it was about to supersede.
        self.drop_owner(ReleaseWarrant::superseded_by_newer_attempt(
            RunKey::any_attempt(job_id),
        ));

        for &id in &cpu_ids {
            self.allocated_cpus[id as usize] = true;
        }
        self.allocated_memory_mb += memory_mb;
        for &idx in &gpu_indices {
            self.gpu_allocated[idx] = true;
        }

        let result = AllocationResult {
            cpu_ids,
            gpu_ids: gpu_device_ids.to_vec(),
            memory_mb,
        };
        self.owners.insert(
            job_id,
            Owned {
                run_attempt,
                result: result.clone(),
            },
        );
        self.launching.insert(job_id, Instant::now());
        Ok(result)
    }

    /// On this node, and either free or already this job's. Narrowing and the
    /// replay check share it so the two can never disagree about one core.
    fn cpu_is_claimable(&self, held: &[u32], cpu: u32) -> bool {
        self.allocated_cpus
            .get(cpu as usize)
            .is_some_and(|&taken| !taken || held.contains(&cpu))
    }

    /// The cores of `cpu_ids` this job may claim: free, or already its own.
    /// Lets a replay under-count rather than be refused over a contested core.
    pub fn claimable_cpu_ids(&self, job_id: u32, cpu_ids: &[u32]) -> Vec<u32> {
        let held: &[u32] = self
            .owners
            .get(&job_id)
            .map_or(&[], |owned| owned.result.cpu_ids.as_slice());
        cpu_ids
            .iter()
            .copied()
            .filter(|&cpu| self.cpu_is_claimable(held, cpu))
            .collect()
    }

    /// Re-record the allocation of a job adopted after an agent restart. Cores
    /// are replayed verbatim; narrow with `claimable_cpu_ids` to drop, not reject.
    pub fn restore_for_job(
        &mut self,
        job_id: u32,
        run_attempt: u32,
        cpu_ids: &[u32],
        memory_mb: u64,
        gpu_device_ids: &[u32],
    ) -> Result<AllocationResult, AllocError> {
        if let Some(existing) = self.owners.get(&job_id) {
            if existing.run_attempt > run_attempt {
                return Err(AllocError::Superseded);
            }
        }
        // Resolved before releasing, so a rejected replay leaves the ledger
        // exactly as it found it rather than dropping the outgoing owner.
        let owned = self.owners.get(&job_id).map(|owned| &owned.result);
        let held_cpus: &[u32] = owned.map_or(&[], |result| result.cpu_ids.as_slice());
        let held_gpus: &[u32] = owned.map_or(&[], |result| result.gpu_ids.as_slice());
        for &cpu in cpu_ids {
            // A core another job still holds would be freed by the first
            // release, handing both jobs an overlapping cpuset.
            if !self.cpu_is_claimable(held_cpus, cpu) {
                return Err(AllocError::CpusUnavailable);
            }
        }
        let mut gpu_indices = Vec::with_capacity(gpu_device_ids.len());
        for &id in gpu_device_ids {
            let idx = self
                .gpus
                .iter()
                .position(|g| g.device_id == id)
                .ok_or(AllocError::GpusUnavailable)?;
            if (self.gpu_allocated[idx] && !held_gpus.contains(&id)) || gpu_indices.contains(&idx) {
                return Err(AllocError::GpusUnavailable);
            }
            gpu_indices.push(idx);
        }
        self.drop_owner(ReleaseWarrant::superseded_by_newer_attempt(
            RunKey::any_attempt(job_id),
        ));

        for &cpu in cpu_ids {
            self.allocated_cpus[cpu as usize] = true;
        }
        self.allocated_memory_mb += memory_mb;
        for &idx in &gpu_indices {
            self.gpu_allocated[idx] = true;
        }

        let result = AllocationResult {
            cpu_ids: cpu_ids.to_vec(),
            gpu_ids: gpu_device_ids.to_vec(),
            memory_mb,
        };
        // Committed outright: an adopted job is already running, and leaving it
        // `launching` would let reconcile reclaim it once the TTL elapsed.
        self.owners.insert(
            job_id,
            Owned {
                run_attempt,
                result: result.clone(),
            },
        );
        Ok(result)
    }

    /// Mark a job's allocation as committed (its process is now tracked), so it
    /// is no longer exempt from reconcile. Returns false if the reservation no
    /// longer exists (reconcile reclaimed it after the launch exceeded the TTL)
    /// or now belongs to a different `run_attempt` (a newer reservation for the
    /// same job id superseded this one) — either way the caller must not treat
    /// the job as backed by an allocation.
    pub fn commit_job(&mut self, job_id: u32, run_attempt: u32) -> bool {
        let owned = self
            .owners
            .get(&job_id)
            .is_some_and(|owned| owned.run_attempt == run_attempt);
        // The marker is keyed by job id alone, so a late commit for a superseded
        // attempt would clear the one protecting the attempt that replaced it.
        if owned {
            self.launching.remove(&job_id);
        }
        owned
    }

    /// Drop a job's ownership entry and free what it held. Takes the warrant so
    /// no path out of the allocator can be written without naming its ground.
    fn drop_owner(&mut self, warrant: ReleaseWarrant) -> bool {
        let job_id = warrant.run().job_id();
        self.launching.remove(&job_id);
        let Some(owned) = self.owners.remove(&job_id) else {
            return false;
        };
        self.release(&owned.result);
        true
    }

    /// The only way out of the allocator, so a release cannot be written without
    /// a warrant. Idempotent; an exact key a newer attempt owns frees nothing.
    pub fn release_job(&mut self, warrant: ReleaseWarrant) -> bool {
        let run = warrant.run();
        let job_id = run.job_id();
        let owned_by_this_run = self
            .owners
            .get(&job_id)
            .is_some_and(|owned| run.names_attempt(owned.run_attempt));
        // A newer attempt already superseded the one this warrant names; freeing
        // it here would hand away the reservation that replaced it.
        if !owned_by_this_run && run.attempt().is_some() {
            return false;
        }
        self.drop_owner(warrant)
    }

    /// Release owned allocations whose job is neither live nor launching within
    /// `launching_ttl`, returning the reclaimed ids. Recovers a failed teardown
    /// or a dropped launch instead of stranding the node until spurd restart.
    /// Claims with nothing tracked behind them. A query, not a reclaim: a
    /// missing entry is not evidence that the job's work finished.
    pub fn unbacked_claims(
        &self,
        live: &HashSet<u32>,
        now: Instant,
        launching_ttl: Duration,
    ) -> Vec<(u32, u32)> {
        let mut unbacked: Vec<(u32, u32)> = self
            .owners
            .iter()
            .filter(|(id, _)| {
                if live.contains(id) {
                    return false;
                }
                match self.launching.get(id) {
                    Some(reserved_at) => {
                        now.saturating_duration_since(*reserved_at) >= launching_ttl
                    }
                    None => true,
                }
            })
            .map(|(id, owned)| (*id, owned.run_attempt))
            .collect();
        unbacked.sort_unstable();
        unbacked
    }

    /// Every run this node charges, and the job ids no exact `RunKey` names.
    /// Those widen into the set to cover their whole job, never dropped or fatal.
    pub fn charged_runs(&self) -> (HashSet<RunKey>, Vec<u32>) {
        let mut charged = HashSet::with_capacity(self.owners.len());
        let mut unnameable = Vec::new();
        for (job_id, owned) in &self.owners {
            match RunKey::new(*job_id, owned.run_attempt) {
                Some(run) => {
                    charged.insert(run);
                }
                None => {
                    unnameable.push(*job_id);
                    charged.insert(RunKey::any_attempt(*job_id));
                }
            }
        }
        unnameable.sort_unstable();
        (charged, unnameable)
    }

    /// The attempt this node charges for one job, `None` if it charges nothing or
    /// names no attempt. Reads this job's owner alone, so no other answers for it.
    pub fn charged_attempt(&self, job_id: u32) -> Option<u32> {
        let owned = self.owners.get(&job_id)?;
        RunKey::new(job_id, owned.run_attempt)?.attempt()
    }
}

/// An owned reservation, tagged with the attempt that created it so a later
/// operation for a superseded attempt can be told apart from the current one.
#[derive(Debug, Clone)]
struct Owned {
    run_attempt: u32,
    result: AllocationResult,
}

/// Result of a successful allocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllocationResult {
    /// Allocated core IDs.
    pub cpu_ids: Vec<u32>,
    /// Allocated GPU device IDs.
    pub gpu_ids: Vec<u32>,
    /// Allocated memory in MB.
    pub memory_mb: u64,
}

impl AllocationResult {
    /// Format CPU IDs as a taskset-compatible string.
    pub fn cpu_list(&self) -> String {
        self.cpu_ids
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Format GPU IDs as ROCR_VISIBLE_DEVICES/CUDA_VISIBLE_DEVICES string.
    pub fn gpu_list(&self) -> String {
        self.gpu_ids
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(",")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(job_id: u32, run_attempt: u32) -> RunKey {
        RunKey::new(job_id, run_attempt).expect("attempts start at 1")
    }
    use spur_core::resource::GpuLinkType;

    fn make_node(cpus: u32, mem: u64, num_gpus: usize, gpu_type: &str) -> NodeAllocation {
        make_node_with_ids(cpus, mem, (0..num_gpus as u32).collect(), gpu_type)
    }

    fn make_node_with_ids(
        cpus: u32,
        mem: u64,
        device_ids: Vec<u32>,
        gpu_type: &str,
    ) -> NodeAllocation {
        let gpus: Vec<GpuResource> = device_ids
            .into_iter()
            .map(|device_id| GpuResource {
                device_id,
                gpu_type: gpu_type.into(),
                memory_mb: 192_000,
                peer_gpus: vec![],
                link_type: GpuLinkType::XGMI,
            })
            .collect();

        let resources = ResourceSet {
            cpus,
            memory_mb: mem,
            gpus,
            ..Default::default()
        };

        NodeAllocation::new("node001".into(), &resources)
    }

    #[test]
    fn test_initial_state() {
        let node = make_node(64, 256_000, 8, "mi300x");
        assert_eq!(node.free_cpus(), 64);
        assert_eq!(node.free_memory_mb(), 256_000);
        assert_eq!(node.free_gpus(None), 8);
    }

    #[test]
    fn test_allocate_cpus() {
        let mut node = make_node(64, 256_000, 0, "");
        let alloc = node.allocate_for_job(1, 1, 16, 0, &[]).unwrap();
        assert_eq!(alloc.cpu_ids.len(), 16);
        assert_eq!(node.free_cpus(), 48);
    }

    #[test]
    fn test_allocate_gpus_by_device_id() {
        let mut node = make_node(64, 256_000, 8, "mi300x");
        let alloc = node.allocate_for_job(1, 1, 0, 0, &[0, 1, 2, 3]).unwrap();
        assert_eq!(alloc.gpu_ids, vec![0, 1, 2, 3]);
        assert_eq!(node.free_gpus(None), 4);
    }

    #[test]
    fn test_record_then_release_noncontiguous_device_ids() {
        // Real nodes expose GPUs whose device_id != vec position (DRM render
        // ids 128..135). Release keys by device_id, so a released device must
        // return to the free pool or it is rejected forever.
        let mut node = make_node_with_ids(64, 256_000, vec![128, 129, 130, 131], "mi350x");

        assert!(node.allocate_for_job(1, 1, 0, 0, &[129, 131]).is_ok());
        assert_eq!(node.free_gpus(None), 2);

        assert!(node.release_job(ReleaseWarrant::controller_cancelled(RunKey::any_attempt(1))));
        assert_eq!(
            node.free_gpus(None),
            4,
            "released GPUs must return to the free pool"
        );

        // The whole point: the device is re-allocatable after release.
        assert!(
            node.allocate_for_job(2, 1, 0, 0, &[129, 131]).is_ok(),
            "device_ids must be re-allocatable after release"
        );
    }

    #[test]
    fn test_allocate_rejects_unknown_device_id() {
        let mut node = make_node_with_ids(64, 256_000, vec![128, 129], "mi350x");
        assert!(node.allocate_for_job(1, 1, 0, 0, &[200]).is_err());
        // A rejected allocation must not leave partial state behind.
        assert_eq!(node.free_gpus(None), 2);
        assert!(!node.release_job(ReleaseWarrant::controller_cancelled(RunKey::any_attempt(1))));
    }

    #[test]
    fn test_allocate_rolls_back_on_partial_gpu_conflict() {
        // If a multi-GPU allocation hits a conflict partway, it must not leave
        // the earlier device_ids marked allocated — that is itself a leak.
        let mut node = make_node_with_ids(64, 256_000, vec![128, 129, 130], "mi350x");
        assert!(node.allocate_for_job(1, 1, 0, 0, &[129]).is_ok());
        // [128, 129] — 129 already taken, so the whole call must fail and 128
        // must remain free.
        assert!(node.allocate_for_job(2, 1, 0, 0, &[128, 129]).is_err());
        assert!(
            node.allocate_for_job(3, 1, 0, 0, &[128]).is_ok(),
            "128 must remain free after the failed partial allocation"
        );
    }

    #[test]
    fn test_allocate_for_job_release_by_id() {
        let mut node = make_node_with_ids(64, 256_000, vec![128, 129, 130, 131], "mi350x");
        let alloc = node
            .allocate_for_job(1, 1, 8, 32_000, &[129, 131])
            .expect("allocation should succeed");
        assert_eq!(alloc.gpu_ids, vec![129, 131]);
        assert_eq!(node.free_cpus(), 56);
        assert_eq!(node.free_gpus(None), 2);
        assert_eq!(node.free_memory_mb(), 224_000);

        assert!(node.release_job(ReleaseWarrant::controller_cancelled(RunKey::any_attempt(1))));
        assert_eq!(node.free_cpus(), 64);
        assert_eq!(node.free_gpus(None), 4);
        assert_eq!(node.free_memory_mb(), 256_000);
        // Idempotent: releasing again is a no-op.
        assert!(!node.release_job(ReleaseWarrant::controller_cancelled(RunKey::any_attempt(1))));
    }

    #[test]
    fn test_allocate_for_job_rejects_in_flight_duplicate() {
        // A second launch while the first is still in flight (reserved, not yet
        // committed or released) is a genuine concurrent duplicate: rejecting it
        // avoids double-counting CPU/mem and orphaning the prior owner entry.
        let mut node = make_node(64, 256_000, 0, "");
        assert!(node.allocate_for_job(1, 1, 8, 16_000, &[]).is_ok());
        assert_eq!(
            node.allocate_for_job(1, 1, 8, 16_000, &[]),
            Err(AllocError::DuplicateJob)
        );
        assert_eq!(node.free_cpus(), 56);
        assert_eq!(node.free_memory_mb(), 240_000);
        // After release the id is free to reserve again.
        assert!(node.release_job(ReleaseWarrant::controller_cancelled(RunKey::any_attempt(1))));
        assert!(node.allocate_for_job(1, 1, 8, 16_000, &[]).is_ok());
    }

    #[test]
    fn test_allocate_for_job_supersedes_stale_committed_reservation() {
        // A committed reservation whose teardown has not released yet (e.g. a
        // preempted-then-requeued job re-dispatched under the same id before the
        // agent reaped the killed process) must be superseded, not rejected —
        // otherwise the legitimate re-launch fails and the job never runs. The
        // stale reservation is released first, so resources are not double-counted.
        let mut node = make_node(64, 256_000, 0, "");
        node.allocate_for_job(7, 1, 8, 16_000, &[]).unwrap();
        node.commit_job(7, 1); // prior run committed, then preempted (not released).
        assert_eq!(node.free_cpus(), 56);

        // Re-dispatch under the same id succeeds and does not double-count.
        let alloc = node
            .allocate_for_job(7, 1, 8, 16_000, &[])
            .expect("re-dispatch of a stale committed job id must succeed");
        assert_eq!(alloc.cpu_ids.len(), 8);
        assert_eq!(node.free_cpus(), 56);
        assert_eq!(node.free_memory_mb(), 240_000);
        // Exactly one owner entry remains, and the fresh reservation is again
        // treated as in-flight until it commits or releases.
        assert_eq!(
            node.allocate_for_job(7, 1, 8, 16_000, &[]),
            Err(AllocError::DuplicateJob)
        );
    }

    #[test]
    fn test_allocate_for_job_rejects_conflicting_gpu() {
        let mut node = make_node_with_ids(64, 256_000, vec![0, 1], "mi300x");
        assert!(node.allocate_for_job(1, 1, 4, 0, &[0]).is_ok());
        // Second job wanting the same device id must fail with no state change.
        assert_eq!(
            node.allocate_for_job(2, 1, 4, 0, &[0]),
            Err(AllocError::GpusUnavailable)
        );
        assert_eq!(node.free_gpus(None), 1);
        // The failed job left no owner entry.
        assert!(!node.release_job(ReleaseWarrant::controller_cancelled(RunKey::any_attempt(2))));
    }

    #[test]
    fn test_conflicting_owners_reports_committed_but_not_launching() {
        let mut node = make_node_with_ids(64, 256_000, vec![0, 1, 2, 3], "mi300x");
        // job 1: committed, owns GPUs 0 and 1.
        node.allocate_for_job(1, 1, 4, 8_000, &[0, 1]).unwrap();
        node.commit_job(1, 1);
        // job 2: still launching (reserved, not committed), owns GPU 2.
        node.allocate_for_job(2, 1, 4, 8_000, &[2]).unwrap();

        // A new dispatch onto GPU 1 conflicts with committed job 1.
        assert_eq!(node.conflicting_owners(&[1]), vec![1]);
        // A dispatch onto GPU 3 (free) conflicts with nobody.
        assert!(node.conflicting_owners(&[3]).is_empty());
        // GPU 2's owner is still launching — a real duplicate, never reported.
        assert!(node.conflicting_owners(&[2]).is_empty());
        // A dispatch spanning a committed and a launching device reports only the
        // committed owner.
        assert_eq!(node.conflicting_owners(&[1, 2]), vec![1]);
    }

    #[test]
    fn test_reconcile_reclaims_orphans_but_spares_live_and_launching() {
        let mut node = make_node_with_ids(64, 256_000, vec![0, 1, 2, 3], "mi300x");
        // job 1: committed and live.
        node.allocate_for_job(1, 1, 4, 8_000, &[0]).unwrap();
        node.commit_job(1, 1);
        // job 2: committed but NOT live (teardown failed to release — orphan).
        node.allocate_for_job(2, 1, 4, 8_000, &[1]).unwrap();
        node.commit_job(2, 1);
        // job 3: launching within TTL — spared.
        node.allocate_for_job(3, 1, 4, 8_000, &[2]).unwrap();

        let live: HashSet<u32> = [1].into_iter().collect();
        let ttl = Duration::from_secs(120);
        assert_eq!(
            node.unbacked_claims(&live, Instant::now(), ttl),
            vec![(2, 1)],
            "only the untracked job is reported"
        );
        // Reporting is not reclaiming: the claim is still held afterwards.
        assert_eq!(node.free_gpus(None), 1);

        assert!(node.release_job(ReleaseWarrant::controller_cancelled(RunKey::any_attempt(2))));
        assert!(node.release_job(ReleaseWarrant::controller_cancelled(RunKey::any_attempt(1))));
        assert!(node.release_job(ReleaseWarrant::controller_cancelled(RunKey::any_attempt(3))));
        assert_eq!(node.free_gpus(None), 4);
    }

    #[test]
    fn a_runs_own_steps_never_conflict_with_each_other() {
        // The claim points at the run, not at one step, so a run's steps share
        // its devices. A conflict here would cancel an entitled job.
        let mut node = make_node_with_ids(8, 64_000, vec![0, 1], "mi300x");
        node.allocate_for_job(7, 1, 2, 1_000, &[0]).unwrap();
        node.commit_job(7, 1);

        assert_eq!(
            node.conflicting_owners(&[0]),
            vec![7],
            "the run itself holds it"
        );
        // Re-allocating for the same run succeeds rather than conflicting.
        assert!(node.allocate_for_job(7, 1, 2, 1_000, &[0]).is_ok());
        // A different job is a genuine conflict.
        node.commit_job(7, 1);
        assert_eq!(
            node.allocate_for_job(8, 1, 2, 1_000, &[0]),
            Err(AllocError::GpusUnavailable)
        );
    }

    #[test]
    fn allocate_refuses_a_cpu_shortfall_instead_of_under_serving() {
        // It used to fill what it could and return Ok with a short list, which
        // both callers read as a full allocation.
        let mut node = make_node(4, 64_000, 0, "");
        node.allocate_for_job(1, 1, 3, 1_000, &[]).unwrap();

        assert_eq!(
            node.allocate_for_job(2, 1, 3, 1_000, &[]),
            Err(AllocError::CpusUnavailable)
        );
        assert_eq!(node.free_cpus(), 1, "the refusal must not consume cores");
        assert!(node.allocate_for_job(2, 1, 1, 1_000, &[]).is_ok());
    }

    #[test]
    fn allocate_refuses_to_take_the_node_past_its_memory() {
        let mut node = make_node(8, 10_000, 0, "");
        node.allocate_for_job(1, 1, 1, 6_000, &[]).unwrap();

        assert_eq!(
            node.allocate_for_job(2, 1, 1, 5_000, &[]),
            Err(AllocError::MemoryUnavailable)
        );
        assert_eq!(
            node.allocated_memory_mb, 6_000,
            "the refusal must not commit"
        );
        assert!(node.allocate_for_job(2, 1, 1, 4_000, &[]).is_ok());
        assert_eq!(node.allocated_memory_mb, 10_000);
    }

    #[test]
    fn a_node_that_never_read_its_memory_is_not_treated_as_having_none() {
        // A node reports 0 when /proc/meminfo is unreadable. Enforcing a ceiling
        // against that refuses every job the node is asked to run.
        let mut node = make_node(8, 0, 0, "");
        assert!(node.allocate_for_job(1, 1, 1, 4_000, &[]).is_ok());
    }

    #[test]
    fn a_refused_reallocation_does_not_drop_the_owner_it_would_supersede() {
        let mut node = make_node(4, 10_000, 0, "");
        node.allocate_for_job(1, 1, 2, 4_000, &[]).unwrap();
        node.commit_job(1, 1);
        node.allocate_for_job(2, 1, 2, 4_000, &[]).unwrap();
        node.commit_job(2, 1);

        // Job 1 re-allocating beyond what it could free must refuse without
        // having already released what it holds.
        assert_eq!(
            node.allocate_for_job(1, 2, 4, 4_000, &[]),
            Err(AllocError::CpusUnavailable)
        );
        assert_eq!(
            node.free_cpus(),
            0,
            "the refusal must not have released job 1"
        );
        assert_eq!(node.allocated_memory_mb, 8_000);

        // It may still grow into exactly what it can free.
        assert!(node.allocate_for_job(1, 2, 2, 6_000, &[]).is_ok());
    }

    #[test]
    fn test_unbacked_claims_reports_launching_past_ttl_without_freeing_it() {
        let mut node = make_node_with_ids(64, 256_000, vec![0, 1, 2, 3], "mi300x");
        // A launch reserved but never committed must be reclaimed past the TTL.
        node.allocate_for_job(1, 1, 4, 8_000, &[0]).unwrap();

        let live: HashSet<u32> = HashSet::new();
        let ttl = Duration::from_secs(120);

        assert!(node.unbacked_claims(&live, Instant::now(), ttl).is_empty());
        assert_eq!(node.free_gpus(None), 3, "still reserved within TTL");

        let future = Instant::now() + Duration::from_secs(121);
        assert_eq!(node.unbacked_claims(&live, future, ttl), vec![(1, 1)]);
        assert_eq!(
            node.free_gpus(None),
            3,
            "a timeout alone must not free a claim the agent cannot account for"
        );
    }

    #[test]
    fn test_commit_job_reports_reclaimed_reservation() {
        let mut node = make_node_with_ids(64, 256_000, vec![0, 1], "mi300x");
        node.allocate_for_job(1, 1, 4, 8_000, &[0]).unwrap();
        assert!(
            node.commit_job(1, 1),
            "commit of a live reservation returns true"
        );

        // A reservation explicitly released before commit: the owner is gone,
        // so commit must report false and stay a no-op.
        node.allocate_for_job(2, 1, 4, 8_000, &[1]).unwrap();
        node.release_job(ReleaseWarrant::controller_cancelled(RunKey::any_attempt(2)));
        assert!(
            !node.commit_job(2, 1),
            "commit of a released reservation returns false"
        );
        assert_eq!(
            node.free_gpus(None),
            1,
            "the released GPU stays free after the no-op commit"
        );
    }

    #[test]
    fn test_commit_job_rejects_a_superseded_attempt() {
        // Attempt 1 reserves job 7, then something (a cancel, or the TTL
        // reconcile) releases it believing it's gone before it ever commits —
        // freeing the job id for a redispatch (attempt 2) to reserve and
        // commit. Attempt 1, unaware it was superseded, must not have its own,
        // now-stale commit adopt attempt 2's reservation as its own.
        let mut node = make_node(64, 256_000, 0, "");
        node.allocate_for_job(7, 1, 8, 16_000, &[]).unwrap();
        node.release_job(ReleaseWarrant::controller_cancelled(RunKey::any_attempt(7)));
        node.allocate_for_job(7, 2, 8, 16_000, &[]).unwrap();
        assert!(node.commit_job(7, 2), "the current attempt commits");

        assert!(
            !node.commit_job(7, 1),
            "a superseded attempt's late commit must not succeed"
        );
        assert_eq!(
            node.free_cpus(),
            56,
            "the current attempt's resources must remain allocated"
        );
    }

    #[test]
    fn test_allocate_for_job_rejects_a_stale_attempt_superseding_a_committed_newer_one() {
        // Attempt 2 reserves and commits job 7. A late/duplicate LaunchJob for
        // the older attempt 1 must not be allowed to supersede it — that would
        // hand attempt 2's live, running resources to a stale relaunch.
        let mut node = make_node(64, 256_000, 0, "");
        node.allocate_for_job(7, 2, 8, 16_000, &[]).unwrap();
        assert!(node.commit_job(7, 2));

        assert_eq!(
            node.allocate_for_job(7, 1, 8, 16_000, &[]),
            Err(AllocError::Superseded)
        );
        assert_eq!(
            node.free_cpus(),
            56,
            "the newer attempt's resources must remain allocated"
        );
    }

    #[test]
    fn test_a_superseded_commit_leaves_the_replacement_launch_protected() {
        // Attempt 1's reservation is reclaimed, attempt 2 takes the job id, and
        // attempt 1's late commit lands while attempt 2 is still mid-launch.
        let mut node = make_node(64, 256_000, 0, "");
        node.allocate_for_job(7, 1, 8, 16_000, &[]).unwrap();
        node.release_job(ReleaseWarrant::controller_cancelled(RunKey::any_attempt(7)));
        node.allocate_for_job(7, 2, 8, 16_000, &[]).unwrap();

        assert!(!node.commit_job(7, 1), "a superseded attempt cannot commit");

        assert!(
            node.unbacked_claims(&HashSet::new(), Instant::now(), Duration::from_secs(120))
                .is_empty(),
            "a stale commit must not strip the launching marker sparing attempt 2"
        );
        assert_eq!(
            node.free_cpus(),
            56,
            "attempt 2's mid-launch reservation must survive"
        );
    }

    #[test]
    fn test_allocate_for_job_supersedes_a_committed_same_or_older_attempt() {
        // A same-attempt retry, or a genuinely newer attempt reserving after a
        // stale committed owner, must still supersede as before.
        let mut node = make_node(64, 256_000, 0, "");
        node.allocate_for_job(7, 1, 8, 16_000, &[]).unwrap();
        assert!(node.commit_job(7, 1));

        assert!(node.allocate_for_job(7, 2, 8, 16_000, &[]).is_ok());
        assert_eq!(node.free_cpus(), 56);
    }

    #[test]
    fn test_restore_for_job_rejects_a_core_another_job_holds() {
        let mut node = make_node(8, 64_000, 0, "");
        node.allocate_for_job(1, 1, 4, 8_000, &[]).unwrap();
        assert!(node.commit_job(1, 1));
        assert_eq!(node.free_cpus(), 4);

        // Replaying a descriptor that names core 3 — job 1's — would hand both
        // an overlapping cpuset the moment either one released.
        assert_eq!(
            node.restore_for_job(2, 1, &[3, 4], 8_000, &[]),
            Err(AllocError::CpusUnavailable)
        );
        assert_eq!(node.free_cpus(), 4, "rejected replay changed the ledger");
        assert_eq!(node.free_memory_mb(), 56_000);
        assert!(
            !node.release_job(ReleaseWarrant::controller_cancelled(RunKey::any_attempt(2))),
            "rejected replay left an owner entry"
        );

        // Job 1's cores are still exclusively its own to release.
        assert!(node.release_job(ReleaseWarrant::controller_cancelled(RunKey::any_attempt(1))));
        assert_eq!(node.free_cpus(), 8);
    }

    #[test]
    fn test_restore_for_job_replays_a_jobs_own_cores_again() {
        let mut node = make_node_with_ids(8, 64_000, vec![0, 1], "mi300x");
        node.restore_for_job(5, 2, &[1, 2], 8_000, &[0]).unwrap();
        assert_eq!(node.free_cpus(), 6);

        // A second adoption pass replays the same descriptor; the job's own
        // cores and GPUs must not read as someone else's conflict.
        node.restore_for_job(5, 2, &[1, 2], 8_000, &[0]).unwrap();
        assert_eq!(node.free_cpus(), 6);
        assert_eq!(node.free_memory_mb(), 56_000);
        assert_eq!(node.free_gpus(None), 1);
    }

    #[test]
    fn test_restore_for_job_rejects_a_superseded_attempt() {
        let mut node = make_node(8, 64_000, 0, "");
        node.allocate_for_job(9, 3, 2, 8_000, &[]).unwrap();
        assert!(node.commit_job(9, 3));

        // A newer attempt already owns this id; a stale adoption must not
        // clobber it or release the cores it is running on.
        assert_eq!(
            node.restore_for_job(9, 2, &[4, 5], 8_000, &[]),
            Err(AllocError::Superseded)
        );
        assert_eq!(node.free_cpus(), 6);
        assert!(node.release_job(ReleaseWarrant::controller_cancelled(key(9, 3))));
        assert_eq!(node.free_cpus(), 8);
    }

    #[test]
    fn test_restore_for_job_commits_outright_instead_of_leaving_it_launching() {
        let mut node = make_node_with_ids(8, 64_000, vec![0, 1], "mi300x");
        node.restore_for_job(4, 1, &[0, 1], 8_000, &[0]).unwrap();

        // conflicting_owners skips mid-launch owners, so reporting one proves
        // the adopted job committed rather than staying reclaimable.
        assert_eq!(node.conflicting_owners(&[0]), vec![4]);
        let live: HashSet<u32> = HashSet::new();
        let ttl = Duration::from_secs(600);
        assert_eq!(
            node.unbacked_claims(&live, Instant::now(), ttl),
            vec![(4, 1)]
        );
        node.release_job(ReleaseWarrant::controller_cancelled(RunKey::any_attempt(4)));
        assert_eq!(node.free_cpus(), 8);

        // A job still mid-launch when the agent restarted: adoption must clear
        // the launching marker it left behind.
        node.allocate_for_job(7, 1, 2, 8_000, &[1]).unwrap();
        assert!(node.conflicting_owners(&[1]).is_empty());
        node.restore_for_job(7, 1, &[2, 3], 8_000, &[1]).unwrap();
        assert_eq!(node.conflicting_owners(&[1]), vec![7]);
    }

    #[test]
    fn test_restore_for_job_keeps_an_incumbent_owner_when_it_rejects() {
        let mut node = make_node_with_ids(8, 64_000, vec![0, 1], "mi300x");
        node.restore_for_job(5, 1, &[1, 2], 8_000, &[0]).unwrap();
        node.restore_for_job(9, 1, &[3], 8_000, &[1]).unwrap();

        // Job 5 replays a descriptor that has grown job 9's core. Rejecting
        // after the release would drop job 5's own entry along with it.
        assert_eq!(
            node.restore_for_job(5, 1, &[1, 2, 3], 8_000, &[0]),
            Err(AllocError::CpusUnavailable)
        );
        assert_eq!(node.free_cpus(), 5);
        assert_eq!(node.free_memory_mb(), 48_000);
        assert_eq!(node.free_gpus(None), 0);
        assert!(
            node.release_job(ReleaseWarrant::controller_cancelled(key(5, 1))),
            "job 5 lost its owner entry"
        );
        assert_eq!(node.free_cpus(), 7);
    }

    #[test]
    fn test_restore_for_job_rejects_a_gpu_another_job_holds() {
        let mut node = make_node_with_ids(8, 64_000, vec![0, 1], "mi300x");
        node.allocate_for_job(1, 1, 2, 8_000, &[0]).unwrap();
        assert!(node.commit_job(1, 1));

        assert_eq!(
            node.restore_for_job(2, 1, &[4, 5], 8_000, &[0]),
            Err(AllocError::GpusUnavailable)
        );
        assert_eq!(node.free_gpus(None), 1);
        assert_eq!(node.free_cpus(), 6, "rejected replay claimed cores anyway");
        assert!(!node.release_job(ReleaseWarrant::controller_cancelled(RunKey::any_attempt(2))));
    }

    #[test]
    fn test_restore_for_job_records_memory_and_gpus_with_no_cores() {
        let mut node = make_node_with_ids(4, 64_000, vec![0], "mi300x");

        // Narrowing can strip every recorded core. The GPU and memory still
        // have to land, or a live job's device reads free.
        node.restore_for_job(6, 1, &[], 8_000, &[0]).unwrap();
        assert_eq!(node.free_gpus(None), 0);
        assert_eq!(node.free_memory_mb(), 56_000);
        assert_eq!(node.conflicting_owners(&[0]), vec![6]);
    }

    #[test]
    fn test_claimable_cpu_ids_drops_only_the_contested_cores() {
        let mut node = make_node(8, 64_000, 0, "");
        node.allocate_for_job(1, 1, 2, 8_000, &[]).unwrap();

        // Another job's cores and cores the node lost are dropped; the job's
        // own survive so a repeated replay does not shrink it.
        assert_eq!(node.claimable_cpu_ids(2, &[0, 1, 4, 9]), vec![4]);
        node.restore_for_job(2, 1, &[4, 5], 8_000, &[]).unwrap();
        assert_eq!(node.claimable_cpu_ids(2, &[4, 5, 0]), vec![4, 5]);
        assert!(node.claimable_cpu_ids(3, &[0, 1]).is_empty());
    }

    #[test]
    fn test_restore_for_job_rejects_a_core_the_node_no_longer_has() {
        let mut node = make_node(4, 64_000, 0, "");
        assert_eq!(
            node.restore_for_job(1, 1, &[3, 4], 8_000, &[]),
            Err(AllocError::CpusUnavailable)
        );
        assert_eq!(node.free_cpus(), 4);
        assert!(!node.release_job(ReleaseWarrant::controller_cancelled(RunKey::any_attempt(1))));
    }

    #[test]
    fn test_release_job_if_spares_a_reused_job_ids_reservation() {
        let mut node = make_node(64, 256_000, 0, "");
        node.allocate_for_job(7, 1, 8, 16_000, &[]).unwrap();
        node.release_job(ReleaseWarrant::controller_cancelled(RunKey::any_attempt(7)));
        node.allocate_for_job(7, 2, 8, 16_000, &[]).unwrap();

        assert!(
            !node.release_job(ReleaseWarrant::controller_cancelled(key(7, 1))),
            "a stale attempt must not release a different, current attempt's reservation"
        );
        assert_eq!(node.free_cpus(), 56);
        assert!(
            node.release_job(ReleaseWarrant::controller_cancelled(key(7, 2))),
            "the current attempt can release its own"
        );
        assert_eq!(node.free_cpus(), 64);
    }

    #[test]
    fn test_memory_released_symmetrically_with_zero_cpus() {
        // A job with 0 cpus must still have its memory reserved and released
        // symmetrically, or release drives allocated_memory_mb below what was
        // added.
        let mut node = make_node(64, 256_000, 0, "");
        node.allocate_for_job(1, 1, 0, 16_000, &[]).unwrap();
        assert_eq!(node.free_memory_mb(), 240_000);
        node.release_job(ReleaseWarrant::controller_cancelled(RunKey::any_attempt(1)));
        assert_eq!(node.free_memory_mb(), 256_000);
    }

    #[test]
    fn test_multiple_allocations() {
        let mut node = make_node(64, 256_000, 8, "mi300x");
        let a1 = node.allocate_for_job(1, 1, 16, 64_000, &[0, 1]).unwrap();
        let a2 = node.allocate_for_job(2, 1, 16, 64_000, &[2, 3]).unwrap();
        assert_eq!(node.free_cpus(), 32);
        assert_eq!(node.free_gpus(None), 4);

        // CPU IDs should not overlap
        let overlap: Vec<_> = a1
            .cpu_ids
            .iter()
            .filter(|id| a2.cpu_ids.contains(id))
            .collect();
        assert!(overlap.is_empty());
    }

    #[test]
    fn test_allocation_result_format() {
        let alloc = AllocationResult {
            cpu_ids: vec![0, 1, 2, 3],
            gpu_ids: vec![0, 1],
            memory_mb: 128_000,
        };
        assert_eq!(alloc.cpu_list(), "0,1,2,3");
        assert_eq!(alloc.gpu_list(), "0,1");
    }

    // The superseded owner may still be running. A refusal that has already
    // freed it hands its cores and GPUs to the next job to ask.
    #[test]
    fn a_refused_relaunch_leaves_the_attempt_it_would_have_superseded_holding() {
        let mut node = make_node(16, 64_000, 4, "mi300x");
        node.allocate_for_job(7, 1, 8, 32_000, &[0, 1])
            .expect("the first attempt reserves");
        node.commit_job(7, 1);
        // Held by another job, so the relaunch below cannot have it.
        node.allocate_for_job(9, 1, 4, 8_000, &[2])
            .expect("a neighbour takes a device");
        node.commit_job(9, 1);

        assert_eq!(
            node.allocate_for_job(7, 2, 8, 32_000, &[0, 2]),
            Err(AllocError::GpusUnavailable)
        );

        assert_eq!(
            node.charged_runs(),
            (HashSet::from([key(7, 1), key(9, 1)]), vec![]),
            "a refused attempt must not evict the one it was superseding"
        );
        assert_eq!(node.free_cpus(), 4);
        assert_eq!(node.free_memory_mb(), 24_000);
        let mut still_allocated = node.allocated_gpu_ids();
        still_allocated.sort_unstable();
        assert_eq!(still_allocated, vec![0, 1, 2]);
    }

    // The set gates whether a record may be deleted, so an owner it cannot
    // represent must widen to its whole job rather than read as uncharged.
    #[test]
    fn an_owner_that_cannot_be_named_as_a_run_is_widened_not_dropped() {
        let mut node = make_node(8, 16_000, 0, "mi300x");
        node.restore_for_job(7, 0, &[0, 1], 1_000, &[])
            .expect("a descriptor naming no attempt still charges the node");

        assert_eq!(
            node.charged_runs(),
            (HashSet::from([RunKey::any_attempt(7)]), vec![7]),
            "a charge the set cannot name must cover every attempt of its job"
        );
    }

    // One unnameable owner used to answer for the whole node, so a healthy
    // neighbour's charge has to survive it being there.
    #[test]
    fn an_unnameable_owner_does_not_answer_for_another_job() {
        let mut node = make_node(8, 16_000, 0, "mi300x");
        node.restore_for_job(7, 0, &[0, 1], 1_000, &[])
            .expect("a descriptor naming no attempt still charges the node");
        node.allocate_for_job(9, 3, 2, 1_000, &[])
            .expect("a neighbour reserves alongside it");

        assert_eq!(node.charged_attempt(9), Some(3));
        assert_eq!(node.charged_attempt(7), None);
        assert_eq!(node.charged_attempt(11), None);
        let (charged, unnameable) = node.charged_runs();
        assert!(charged.contains(&key(9, 3)));
        assert_eq!(unnameable, vec![7]);
    }

    // The superseding attempt still has to be able to take over what the
    // outgoing one held, or every relaunch refuses itself.
    #[test]
    fn a_newer_attempt_reclaims_the_slice_of_the_one_it_supersedes() {
        let mut node = make_node(16, 64_000, 4, "mi300x");
        node.allocate_for_job(7, 1, 16, 64_000, &[0, 1, 2, 3])
            .expect("the first attempt takes the whole node");
        node.commit_job(7, 1);

        let second = node
            .allocate_for_job(7, 2, 16, 64_000, &[0, 1, 2, 3])
            .expect("the newer attempt reclaims what the older one held");

        assert_eq!(second.gpu_ids, vec![0, 1, 2, 3]);
        assert_eq!(node.charged_runs(), (HashSet::from([key(7, 2)]), vec![]));
        assert_eq!(node.free_cpus(), 0);
        assert_eq!(node.free_memory_mb(), 0);
    }
}
