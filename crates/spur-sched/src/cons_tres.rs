// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Consumable TRES resource selection.
//!
//! Tracks resources at core/socket/GPU granularity within a node.
//! This is the equivalent of Slurm's select/cons_tres plugin.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use spur_core::resource::{GpuResource, ResourceSet};

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
}

/// Outcome of applying a fresh inventory to a node's live allocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapacityChange {
    /// Applied cleanly: cpu/mem/gpu capacity rebuilt from `fresh`; any live
    /// allocations remain valid (their stable_ids still present).
    Applied,
    /// One or more stable_ids that a job currently holds are gone in `fresh`.
    /// Capacity is NOT modified. Carries the affected (job_id, lost_stable_ids).
    AllocatedDevicesLost(Vec<(u32, Vec<u64>)>),
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

    /// Apply a freshly-discovered inventory to this node's live allocation.
    /// Rebuilds capacity when every live-held GPU is still present; otherwise
    /// reports the lost devices and leaves capacity untouched so the caller
    /// can drive policy (drain/fail) before anything is mutated.
    pub fn update_capacity(&mut self, fresh: &ResourceSet) -> CapacityChange {
        let fresh_ids: HashSet<u64> = fresh.gpus.iter().map(|g| g.stable_id).collect();

        // Which live-owned stable ids are gone in the fresh set?
        let mut lost: Vec<(u32, Vec<u64>)> = self
            .owners
            .iter()
            .filter_map(|(job, owned)| {
                let alloc = &owned.result;
                let gone: Vec<u64> = alloc
                    .gpu_ids
                    .iter()
                    .copied()
                    .filter(|id| !fresh_ids.contains(id))
                    .collect();
                (!gone.is_empty()).then_some((*job, gone))
            })
            .collect();
        // A held core above the new cpu count would be truncated by the resize,
        // freeing a bit its owner still holds. Defer like the GPU case; entries
        // carry an empty device list since the loss is a CPU id, not a stable_id.
        for (job, owned) in &self.owners {
            if owned.result.cpu_ids.iter().any(|&c| c >= fresh.cpus) {
                lost.push((*job, Vec::new()));
            }
        }
        if !lost.is_empty() {
            lost.sort_by_key(|(job, _)| *job);
            return CapacityChange::AllocatedDevicesLost(lost);
        }

        // Safe to rebuild: no held device disappeared. Preserve allocation
        // bitmaps by re-deriving them from owners against the new gpu table.
        self.total_cpus = fresh.cpus;
        self.allocated_cpus.resize(fresh.cpus as usize, false);
        self.total_memory_mb = fresh.memory_mb;
        self.gpus = fresh.gpus.clone();
        self.gpu_allocated = vec![false; self.gpus.len()];
        for owned in self.owners.values() {
            for &sid in &owned.result.gpu_ids {
                if let Some(idx) = self.gpus.iter().position(|g| g.stable_id == sid) {
                    self.gpu_allocated[idx] = true;
                }
            }
        }
        CapacityChange::Applied
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

    /// Per-job GPU stable_ids currently owned on this node. These are the
    /// translated (current) stable_ids `restore_for_job`/`allocate_for_job`
    /// recorded, so an adopted legacy job reports its real GPUs, not the
    /// positional ids its on-disk descriptor still carries.
    pub fn held_job_gpu_ids(&self) -> HashMap<u32, Vec<u64>> {
        self.owners
            .iter()
            .map(|(job, owned)| (*job, owned.result.gpu_ids.clone()))
            .collect()
    }

    /// Stable ids of all currently-allocated GPUs (for diagnostics).
    pub fn allocated_gpu_ids(&self) -> Vec<u64> {
        self.gpu_allocated
            .iter()
            .enumerate()
            .filter(|(_, &a)| a)
            .filter_map(|(i, _)| self.gpus.get(i).map(|g| g.stable_id))
            .collect()
    }

    /// Job ids owning any of `device_ids`, excluding mid-launch owners (a launch
    /// in flight is a real duplicate, not a reclaimable stale owner).
    pub fn conflicting_owners(&self, device_ids: &[u64]) -> Vec<u32> {
        self.owners
            .iter()
            .filter(|(id, _)| !self.launching.contains_key(id))
            .filter(|(_, owned)| owned.result.gpu_ids.iter().any(|g| device_ids.contains(g)))
            .map(|(id, _)| *id)
            .collect()
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
    /// are stable ids, matched against the node's GPU table the same way
    /// `allocate_for_job` records them.
    fn release(&mut self, alloc: &AllocationResult) {
        for &cpu in &alloc.cpu_ids {
            if let Some(a) = self.allocated_cpus.get_mut(cpu as usize) {
                *a = false;
            }
        }
        self.allocated_memory_mb = self.allocated_memory_mb.saturating_sub(alloc.memory_mb);
        for &stable_id in &alloc.gpu_ids {
            if let Some(idx) = self.gpus.iter().position(|g| g.stable_id == stable_id) {
                self.gpu_allocated[idx] = false;
            }
        }
    }

    /// Reserve resources for a job, keyed by job id. GPU stable ids are the
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
        gpu_device_ids: &[u64],
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
        self.release_job(job_id);

        let mut gpu_indices = Vec::with_capacity(gpu_device_ids.len());
        for &id in gpu_device_ids {
            let idx = self
                .gpus
                .iter()
                .position(|g| g.stable_id == id)
                .ok_or(AllocError::GpusUnavailable)?;
            if self.gpu_allocated[idx] || gpu_indices.contains(&idx) {
                return Err(AllocError::GpusUnavailable);
            }
            gpu_indices.push(idx);
        }

        let mut cpu_ids = Vec::new();
        for (i, allocated) in self.allocated_cpus.iter_mut().enumerate() {
            if !*allocated && cpu_ids.len() < cpus as usize {
                *allocated = true;
                cpu_ids.push(i as u32);
            }
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
        gpu_device_ids: &[u64],
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
        let held_gpus: &[u64] = owned.map_or(&[], |result| result.gpu_ids.as_slice());
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
                .position(|g| g.stable_id == id)
                .ok_or(AllocError::GpusUnavailable)?;
            if (self.gpu_allocated[idx] && !held_gpus.contains(&id)) || gpu_indices.contains(&idx) {
                return Err(AllocError::GpusUnavailable);
            }
            gpu_indices.push(idx);
        }
        self.release_job(job_id);

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

    /// Release a job's allocation by id, regardless of which attempt owns it.
    /// Idempotent: releasing an unknown or already-released job is a no-op
    /// returning false. Only for callers that genuinely don't have a specific
    /// attempt to compare (reconcile, foreign-owner reclaim) — everyone else
    /// should use `release_job_if`.
    pub fn release_job(&mut self, job_id: u32) -> bool {
        self.launching.remove(&job_id);
        let Some(owned) = self.owners.remove(&job_id) else {
            return false;
        };
        self.release(&owned.result);
        true
    }

    /// Release a job's allocation only if `run_attempt` is still the current
    /// owner, so a stale caller can't free a different, newer attempt's
    /// reservation that has since superseded the one it thinks it owns.
    pub fn release_job_if(&mut self, job_id: u32, run_attempt: u32) -> bool {
        if self
            .owners
            .get(&job_id)
            .is_some_and(|owned| owned.run_attempt == run_attempt)
        {
            self.release_job(job_id)
        } else {
            false
        }
    }

    /// The attempt currently owning `job_id`'s reservation, if any — lets a
    /// caller resolve what a stale, attempt-less release would otherwise hit.
    pub fn owner_attempt(&self, job_id: u32) -> Option<u32> {
        self.owners.get(&job_id).map(|owned| owned.run_attempt)
    }

    /// Release owned allocations whose job is neither live nor launching within
    /// `launching_ttl`, returning the reclaimed ids. Recovers a failed teardown
    /// or a dropped launch instead of stranding the node until spurd restart.
    pub fn reconcile(
        &mut self,
        live: &HashSet<u32>,
        now: Instant,
        launching_ttl: Duration,
    ) -> Vec<u32> {
        let orphaned: Vec<u32> = self
            .owners
            .keys()
            .copied()
            .filter(|id| {
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
            .collect();
        for &id in &orphaned {
            self.release_job(id);
        }
        orphaned
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
    pub gpu_ids: Vec<u64>,
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
                stable_id: device_id as u64,
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
    fn held_job_gpu_ids_reports_each_owners_stable_ids() {
        let mut node = make_node(64, 256_000, 8, "mi300x");
        node.allocate_for_job(1, 1, 0, 0, &[0, 1]).unwrap();
        node.allocate_for_job(2, 1, 8, 0, &[]).unwrap();

        let held = node.held_job_gpu_ids();
        let mut ids = held.get(&1).cloned().unwrap_or_default();
        ids.sort_unstable();
        assert_eq!(ids, vec![0, 1]);
        // A job holding no GPUs still appears, with an empty list.
        assert_eq!(held.get(&2), Some(&Vec::new()));
    }

    #[test]
    fn test_record_then_release_noncontiguous_device_ids() {
        // Real nodes expose GPUs whose device_id != vec position (DRM render
        // ids 128..135). Release keys by device_id, so a released device must
        // return to the free pool or it is rejected forever.
        let mut node = make_node_with_ids(64, 256_000, vec![128, 129, 130, 131], "mi350x");

        assert!(node.allocate_for_job(1, 1, 0, 0, &[129, 131]).is_ok());
        assert_eq!(node.free_gpus(None), 2);

        assert!(node.release_job(1));
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
        assert!(!node.release_job(1));
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

        assert!(node.release_job(1));
        assert_eq!(node.free_cpus(), 64);
        assert_eq!(node.free_gpus(None), 4);
        assert_eq!(node.free_memory_mb(), 256_000);
        // Idempotent: releasing again is a no-op.
        assert!(!node.release_job(1));
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
        assert!(node.release_job(1));
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
        assert!(!node.release_job(2));
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
        let mut reclaimed = node.reconcile(&live, Instant::now(), ttl);
        reclaimed.sort();
        assert_eq!(reclaimed, vec![2], "only the orphan (job 2) is reclaimed");

        assert!(!node.release_job(2), "job 2 already reconciled");
        assert!(node.release_job(1));
        assert!(node.release_job(3));
        assert_eq!(node.free_gpus(None), 4);
    }

    #[test]
    fn test_reconcile_reclaims_launching_past_ttl() {
        let mut node = make_node_with_ids(64, 256_000, vec![0, 1, 2, 3], "mi300x");
        // A launch reserved but never committed must be reclaimed past the TTL.
        node.allocate_for_job(1, 1, 4, 8_000, &[0]).unwrap();

        let live: HashSet<u32> = HashSet::new();
        let ttl = Duration::from_secs(120);

        assert!(node.reconcile(&live, Instant::now(), ttl).is_empty());
        assert_eq!(node.free_gpus(None), 3, "still reserved within TTL");

        let future = Instant::now() + Duration::from_secs(121);
        assert_eq!(node.reconcile(&live, future, ttl), vec![1]);
        assert_eq!(node.free_gpus(None), 4, "reclaimed after TTL");
    }

    #[test]
    fn test_commit_job_reports_reclaimed_reservation() {
        let mut node = make_node_with_ids(64, 256_000, vec![0, 1], "mi300x");
        node.allocate_for_job(1, 1, 4, 8_000, &[0]).unwrap();
        assert!(
            node.commit_job(1, 1),
            "commit of a live reservation returns true"
        );

        // A launch whose reservation reconcile reclaimed before commit: the
        // owner is gone, so commit must report false and stay a no-op. Keep
        // job 1 in the live set so only the past-TTL launch (job 2) is reclaimed.
        node.allocate_for_job(2, 1, 4, 8_000, &[1]).unwrap();
        let live: HashSet<u32> = [1].into_iter().collect();
        let past = Instant::now() + Duration::from_secs(121);
        node.reconcile(&live, past, Duration::from_secs(120));
        assert!(
            !node.commit_job(2, 1),
            "commit of a reclaimed reservation returns false"
        );
        assert_eq!(
            node.free_gpus(None),
            1,
            "reclaimed GPU stays free after the no-op commit"
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
        node.release_job(7);
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
        node.release_job(7);
        node.allocate_for_job(7, 2, 8, 16_000, &[]).unwrap();

        assert!(!node.commit_job(7, 1), "a superseded attempt cannot commit");

        assert!(
            node.reconcile(&HashSet::new(), Instant::now(), Duration::from_secs(120))
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
        assert!(!node.release_job(2), "rejected replay left an owner entry");

        // Job 1's cores are still exclusively its own to release.
        assert!(node.release_job(1));
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
        assert!(node.release_job_if(9, 3));
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
        assert_eq!(node.reconcile(&live, Instant::now(), ttl), vec![4]);
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
        assert!(node.release_job_if(5, 1), "job 5 lost its owner entry");
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
        assert!(!node.release_job(2));
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
        assert!(!node.release_job(1));
    }

    #[test]
    fn test_release_job_if_spares_a_reused_job_ids_reservation() {
        let mut node = make_node(64, 256_000, 0, "");
        node.allocate_for_job(7, 1, 8, 16_000, &[]).unwrap();
        node.release_job(7);
        node.allocate_for_job(7, 2, 8, 16_000, &[]).unwrap();

        assert!(
            !node.release_job_if(7, 1),
            "a stale attempt must not release a different, current attempt's reservation"
        );
        assert_eq!(node.free_cpus(), 56);
        assert!(
            node.release_job_if(7, 2),
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
        node.release_job(1);
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

    #[test]
    fn update_capacity_grows_and_preserves_live_allocation() {
        let mk = |ids: &[(u32, u64)]| ResourceSet {
            cpus: 8,
            memory_mb: 1024,
            gpus: ids
                .iter()
                .map(|&(d, s)| GpuResource {
                    device_id: d,
                    gpu_type: "mi300x".into(),
                    memory_mb: 0,
                    peer_gpus: vec![],
                    link_type: GpuLinkType::XGMI,
                    stable_id: s,
                })
                .collect(),
            generic: Default::default(),
            generation: 1,
        };
        // start with 2 gpus (stable 128,129); job holds stable 129
        let mut node = NodeAllocation::new("n".into(), &mk(&[(0, 128), (1, 129)]));
        node.allocate_for_job(7, 0, 0, 0, &[129]).unwrap();
        // grow to 4 gpus (SPX->CPX-ish); 129 still present
        let change = node.update_capacity(&mk(&[(0, 128), (1, 129), (2, 130), (3, 131)]));
        assert!(matches!(change, CapacityChange::Applied));
        // free count reflects new total minus the still-held one
        assert_eq!(node.gpus.len(), 4);
        assert_eq!(node.free_gpus(None), 3);
        // the held allocation is intact: releasing it frees exactly one
        assert!(node.release_job(7));
        assert_eq!(node.free_gpus(None), 4);
    }

    #[test]
    fn update_capacity_reports_lost_allocated_device_without_applying() {
        let mk = |ids: &[(u32, u64)]| ResourceSet {
            cpus: 8,
            memory_mb: 1024,
            gpus: ids
                .iter()
                .map(|&(d, s)| GpuResource {
                    device_id: d,
                    gpu_type: "mi300x".into(),
                    memory_mb: 0,
                    peer_gpus: vec![],
                    link_type: GpuLinkType::XGMI,
                    stable_id: s,
                })
                .collect(),
            generic: Default::default(),
            generation: 1,
        };
        let mut node = NodeAllocation::new("n".into(), &mk(&[(0, 128), (1, 129)]));
        node.allocate_for_job(7, 0, 0, 0, &[129]).unwrap();
        // 129 vanishes (the device the job holds)
        let change = node.update_capacity(&mk(&[(0, 128)]));
        match change {
            CapacityChange::AllocatedDevicesLost(v) => assert_eq!(v, vec![(7, vec![129])]),
            other => panic!("expected AllocatedDevicesLost, got {other:?}"),
        }
        // capacity untouched: still 2 gpus, job still holds its device
        assert_eq!(node.gpus.len(), 2);
    }

    #[test]
    fn update_capacity_defers_cpu_shrink_that_would_truncate_held_core() {
        let node_rs = |cpus: u32| ResourceSet {
            cpus,
            memory_mb: 1024,
            gpus: vec![],
            ..Default::default()
        };
        let mut node = NodeAllocation::new("n".into(), &node_rs(8));
        // Hold core 6, which lies above a shrink to 4 cpus.
        let alloc = node.allocate_for_job(7, 0, 8, 0, &[]).unwrap();
        assert!(alloc.cpu_ids.contains(&6));

        let change = node.update_capacity(&node_rs(4));
        match change {
            CapacityChange::AllocatedDevicesLost(v) => assert_eq!(v, vec![(7, Vec::new())]),
            other => panic!("expected AllocatedDevicesLost, got {other:?}"),
        }
        // Capacity untouched: still 8 cpus, core 6 still held.
        assert_eq!(node.total_cpus, 8);
        assert_eq!(node.allocated_cpus.len(), 8);
        assert!(node.allocated_cpus[6]);
    }

    #[test]
    fn update_capacity_applies_benign_cpu_shrink_with_no_held_core_above() {
        let node_rs = |cpus: u32| ResourceSet {
            cpus,
            memory_mb: 1024,
            gpus: vec![],
            ..Default::default()
        };
        let mut node = NodeAllocation::new("n".into(), &node_rs(8));
        // Hold only cores 0..2, below the shrink target.
        node.allocate_for_job(7, 0, 2, 0, &[]).unwrap();

        let change = node.update_capacity(&node_rs(4));
        assert!(matches!(change, CapacityChange::Applied));
        assert_eq!(node.total_cpus, 4);
        assert_eq!(node.allocated_cpus.len(), 4);
    }

    #[test]
    fn allocation_survives_positional_renumber_by_stable_id() {
        // Node has 3 GPUs; device_id is positional (0,1,2), stable_id is durable
        // (128,129,130). A job holds the middle GPU (stable 129). After the first
        // GPU vanishes and capacity is rebuilt with device_id renumbered (0,1) but
        // stable_id preserved (129,130), the job's GPU must still resolve.
        let gpus = vec![
            GpuResource {
                device_id: 0,
                gpu_type: "mi300x".into(),
                memory_mb: 0,
                peer_gpus: vec![],
                link_type: GpuLinkType::XGMI,
                stable_id: 128,
            },
            GpuResource {
                device_id: 1,
                gpu_type: "mi300x".into(),
                memory_mb: 0,
                peer_gpus: vec![],
                link_type: GpuLinkType::XGMI,
                stable_id: 129,
            },
            GpuResource {
                device_id: 2,
                gpu_type: "mi300x".into(),
                memory_mb: 0,
                peer_gpus: vec![],
                link_type: GpuLinkType::XGMI,
                stable_id: 130,
            },
        ];
        let rs = ResourceSet {
            cpus: 8,
            memory_mb: 1024,
            gpus,
            generic: Default::default(),
            generation: 1,
        };
        let mut node = NodeAllocation::new("n".into(), &rs);
        // allocate by stable id 129
        node.allocate_for_job(7, 0, 0, 0, &[129]).unwrap();
        assert_eq!(node.free_gpus(None), 2);
        // release by the same stable id frees it
        assert!(node.release_job(7));
        assert_eq!(node.free_gpus(None), 3);
    }
}
