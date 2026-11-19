// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::NodeConfig;
use crate::cpu_topology::{parse_cpu_list, CpuListError, CpuTopology};
use crate::resource::{GpuResource, ResourceAllocations};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuReservedCpus {
    pub stable_id: u64,
    pub cpus: Vec<u32>,
}

/// Controller-computed split of a node's CPUs. All CPU lists are sorted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoreLayout {
    pub spec_cpus: Vec<u32>,
    pub mem_spec_mb: u64,
    pub gpu_reserved: Vec<GpuReservedCpus>,
    pub general_cpus: Vec<u32>,
    pub daemons_off_spec: bool,
    /// Non-spec CPUs a `cpus` cap takes out of jobs' reach.
    pub trimmed_cpus: Vec<u32>,
}

/// Orders layout deliveries across controller restarts and failovers. Field
/// order is the comparison order: a restarted leader can keep its Raft term,
/// so the controller's durable start counter breaks that tie.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct LayoutRevision {
    pub term: u64,
    pub boot: u64,
    pub seq: u64,
}

/// The `[[nodes]]` settings that shape a node's core layout.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeCoreSettings {
    pub core_spec_count: Option<u32>,
    pub cpu_spec_list: Option<String>,
    pub reserved_memory_mb: u64,
    pub daemons_off_spec: bool,
    pub restricted_cores_per_gpu: Option<u32>,
    /// The `[[nodes]] cpus` cap, 0 when unset or when no cores are reserved per
    /// GPU: only then do the reserved cores have to be picked within it.
    pub cpus_cap: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LayoutError {
    #[error("no CPU topology from spurd; restart or upgrade spurd")]
    MissingTopology,
    #[error("cpu_spec_list: {0}")]
    BadSpecList(#[from] CpuListError),
    #[error("cpu_spec_list names CPU {0}, which is not online")]
    SpecCpuNotOnline(u32),
    #[error("spec cores would take all {cores} cores, leaving none for jobs")]
    SpecTakesAllCores { cores: usize },
    #[error(
        "reserved_memory_mb {reserved_mb} would leave none of the node's {memory_mb} MB for jobs"
    )]
    ReservedMemoryTakesAll { reserved_mb: u64, memory_mb: u64 },
    #[error(
        "GPU {stable_id:#x} has no online cores near it: set cores= in [[devices.gres]] or give it a NUMA node with CPUs"
    )]
    GpuNoAffinity { stable_id: u64 },
    #[error(
        "GPU {stable_id:#x} needs {wanted} reserved cores but only {available} near it are free"
    )]
    TooFewCores {
        stable_id: u64,
        wanted: u32,
        available: u32,
    },
    #[error(
        "GPU {stable_id:#x} needs {wanted} reserved cores but only {available} near it are left within cpus = {cap}"
    )]
    TooFewCoresWithinCap {
        stable_id: u64,
        wanted: u32,
        available: u32,
        cap: u32,
    },
}

#[derive(Clone, Copy)]
enum SpecRequest<'a> {
    Count(u32),
    List(&'a str),
}

impl NodeCoreSettings {
    pub fn from_config(nc: &NodeConfig) -> Self {
        let restricted_cores_per_gpu = nc.restricted_cores_per_gpu.filter(|r| *r > 0);
        NodeCoreSettings {
            core_spec_count: nc.core_spec_count,
            cpu_spec_list: nc.cpu_spec_list.clone(),
            reserved_memory_mb: nc.reserved_memory_mb,
            daemons_off_spec: nc.daemons_off_spec,
            restricted_cores_per_gpu,
            cpus_cap: if restricted_cores_per_gpu.is_some() {
                nc.cpus
            } else {
                0
            },
        }
    }

    fn restricted(&self) -> Option<u32> {
        self.restricted_cores_per_gpu.filter(|r| *r > 0)
    }

    fn spec_request(&self) -> Option<SpecRequest<'_>> {
        if let Some(n) = self.core_spec_count.filter(|n| *n > 0) {
            return Some(SpecRequest::Count(n));
        }
        self.cpu_spec_list
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(SpecRequest::List)
    }
}

impl CoreLayout {
    /// `Ok(None)` when the node sets neither spec cores nor reserved cores per
    /// GPU. `memory_mb` is the node's detected memory, 0 when unknown.
    pub fn compute(
        topology: Option<&CpuTopology>,
        settings: &NodeCoreSettings,
        gpus: &[GpuResource],
        memory_mb: u64,
        previous: &[GpuReservedCpus],
    ) -> Result<Option<CoreLayout>, LayoutError> {
        let spec = settings.spec_request();
        let restricted = settings.restricted();
        if spec.is_none() && restricted.is_none() {
            return Ok(None);
        }
        if memory_mb > 0 && settings.reserved_memory_mb >= memory_mb {
            return Err(LayoutError::ReservedMemoryTakesAll {
                reserved_mb: settings.reserved_memory_mb,
                memory_mb,
            });
        }
        let topology = topology.ok_or(LayoutError::MissingTopology)?;
        let spec_cores = match spec {
            Some(SpecRequest::Count(n)) => pick_spec_cores(topology, n),
            Some(SpecRequest::List(list)) => cores_of_cpus(topology, &parse_cpu_list(list)?)?,
            None => BTreeSet::new(),
        };
        if spec.is_some() && spec_cores.len() >= topology.cores.len() {
            return Err(LayoutError::SpecTakesAllCores {
                cores: topology.cores.len(),
            });
        }
        let cap = restricted.map(|_| settings.cpus_cap).filter(|c| *c > 0);
        let trimmed = match cap {
            Some(cap) => trim_to_cap(topology, &spec_cores, cap),
            None => BTreeSet::new(),
        };
        let excluded: BTreeSet<usize> = spec_cores.union(&trimmed).copied().collect();
        let reserved = match restricted {
            Some(per_gpu) => pick_reserved_cores(topology, gpus, per_gpu, &excluded, previous)
                .map_err(|e| within_cap(e, cap.filter(|_| !trimmed.is_empty())))?,
            None => Vec::new(),
        };
        let taken: BTreeSet<usize> = reserved
            .iter()
            .flat_map(|(_, cores)| cores.iter().copied())
            .collect();
        let general: BTreeSet<usize> = (0..topology.cores.len())
            .filter(|i| !excluded.contains(i) && !taken.contains(i))
            .collect();
        Ok(Some(CoreLayout {
            spec_cpus: cpus_of(topology, &spec_cores),
            mem_spec_mb: settings.reserved_memory_mb,
            gpu_reserved: reserved
                .iter()
                .map(|(stable_id, cores)| GpuReservedCpus {
                    stable_id: *stable_id,
                    cpus: cpus_of(topology, cores),
                })
                .collect(),
            general_cpus: cpus_of(topology, &general),
            daemons_off_spec: settings.daemons_off_spec,
            trimmed_cpus: cpus_of(topology, &trimmed),
        }))
    }

    /// How many reserved CPUs the given GPUs bring.
    pub fn reserved_for(&self, gpu_stable_ids: &[u64]) -> u32 {
        self.gpu_reserved
            .iter()
            .filter(|g| gpu_stable_ids.contains(&g.stable_id))
            .map(|g| g.cpus.len() as u32)
            .sum()
    }

    /// `None` without per-GPU reservations: the node keeps the single-pool check.
    pub fn pools(&self) -> Option<CpuPools> {
        // Every GPU gets exactly R cores or compute fails, so min is exact;
        // were they ever uneven, min still errs toward more general need.
        let per_gpu = self
            .gpu_reserved
            .iter()
            .map(|g| g.cpus.len() as u32)
            .min()?;
        Some(CpuPools {
            general: self.general_cpus.len() as u32,
            reserved_per_gpu: per_gpu,
            reserved_total: self.gpu_reserved.iter().map(|g| g.cpus.len() as u32).sum(),
        })
    }

    /// Tag `alloc` with this layout and the CPUs its GPUs' reserved cores cover.
    pub fn stamp(&self, alloc: &mut ResourceAllocations) {
        let reserved = self.reserved_for(&alloc.device_ids("gpu"));
        alloc.reserved_cpus = reserved_share(alloc.cpus, reserved);
        alloc.layout_fingerprint = self.fingerprint();
    }

    /// FNV-1a 64 over: spec_cpus, mem_spec_mb, gpu_reserved (sorted by
    /// stable_id; each stable_id then its cpus), general_cpus, daemons_off_spec,
    /// then trimmed_cpus only when non-empty, so layouts without a cap keep
    /// their hash. Each list is its length (u64) then its items (u32); integers
    /// are little-endian; the bool is one byte. Stable across builds by design.
    pub fn fingerprint(&self) -> u64 {
        let mut h = Fnv::new();
        h.list(&self.spec_cpus);
        h.u64(self.mem_spec_mb);
        let mut gpus: Vec<&GpuReservedCpus> = self.gpu_reserved.iter().collect();
        gpus.sort_by_key(|g| g.stable_id);
        h.u64(gpus.len() as u64);
        for g in gpus {
            h.u64(g.stable_id);
            h.list(&g.cpus);
        }
        h.list(&self.general_cpus);
        h.bytes(&[u8::from(self.daemons_off_spec)]);
        if !self.trimmed_cpus.is_empty() {
            h.list(&self.trimmed_cpus);
        }
        nonzero(h.finish())
    }
}

/// 0 on the wire means "no layout held", so a layout must never hash to it.
fn nonzero(hash: u64) -> u64 {
    if hash == 0 {
        1
    } else {
        hash
    }
}

struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    fn bytes(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    fn u64(&mut self, v: u64) {
        self.bytes(&v.to_le_bytes());
    }
    fn list(&mut self, v: &[u32]) {
        self.u64(v.len() as u64);
        for x in v {
            self.bytes(&x.to_le_bytes());
        }
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

fn pick_spec_cores(topology: &CpuTopology, count: u32) -> BTreeSet<usize> {
    let all: BTreeSet<usize> = (0..topology.cores.len()).collect();
    highest_first(topology, &all)
        .into_iter()
        .take(count as usize)
        .collect()
}

/// Slurm's order: the highest local core index first, sockets from highest to
/// lowest at each index (`node_conf_select_spec_cores`), over `eligible` only.
fn highest_first(topology: &CpuTopology, eligible: &BTreeSet<usize>) -> Vec<usize> {
    let mut by_socket: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
    for &i in eligible {
        by_socket
            .entry(topology.cores[i].socket)
            .or_default()
            .push(i);
    }
    let widest = by_socket.values().map(Vec::len).max().unwrap_or(0);
    let mut order = Vec::with_capacity(eligible.len());
    for local in (0..widest).rev() {
        for cores in by_socket.values().rev() {
            if let Some(&i) = cores.get(local) {
                order.push(i);
            }
        }
    }
    order
}

/// Whole non-spec cores to drop so the CPUs jobs may use fit in `cap`; on SMT
/// an odd cap rounds down rather than split a core. Like Slurm's configured
/// core count, the lowest cores in socket-major order are kept, so the last
/// socket's highest cores go first.
fn trim_to_cap(topology: &CpuTopology, spec_cores: &BTreeSet<usize>, cap: u32) -> BTreeSet<usize> {
    let allowed: Vec<usize> = (0..topology.cores.len())
        .filter(|i| !spec_cores.contains(i))
        .collect();
    let cpus: u32 = allowed
        .iter()
        .map(|&i| topology.cores[i].cpus.len() as u32)
        .sum();
    let mut excess = cpus.saturating_sub(cap);
    let mut trimmed = BTreeSet::new();
    for i in allowed.into_iter().rev() {
        if excess == 0 {
            break;
        }
        excess = excess.saturating_sub(topology.cores[i].cpus.len() as u32);
        trimmed.insert(i);
    }
    trimmed
}

/// Names the cap when it is what left a GPU short of reserved cores.
fn within_cap(e: LayoutError, cap: Option<u32>) -> LayoutError {
    match (e, cap) {
        (
            LayoutError::TooFewCores {
                stable_id,
                wanted,
                available,
            },
            Some(cap),
        ) => LayoutError::TooFewCoresWithinCap {
            stable_id,
            wanted,
            available,
            cap,
        },
        (e, _) => e,
    }
}

/// A node's CPU pools as counts, all the scheduler needs to admit a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuPools {
    pub general: u32,
    pub reserved_per_gpu: u32,
    pub reserved_total: u32,
}

impl CpuPools {
    /// Whether `cpus` CPUs on `gpus` of the node's GPUs fit beside `used`. Only
    /// the general part is checked: a GPU's reserved CPUs go to the one job
    /// holding that GPU, so they are always free when it gets the GPU.
    pub fn admits(&self, used: &ResourceAllocations, cpus: u32, gpus: u32) -> bool {
        let reserved_used = used.reserved_cpus.min(self.reserved_total);
        let general_used = used.cpus.saturating_sub(reserved_used);
        let share = reserved_share(cpus, gpus.saturating_mul(self.reserved_per_gpu));
        general_used.saturating_add(cpus - share) <= self.general
    }
}

/// CPUs of a job that come from its own GPUs' reserved cores; the rest come
/// from the general pool. Scheduler fit and agent picking both use this.
pub fn reserved_share(cpus: u32, reserved_of_its_gpus: u32) -> u32 {
    cpus.min(reserved_of_its_gpus)
}

/// A GPU's stable id and its affinity cores, resolved once per compute.
type GpuNear = (u64, Result<BTreeSet<usize>, LayoutError>);

/// Each GPU still present first keeps its previous cores that are still near
/// it, online and not excluded; only then do GPUs short of `per_gpu` take the
/// lowest free cores, so a new GPU never moves a held one's. Falls back to
/// the fresh rule when history fragments the cores too much to fit.
fn pick_reserved_cores(
    topology: &CpuTopology,
    gpus: &[GpuResource],
    per_gpu: u32,
    excluded: &BTreeSet<usize>,
    previous: &[GpuReservedCpus],
) -> Result<Vec<(u64, BTreeSet<usize>)>, LayoutError> {
    let mut ordered: Vec<&GpuResource> = gpus.iter().collect();
    ordered.sort_by_key(|g| g.stable_id);
    // Resolved once so a bad `cores=` warns once per compute; each phase still
    // surfaces an affinity error where it reaches that GPU, as before.
    let near: Vec<GpuNear> = ordered
        .iter()
        .map(|g| (g.stable_id, gpu_affinity_cores(topology, g)))
        .collect();
    if previous.is_empty() {
        return fill_reserved_cores(&near, per_gpu, excluded, BTreeMap::new());
    }
    let mut taken = BTreeSet::new();
    let mut kept: BTreeMap<u64, BTreeSet<usize>> = BTreeMap::new();
    for (stable_id, cores) in &near {
        let keep = keep_previous_cores(
            topology, *stable_id, cores, per_gpu, excluded, &taken, previous,
        )?;
        taken.extend(keep.iter().copied());
        kept.entry(*stable_id).or_default().extend(keep);
    }
    fill_reserved_cores(&near, per_gpu, excluded, kept)
        .or_else(|_| fill_reserved_cores(&near, per_gpu, excluded, BTreeMap::new()))
}

/// Phase 1 for one GPU: its previous cores that are still valid, lowest first.
fn keep_previous_cores(
    topology: &CpuTopology,
    stable_id: u64,
    near: &Result<BTreeSet<usize>, LayoutError>,
    per_gpu: u32,
    excluded: &BTreeSet<usize>,
    taken: &BTreeSet<usize>,
    previous: &[GpuReservedCpus],
) -> Result<BTreeSet<usize>, LayoutError> {
    let Some(prev) = previous.iter().find(|p| p.stable_id == stable_id) else {
        return Ok(BTreeSet::new());
    };
    let near = near.as_ref().map_err(Clone::clone)?;
    Ok(prev
        .cpus
        .iter()
        .filter_map(|&cpu| topology.core_of_cpu(cpu))
        .filter(|c| near.contains(c) && !excluded.contains(c) && !taken.contains(c))
        .collect::<BTreeSet<usize>>()
        .into_iter()
        .take(per_gpu as usize)
        .collect())
}

/// Phase 2: in stable-id order each GPU tops its kept cores up to `per_gpu`
/// with the lowest free cores of its affinity. An empty `kept` is Slurm's
/// fresh rule.
fn fill_reserved_cores(
    near: &[GpuNear],
    per_gpu: u32,
    excluded: &BTreeSet<usize>,
    mut kept: BTreeMap<u64, BTreeSet<usize>>,
) -> Result<Vec<(u64, BTreeSet<usize>)>, LayoutError> {
    let mut taken: BTreeSet<usize> = kept.values().flatten().copied().collect();
    let mut reserved = Vec::with_capacity(near.len());
    for (stable_id, cores) in near {
        let mut picked = kept.remove(stable_id).unwrap_or_default();
        let free: Vec<usize> = cores
            .as_ref()
            .map_err(Clone::clone)?
            .iter()
            .copied()
            .filter(|i| !excluded.contains(i) && !taken.contains(i))
            .collect();
        let short = (per_gpu as usize).saturating_sub(picked.len());
        if free.len() < short {
            return Err(LayoutError::TooFewCores {
                stable_id: *stable_id,
                wanted: per_gpu,
                available: (picked.len() + free.len()) as u32,
            });
        }
        picked.extend(free.into_iter().take(short));
        taken.extend(picked.iter().copied());
        reserved.push((*stable_id, picked));
    }
    Ok(reserved)
}

/// Cores near a GPU (its configured CPUs if any, else its NUMA node's), plus
/// the configured CPUs not online on the node.
fn near_cores(topology: &CpuTopology, gpu: &GpuResource) -> (BTreeSet<usize>, Vec<u32>) {
    if gpu.affinity_cpus.is_empty() {
        let cores = gpu.numa_node.map_or_else(BTreeSet::new, |numa| {
            (0..topology.cores.len())
                .filter(|&i| topology.cores[i].numa_node == Some(numa))
                .collect()
        });
        return (cores, Vec::new());
    }
    let mut dropped = Vec::new();
    let cores = gpu
        .affinity_cpus
        .iter()
        .filter_map(|&cpu| {
            let core = topology.core_of_cpu(cpu);
            if core.is_none() {
                dropped.push(cpu);
            }
            core
        })
        .collect();
    (cores, dropped)
}

/// Cores near a GPU: its configured CPUs if any, else its NUMA node's. CPUs
/// not online on the node are ignored.
fn gpu_affinity_cores(
    topology: &CpuTopology,
    gpu: &GpuResource,
) -> Result<BTreeSet<usize>, LayoutError> {
    let (cores, dropped) = near_cores(topology, gpu);
    // One warning per GPU per layout recompute (not per CPU), so a wide
    // misconfiguration doesn't flood the log; still repeats each
    // NodeRegister/NodeUpdate apply and Raft replay.
    if !dropped.is_empty() {
        tracing::warn!(
            stable_id = gpu.stable_id,
            dropped_cpus = ?dropped,
            "cores= names CPUs beyond this node's topology; dropping them"
        );
    }
    if cores.is_empty() {
        return Err(LayoutError::GpuNoAffinity {
            stable_id: gpu.stable_id,
        });
    }
    Ok(cores)
}

/// CPUs `restricted_cores_per_gpu` picks a GPU's reserved cores from; empty
/// when it has no `cores=` and no NUMA node. Silent, for display paths.
pub fn gpu_affinity_cpus(topology: &CpuTopology, gpu: &GpuResource) -> Vec<u32> {
    cpus_of(topology, &near_cores(topology, gpu).0)
}

fn cpus_of(topology: &CpuTopology, cores: &BTreeSet<usize>) -> Vec<u32> {
    let mut cpus: Vec<u32> = cores
        .iter()
        .flat_map(|&i| topology.cores[i].cpus.iter().copied())
        .collect();
    cpus.sort_unstable();
    cpus
}

fn cores_of_cpus(topology: &CpuTopology, cpus: &[u32]) -> Result<BTreeSet<usize>, LayoutError> {
    cpus.iter()
        .map(|&cpu| {
            topology
                .core_of_cpu(cpu)
                .ok_or(LayoutError::SpecCpuNotOnline(cpu))
        })
        .collect()
}

/// Matching rule of a `[[nodes]]` entry: `names` plus `selector`, the latter
/// as a sorted map so two entries with the same pairs in a different
/// `HashMap` iteration order still compare equal.
type EntryMatcher = (String, BTreeMap<String, String>);

fn entry_matcher(nc: &NodeConfig) -> EntryMatcher {
    (
        nc.names.clone(),
        nc.selector
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    )
}

/// `[[nodes]]` entries whose change could alter some node's layout. The first
/// matching entry wins, so every entry up to the last one setting spec cores
/// is compared, matchers and order included.
pub fn restart_only_node_changes(old: &[NodeConfig], new: &[NodeConfig]) -> Vec<String> {
    let unset = NodeCoreSettings {
        daemons_off_spec: true,
        ..Default::default()
    };
    // Memory-only entries have no layout to keep in step, so they stay live.
    let restart_relevant = |nc: &NodeConfig| -> NodeCoreSettings {
        let s = NodeCoreSettings::from_config(nc);
        let has_core = s.core_spec_count.is_some_and(|n| n > 0)
            || s.cpu_spec_list
                .as_deref()
                .is_some_and(|l| !l.trim().is_empty())
            || s.restricted().is_some();
        if has_core {
            s
        } else {
            unset.clone()
        }
    };
    let keyed = |nodes: &[NodeConfig]| -> Vec<(EntryMatcher, NodeCoreSettings)> {
        nodes
            .iter()
            .map(|nc| (entry_matcher(nc), restart_relevant(nc)))
            .collect()
    };
    let prefix_len = |entries: &[(EntryMatcher, NodeCoreSettings)]| {
        entries
            .iter()
            .rposition(|(_, s)| *s != unset)
            .map_or(0, |i| i + 1)
    };

    let old_entries = keyed(old);
    let new_entries = keyed(new);
    let old_prefix = &old_entries[..prefix_len(&old_entries)];
    let new_prefix = &new_entries[..prefix_len(&new_entries)];
    if old_prefix == new_prefix {
        return Vec::new();
    }

    let mut changed = Vec::new();
    for i in 0..old_prefix.len().max(new_prefix.len()) {
        if old_prefix.get(i) == new_prefix.get(i) {
            continue;
        }
        if i < old_prefix.len() {
            let key = entry_key(&old[i]);
            if !changed.contains(&key) {
                changed.push(key);
            }
        }
        if i < new_prefix.len() {
            let key = entry_key(&new[i]);
            if !changed.contains(&key) {
                changed.push(key);
            }
        }
    }
    changed
}

fn entry_key(nc: &NodeConfig) -> String {
    if !nc.names.is_empty() {
        return nc.names.clone();
    }
    let selector: BTreeMap<&String, &String> = nc.selector.iter().collect();
    format!("{selector:?}")
}

impl From<&CoreLayout> for spur_proto::proto::CoreLayout {
    fn from(l: &CoreLayout) -> Self {
        spur_proto::proto::CoreLayout {
            spec_cpus: l.spec_cpus.clone(),
            mem_spec_mb: l.mem_spec_mb,
            gpu_reserved: l
                .gpu_reserved
                .iter()
                .map(|g| spur_proto::proto::GpuReservedCpus {
                    stable_id: g.stable_id,
                    cpus: g.cpus.clone(),
                })
                .collect(),
            general_cpus: l.general_cpus.clone(),
            daemons_off_spec: l.daemons_off_spec,
            trimmed_cpus: l.trimmed_cpus.clone(),
        }
    }
}

impl From<LayoutRevision> for spur_proto::proto::LayoutRevision {
    fn from(r: LayoutRevision) -> Self {
        spur_proto::proto::LayoutRevision {
            term: r.term,
            boot: r.boot,
            seq: r.seq,
        }
    }
}

impl From<spur_proto::proto::LayoutRevision> for LayoutRevision {
    fn from(r: spur_proto::proto::LayoutRevision) -> Self {
        LayoutRevision {
            term: r.term,
            boot: r.boot,
            seq: r.seq,
        }
    }
}

impl From<spur_proto::proto::CoreLayout> for CoreLayout {
    fn from(l: spur_proto::proto::CoreLayout) -> Self {
        CoreLayout {
            spec_cpus: l.spec_cpus,
            mem_spec_mb: l.mem_spec_mb,
            gpu_reserved: l
                .gpu_reserved
                .into_iter()
                .map(|g| GpuReservedCpus {
                    stable_id: g.stable_id,
                    cpus: g.cpus,
                })
                .collect(),
            general_cpus: l.general_cpus,
            daemons_off_spec: l.daemons_off_spec,
            trimmed_cpus: l.trimmed_cpus,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu_topology::{CoreInfo, CpuTopology};
    use crate::resource::AllocatedDevice;

    /// Golden value: FNV-1a 64 over the canonical encoding documented on
    /// `CoreLayout::fingerprint`, computed independently in Python. Pins
    /// cross-build stability.
    const GOLDEN_FINGERPRINT: u64 = 0x2f4ffdf52174e0df;

    /// 2 sockets x 64 cores x 2 threads: socket s, local core c -> CPUs s*64+c, s*64+c+128.
    fn example_node() -> CpuTopology {
        let mut cores = Vec::new();
        for s in 0..2u32 {
            for c in 0..64u32 {
                let cpu = s * 64 + c;
                cores.push(CoreInfo {
                    socket: s,
                    numa_node: Some(s),
                    cpus: vec![cpu, cpu + 128],
                });
            }
        }
        CpuTopology { cores }
    }

    fn spec_count(n: u32) -> NodeCoreSettings {
        NodeCoreSettings {
            core_spec_count: Some(n),
            reserved_memory_mb: 16384,
            daemons_off_spec: true,
            ..Default::default()
        }
    }

    fn spec_list(list: &str) -> NodeCoreSettings {
        NodeCoreSettings {
            cpu_spec_list: Some(list.into()),
            daemons_off_spec: true,
            ..Default::default()
        }
    }

    #[test]
    fn core_spec_count_one_takes_the_highest_core_on_the_highest_socket() {
        let layout = CoreLayout::compute(Some(&example_node()), &spec_count(1), &[], 0, &[])
            .unwrap()
            .unwrap();
        assert_eq!(layout.spec_cpus, vec![127, 255]);
        assert_eq!(layout.general_cpus.len(), 254);
        assert!(!layout.general_cpus.contains(&127));
        assert_eq!(layout.mem_spec_mb, 16384);
        assert!(layout.daemons_off_spec);
        assert!(layout.gpu_reserved.is_empty());
    }

    #[test]
    fn core_spec_count_round_robins_down_across_sockets() {
        let layout = CoreLayout::compute(Some(&example_node()), &spec_count(3), &[], 0, &[])
            .unwrap()
            .unwrap();
        // core 63 of socket 1, core 63 of socket 0, then core 62 of socket 1.
        assert_eq!(layout.spec_cpus, vec![63, 126, 127, 191, 254, 255]);
    }

    #[test]
    fn non_smt_and_single_socket_nodes_follow_the_same_order() {
        let topo = CpuTopology {
            cores: (0..4)
                .map(|c| CoreInfo {
                    socket: 0,
                    numa_node: None,
                    cpus: vec![c],
                })
                .collect(),
        };
        let layout = CoreLayout::compute(Some(&topo), &spec_count(2), &[], 0, &[])
            .unwrap()
            .unwrap();
        assert_eq!(layout.spec_cpus, vec![2, 3]);
        assert_eq!(layout.general_cpus, vec![0, 1]);
    }

    #[test]
    fn non_contiguous_sockets_with_uneven_core_counts_follow_slurm_order() {
        // Non-SMT: socket 0 has 3 cores (local 0-2), socket 2 has 2 (local 0-1).
        // Slurm order: highest local index first, highest socket first at each
        // index, skipping sockets that lack that local index.
        let topo = CpuTopology {
            cores: vec![
                CoreInfo {
                    socket: 0,
                    numa_node: None,
                    cpus: vec![0],
                },
                CoreInfo {
                    socket: 0,
                    numa_node: None,
                    cpus: vec![1],
                },
                CoreInfo {
                    socket: 0,
                    numa_node: None,
                    cpus: vec![2],
                },
                CoreInfo {
                    socket: 2,
                    numa_node: None,
                    cpus: vec![10],
                },
                CoreInfo {
                    socket: 2,
                    numa_node: None,
                    cpus: vec![11],
                },
            ],
        };
        // local 2: only socket 0 has it -> core 2 (cpu 2).
        // local 1: socket 2 first -> core (cpu 11), then socket 0 -> core (cpu 1).
        let two = CoreLayout::compute(Some(&topo), &spec_count(2), &[], 0, &[])
            .unwrap()
            .unwrap();
        assert_eq!(two.spec_cpus, vec![2, 11]);
        assert_eq!(two.general_cpus, vec![0, 1, 10]);

        let three = CoreLayout::compute(Some(&topo), &spec_count(3), &[], 0, &[])
            .unwrap()
            .unwrap();
        assert_eq!(three.spec_cpus, vec![1, 2, 11]);
        assert_eq!(three.general_cpus, vec![0, 10]);
    }

    #[test]
    fn cpu_spec_list_is_widened_to_whole_cores() {
        let one = CoreLayout::compute(Some(&example_node()), &spec_list("127"), &[], 0, &[])
            .unwrap()
            .unwrap();
        let both = CoreLayout::compute(Some(&example_node()), &spec_list("127,255"), &[], 0, &[])
            .unwrap()
            .unwrap();
        assert_eq!(one.spec_cpus, vec![127, 255]);
        assert_eq!(one, both);
    }

    #[test]
    fn unset_zero_blank_and_memory_only_settings_give_no_layout() {
        let topo = example_node();
        for settings in [
            NodeCoreSettings::default(),
            spec_count(0),
            spec_list("  "),
            NodeCoreSettings {
                reserved_memory_mb: 16384,
                ..Default::default()
            },
        ] {
            assert_eq!(
                CoreLayout::compute(Some(&topo), &settings, &[], 0, &[]),
                Ok(None),
                "{settings:?}"
            );
        }
        assert_eq!(
            CoreLayout::compute(None, &NodeCoreSettings::default(), &[], 0, &[]),
            Ok(None)
        );
    }

    #[test]
    fn layout_errors_name_the_problem() {
        let topo = example_node();
        assert_eq!(
            CoreLayout::compute(None, &spec_count(1), &[], 0, &[]),
            Err(LayoutError::MissingTopology)
        );
        assert_eq!(
            CoreLayout::compute(Some(&topo), &spec_list("300"), &[], 0, &[]),
            Err(LayoutError::SpecCpuNotOnline(300))
        );
        assert_eq!(
            CoreLayout::compute(Some(&topo), &spec_count(128), &[], 0, &[]),
            Err(LayoutError::SpecTakesAllCores { cores: 128 })
        );
        assert_eq!(
            CoreLayout::compute(Some(&topo), &spec_count(500), &[], 0, &[]),
            Err(LayoutError::SpecTakesAllCores { cores: 128 })
        );
        assert!(LayoutError::MissingTopology
            .to_string()
            .contains("restart or upgrade spurd"));
    }

    #[test]
    fn a_reservation_covering_all_memory_is_a_layout_error() {
        let settings = NodeCoreSettings {
            reserved_memory_mb: 8192,
            ..spec_count(1)
        };
        assert_eq!(
            CoreLayout::compute(Some(&example_node()), &settings, &[], 8192, &[]),
            Err(LayoutError::ReservedMemoryTakesAll {
                reserved_mb: 8192,
                memory_mb: 8192
            })
        );
        assert!(CoreLayout::compute(Some(&example_node()), &settings, &[], 8193, &[]).is_ok());
        assert!(CoreLayout::compute(Some(&example_node()), &settings, &[], 0, &[]).is_ok());
    }

    #[test]
    fn fingerprint_is_deterministic_and_tracks_changes() {
        let topo = example_node();
        let a = CoreLayout::compute(Some(&topo), &spec_count(1), &[], 0, &[])
            .unwrap()
            .unwrap();
        let b = CoreLayout::compute(Some(&topo), &spec_count(1), &[], 0, &[])
            .unwrap()
            .unwrap();
        let c = CoreLayout::compute(Some(&topo), &spec_count(2), &[], 0, &[])
            .unwrap()
            .unwrap();
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert_ne!(a.fingerprint(), c.fingerprint());
        assert_ne!(a.fingerprint(), 0);
    }

    #[test]
    fn fingerprint_matches_the_documented_encoding() {
        let layout = CoreLayout {
            spec_cpus: vec![127, 255],
            mem_spec_mb: 16384,
            gpu_reserved: vec![],
            general_cpus: vec![0, 1],
            daemons_off_spec: true,
            trimmed_cpus: vec![],
        };
        assert_eq!(layout.fingerprint(), GOLDEN_FINGERPRINT);
        let trimmed = CoreLayout {
            trimmed_cpus: vec![2, 3],
            ..layout
        };
        assert_ne!(trimmed.fingerprint(), GOLDEN_FINGERPRINT);
    }

    #[test]
    fn revisions_order_by_term_then_boot_then_seq() {
        let rev = |term, boot, seq| LayoutRevision { term, boot, seq };
        assert!(rev(1, 9, 9) < rev(2, 0, 0));
        assert!(
            rev(2, 1, 9) < rev(2, 2, 0),
            "a restarted leader keeps its term"
        );
        assert!(rev(2, 2, 1) < rev(2, 2, 2));
    }

    #[test]
    fn a_zero_hash_is_reported_as_nonzero() {
        assert_eq!(nonzero(0), 1);
        assert_eq!(nonzero(42), 42);
    }

    #[test]
    fn layout_survives_the_proto_round_trip() {
        let layout = CoreLayout {
            spec_cpus: vec![127, 255],
            mem_spec_mb: 16384,
            gpu_reserved: vec![GpuReservedCpus {
                stable_id: 7,
                cpus: vec![0, 128],
            }],
            general_cpus: vec![1, 129],
            daemons_off_spec: false,
            trimmed_cpus: vec![2, 130],
        };
        let wire = spur_proto::proto::CoreLayout::from(&layout);
        assert_eq!(CoreLayout::from(wire), layout);
    }

    #[test]
    fn settings_come_from_the_nodes_entry() {
        let nc: crate::config::NodeConfig = toml::from_str(
            r#"
            names = "n1"
            core_spec_count = 1
            mem_spec_limit = 2048
            daemons_off_spec = false
            restricted_cores_per_gpu = 16
            "#,
        )
        .unwrap();
        assert_eq!(
            NodeCoreSettings::from_config(&nc),
            NodeCoreSettings {
                core_spec_count: Some(1),
                cpu_spec_list: None,
                reserved_memory_mb: 2048,
                daemons_off_spec: false,
                restricted_cores_per_gpu: Some(16),
                cpus_cap: 0,
            }
        );
    }

    #[test]
    fn the_cpus_cap_shapes_the_layout_only_beside_reserved_cores() {
        let parse = |s: &str| -> crate::config::NodeConfig { toml::from_str(s).unwrap() };
        let with_restricted = parse("names = \"n1\"\ncpus = 200\nrestricted_cores_per_gpu = 8\n");
        assert_eq!(
            NodeCoreSettings::from_config(&with_restricted).cpus_cap,
            200
        );
        let spec_only = parse("names = \"n1\"\ncpus = 200\ncore_spec_count = 1\n");
        assert_eq!(NodeCoreSettings::from_config(&spec_only).cpus_cap, 0);
    }

    #[test]
    fn restart_only_changes_are_reported_per_nodes_entry() {
        let parse = |s: &str| -> Vec<crate::config::NodeConfig> {
            #[derive(serde::Deserialize)]
            struct Nodes {
                nodes: Vec<crate::config::NodeConfig>,
            }
            toml::from_str::<Nodes>(s).unwrap().nodes
        };
        let old = parse(
            "[[nodes]]\nnames = \"a\"\ncore_spec_count = 1\n[[nodes]]\nnames = \"b\"\nweight = 2\n",
        );
        let same_but_weight = parse(
            "[[nodes]]\nnames = \"a\"\ncore_spec_count = 1\n[[nodes]]\nnames = \"b\"\nweight = 5\n",
        );
        assert!(restart_only_node_changes(&old, &same_but_weight).is_empty());
        let changed = parse("[[nodes]]\nnames = \"a\"\ncore_spec_count = 2\n[[nodes]]\nnames = \"b\"\nreserved_memory_mb = 512\n");
        assert_eq!(
            // a's core-spec change is restart-only; b's memory-only reservation
            // is live-reloadable, so only a is reported.
            restart_only_node_changes(&old, &changed),
            vec!["a".to_string()]
        );
        let removed = parse("[[nodes]]\nnames = \"b\"\nweight = 2\n");
        assert_eq!(
            restart_only_node_changes(&old, &removed),
            vec!["a".to_string()]
        );
        let gpu_old = parse("[[nodes]]\nnames = \"g\"\nrestricted_cores_per_gpu = 8\n");
        let gpu_new = parse("[[nodes]]\nnames = \"g\"\nrestricted_cores_per_gpu = 16\n");
        assert_eq!(
            restart_only_node_changes(&gpu_old, &gpu_new),
            vec!["g".to_string()]
        );
        let capped = parse("[[nodes]]\nnames = \"g\"\nrestricted_cores_per_gpu = 8\ncpus = 200\n");
        assert_eq!(
            restart_only_node_changes(&gpu_old, &capped),
            vec!["g".to_string()],
            "a cap reshapes a layout with reserved cores"
        );
        let spec_capped = parse("[[nodes]]\nnames = \"a\"\ncore_spec_count = 1\ncpus = 64\n[[nodes]]\nnames = \"b\"\nweight = 2\n");
        assert!(restart_only_node_changes(&old, &spec_capped).is_empty());
    }

    #[test]
    fn a_memory_only_reservation_change_is_live_reloadable() {
        let parse = |s: &str| -> Vec<crate::config::NodeConfig> {
            #[derive(serde::Deserialize)]
            struct Nodes {
                nodes: Vec<crate::config::NodeConfig>,
            }
            toml::from_str::<Nodes>(s).unwrap().nodes
        };
        let old = parse("[[nodes]]\nnames = \"a\"\nreserved_memory_mb = 1024\n");
        let changed = parse("[[nodes]]\nnames = \"a\"\nreserved_memory_mb = 2048\n");
        assert!(
            restart_only_node_changes(&old, &changed).is_empty(),
            "a memory-only reservation change stays live-reloadable"
        );
        // Adding spec cores on top is restart-only.
        let with_spec =
            parse("[[nodes]]\nnames = \"a\"\ncore_spec_count = 1\nreserved_memory_mb = 2048\n");
        assert_eq!(
            restart_only_node_changes(&old, &with_spec),
            vec!["a".to_string()]
        );
    }

    #[test]
    fn duplicate_entries_are_compared_one_by_one() {
        let parse = |s: &str| -> Vec<crate::config::NodeConfig> {
            #[derive(serde::Deserialize)]
            struct Nodes {
                nodes: Vec<crate::config::NodeConfig>,
            }
            toml::from_str::<Nodes>(s).unwrap().nodes
        };
        let old = parse(
            "[[nodes]]\nnames = \"a\"\ncore_spec_count = 1\n[[nodes]]\nnames = \"a\"\nweight = 2\n",
        );
        let first_changed = parse(
            "[[nodes]]\nnames = \"a\"\ncore_spec_count = 2\n[[nodes]]\nnames = \"a\"\nweight = 2\n",
        );
        assert_eq!(
            restart_only_node_changes(&old, &first_changed),
            vec!["a".to_string()]
        );
        let second_changed = parse(
            "[[nodes]]\nnames = \"a\"\ncore_spec_count = 1\n[[nodes]]\nnames = \"a\"\nweight = 2\ncore_spec_count = 1\n",
        );
        assert_eq!(
            restart_only_node_changes(&old, &second_changed),
            vec!["a".to_string()]
        );
    }

    #[test]
    fn a_matcher_change_shadowing_a_later_spec_entry_is_reported() {
        let parse = |s: &str| -> Vec<crate::config::NodeConfig> {
            #[derive(serde::Deserialize)]
            struct Nodes {
                nodes: Vec<crate::config::NodeConfig>,
            }
            toml::from_str::<Nodes>(s).unwrap().nodes
        };
        let old = parse(
            "[[nodes]]\nnames = \"n1\"\nweight = 2\n[[nodes]]\nnames = \"ALL\"\ncore_spec_count = 1\n",
        );
        // Only the first entry's matcher widens; its settings are unchanged,
        // so a per-key diff would miss that it now shadows more nodes.
        let new = parse(
            "[[nodes]]\nnames = \"n[1-2]\"\nweight = 2\n[[nodes]]\nnames = \"ALL\"\ncore_spec_count = 1\n",
        );
        assert!(!restart_only_node_changes(&old, &new).is_empty());
    }

    #[test]
    fn reordering_entries_ahead_of_a_spec_entry_is_reported() {
        let parse = |s: &str| -> Vec<crate::config::NodeConfig> {
            #[derive(serde::Deserialize)]
            struct Nodes {
                nodes: Vec<crate::config::NodeConfig>,
            }
            toml::from_str::<Nodes>(s).unwrap().nodes
        };
        let old = parse(
            "[[nodes]]\nnames = \"x\"\nweight = 1\n[[nodes]]\nnames = \"y\"\nweight = 2\n[[nodes]]\nnames = \"ALL\"\ncore_spec_count = 1\n",
        );
        // Same entries, swapped order: no per-key difference, but node x and
        // y can now resolve against each other's old shadowed settings.
        let new = parse(
            "[[nodes]]\nnames = \"y\"\nweight = 2\n[[nodes]]\nnames = \"x\"\nweight = 1\n[[nodes]]\nnames = \"ALL\"\ncore_spec_count = 1\n",
        );
        assert!(!restart_only_node_changes(&old, &new).is_empty());
    }

    #[test]
    fn changes_after_the_last_spec_entry_are_not_reported() {
        let parse = |s: &str| -> Vec<crate::config::NodeConfig> {
            #[derive(serde::Deserialize)]
            struct Nodes {
                nodes: Vec<crate::config::NodeConfig>,
            }
            toml::from_str::<Nodes>(s).unwrap().nodes
        };
        let old = parse(
            "[[nodes]]\nnames = \"ALL\"\ncore_spec_count = 1\n[[nodes]]\nnames = \"tail\"\nweight = 1\n",
        );
        let new = parse(
            "[[nodes]]\nnames = \"ALL\"\ncore_spec_count = 1\n[[nodes]]\nnames = \"tail\"\nweight = 99\n",
        );
        assert!(restart_only_node_changes(&old, &new).is_empty());
    }

    fn gpus_on_numa(per_numa: u32) -> Vec<GpuResource> {
        (0..2u32)
            .flat_map(|numa| (0..per_numa).map(move |_| numa))
            .enumerate()
            .map(|(n, numa)| GpuResource {
                device_id: n as u32,
                gpu_type: "mi300x".into(),
                memory_mb: 0,
                peer_gpus: vec![],
                link_type: crate::resource::GpuLinkType::XGMI,
                stable_id: 0x100 + n as u64,
                topology: None,
                numa_node: Some(numa),
                affinity_cpus: vec![],
            })
            .collect()
    }

    fn restricted(r: u32) -> NodeCoreSettings {
        NodeCoreSettings {
            restricted_cores_per_gpu: Some(r),
            daemons_off_spec: true,
            ..Default::default()
        }
    }

    fn reserved_layout(settings: &NodeCoreSettings, gpus: &[GpuResource]) -> CoreLayout {
        CoreLayout::compute(Some(&example_node()), settings, gpus, 0, &[])
            .unwrap()
            .unwrap()
    }

    #[test]
    fn reserved_cores_follow_numa_in_stable_id_order() {
        let mut gpus = gpus_on_numa(4);
        gpus.reverse();
        let layout = reserved_layout(&restricted(8), &gpus);
        assert_eq!(layout.gpu_reserved.len(), 8);
        assert_eq!(layout.gpu_reserved[0].stable_id, 0x100);
        let first: Vec<u32> = (0..8).chain(128..136).collect();
        assert_eq!(
            layout.gpu_reserved[0].cpus, first,
            "socket 0 cores 0-7 with their SMT siblings"
        );
        let fifth: Vec<u32> = (64..72).chain(192..200).collect();
        assert_eq!(layout.gpu_reserved[4].cpus, fifth, "first GPU of NUMA 1");
        assert_eq!(layout.general_cpus.len(), 256 - 8 * 16);
        assert!(layout.spec_cpus.is_empty());
    }

    #[test]
    fn gpu_affinity_cpus_are_its_cores_else_its_numa_nodes() {
        let topology = example_node();
        let mut gpu = gpus_on_numa(1).remove(1);
        let numa1: Vec<u32> = (64..128).chain(192..256).collect();
        assert_eq!(gpu_affinity_cpus(&topology, &gpu), numa1);

        gpu.affinity_cpus = vec![0, 3, 999];
        assert_eq!(
            gpu_affinity_cpus(&topology, &gpu),
            vec![0, 3, 128, 131],
            "configured CPUs widen to whole cores; offline ones are dropped"
        );

        gpu.affinity_cpus = vec![];
        gpu.numa_node = None;
        assert!(gpu_affinity_cpus(&topology, &gpu).is_empty());
        assert!(matches!(
            gpu_affinity_cores(&topology, &gpu),
            Err(LayoutError::GpuNoAffinity { .. })
        ));
    }

    fn sticky(
        settings: &NodeCoreSettings,
        gpus: &[GpuResource],
        previous: &[GpuReservedCpus],
    ) -> CoreLayout {
        CoreLayout::compute(Some(&example_node()), settings, gpus, 0, previous)
            .unwrap()
            .unwrap()
    }

    fn picks_of(layout: &CoreLayout, stable_id: u64) -> Vec<u32> {
        layout
            .gpu_reserved
            .iter()
            .find(|g| g.stable_id == stable_id)
            .map(|g| g.cpus.clone())
            .unwrap_or_default()
    }

    #[test]
    fn sticky_compute_with_no_previous_picks_matches_today() {
        // Same fixture as `reserved_cores_follow_numa_in_stable_id_order`;
        // pinned to its literal picks instead of comparing `compute()` to
        // itself, so a regression in the early-return path can't go unnoticed.
        let mut gpus = gpus_on_numa(4);
        gpus.reverse();
        let layout = sticky(&restricted(8), &gpus, &[]);
        assert_eq!(layout.gpu_reserved.len(), 8);
        assert_eq!(layout.gpu_reserved[0].stable_id, 0x100);
        let first: Vec<u32> = (0..8).chain(128..136).collect();
        assert_eq!(layout.gpu_reserved[0].cpus, first);
        let fifth: Vec<u32> = (64..72).chain(192..200).collect();
        assert_eq!(layout.gpu_reserved[4].cpus, fifth);
        assert_eq!(layout.general_cpus.len(), 256 - 8 * 16);
        assert!(layout.spec_cpus.is_empty());
    }

    #[test]
    fn sticky_compute_with_unrelated_previous_picks_matches_fresh() {
        // Spec section 6: a controller whose history has no entry for any
        // present GPU (e.g. a different node's stable ids) must compute
        // exactly the fresh layout, fingerprint included.
        let unrelated = [GpuReservedCpus {
            stable_id: 0xdead,
            cpus: vec![0, 128],
        }];
        for (r, per_numa, spec) in [
            (8, 4, None),
            (15, 4, Some(1)),
            (2, 1, Some(3)),
            (30, 2, None),
        ] {
            let settings = NodeCoreSettings {
                core_spec_count: spec,
                ..restricted(r)
            };
            let gpus = gpus_on_numa(per_numa);
            let fresh = reserved_layout(&settings, &gpus);
            let stuck = sticky(&settings, &gpus, &unrelated);
            assert_eq!(stuck, fresh);
            assert_eq!(stuck.fingerprint(), fresh.fingerprint());
        }
    }

    #[test]
    fn a_removed_gpu_and_a_new_gpu_recompute_together() {
        let gpus = gpus_on_numa(4);
        let before = reserved_layout(&restricted(8), &gpus);
        let mut kept: Vec<GpuResource> = gpus
            .iter()
            .filter(|g| g.stable_id != 0x100)
            .cloned()
            .collect();
        // The newcomer shares GPU 0x100's affinity (NUMA 0), so its lowest
        // free cores are exactly the ones 0x100's removal just freed.
        let mut newcomer = gpus[0].clone();
        newcomer.stable_id = 0x200;
        kept.push(newcomer.clone());
        let after = sticky(&restricted(8), &kept, &before.gpu_reserved);
        for g in kept.iter().filter(|g| g.stable_id != newcomer.stable_id) {
            assert_eq!(
                picks_of(&after, g.stable_id),
                picks_of(&before, g.stable_id),
                "every other GPU keeps its picks"
            );
        }
        let freed = picks_of(&before, 0x100);
        let newcomer_picks = picks_of(&after, newcomer.stable_id);
        assert_eq!(newcomer_picks.len(), 16, "8 cores with SMT siblings");
        assert_eq!(
            newcomer_picks, freed,
            "the new GPU takes the removed GPU's freed cores"
        );
    }

    #[test]
    fn recomputing_from_its_own_picks_is_a_fixed_point() {
        let gpus = gpus_on_numa(4);
        let first = reserved_layout(&restricted(8), &gpus);
        let again = sticky(&restricted(8), &gpus, &first.gpu_reserved);
        assert_eq!(again, first);
    }

    #[test]
    fn a_gpu_keeps_its_reserved_cores_when_a_lower_id_gpu_appears() {
        let mut gpus = gpus_on_numa(2);
        let newcomer = gpus.remove(0);
        let before = reserved_layout(&restricted(8), &gpus);
        let mut all = gpus.clone();
        all.push(newcomer.clone());
        let after = sticky(&restricted(8), &all, &before.gpu_reserved);
        for g in &gpus {
            assert_eq!(
                picks_of(&after, g.stable_id),
                picks_of(&before, g.stable_id)
            );
        }
        let newcomer_picks = picks_of(&after, newcomer.stable_id);
        assert_eq!(newcomer_picks.len(), 16, "8 cores with SMT siblings");
        let fresh = reserved_layout(&restricted(8), &all);
        assert_ne!(after, fresh, "the fresh rule would have moved the others");
    }

    #[test]
    fn a_removed_gpus_cores_go_only_to_short_gpus() {
        let gpus = gpus_on_numa(4);
        let before = reserved_layout(&restricted(8), &gpus);
        let kept: Vec<GpuResource> = gpus
            .iter()
            .filter(|g| g.stable_id != 0x100)
            .cloned()
            .collect();
        let after = sticky(&restricted(8), &kept, &before.gpu_reserved);
        for g in &kept {
            assert_eq!(
                picks_of(&after, g.stable_id),
                picks_of(&before, g.stable_id)
            );
        }
        let freed: Vec<u32> = picks_of(&before, 0x100);
        assert!(freed.iter().all(|c| after.general_cpus.contains(c)));
    }

    #[test]
    fn an_affinity_change_keeps_only_cores_still_near_the_gpu() {
        let mut gpus = gpus_on_numa(1);
        gpus[0].affinity_cpus = (0..8).collect();
        let before = reserved_layout(&restricted(4), &gpus);
        assert_eq!(
            picks_of(&before, 0x100),
            vec![0, 1, 2, 3, 128, 129, 130, 131]
        );
        gpus[0].affinity_cpus = (2..10).collect();
        let after = sticky(&restricted(4), &gpus, &before.gpu_reserved);
        assert_eq!(
            picks_of(&after, 0x100),
            vec![2, 3, 4, 5, 130, 131, 132, 133]
        );
    }

    #[test]
    fn spec_growing_into_a_gpus_picks_tops_it_up_from_the_lowest_free() {
        let mut gpus = gpus_on_numa(1);
        gpus[0].affinity_cpus = (0..8).collect();
        let before = reserved_layout(&restricted(4), &gpus);
        let settings = NodeCoreSettings {
            cpu_spec_list: Some("1".into()),
            ..restricted(4)
        };
        let after = sticky(&settings, &gpus, &before.gpu_reserved);
        assert_eq!(
            picks_of(&after, 0x100),
            vec![0, 2, 3, 4, 128, 130, 131, 132]
        );
    }

    #[test]
    fn a_cap_trim_moves_only_the_trimmed_pick() {
        let gpus = gpus_on_numa(1);
        // NUMA 1's GPU previously held cores 120-127; a cap of 248 CPUs trims
        // socket 1's top whole cores 124-127 (Slurm order), so it keeps
        // 120-123 and tops up with the lowest free cores, 64-67.
        let previous = [GpuReservedCpus {
            stable_id: 0x101,
            cpus: (120..128).chain(248..256).collect(),
        }];
        let settings = NodeCoreSettings {
            cpus_cap: 248,
            ..restricted(8)
        };
        let after = sticky(&settings, &gpus, &previous);
        let expected: Vec<u32> = (64..68)
            .chain(120..124)
            .chain(192..196)
            .chain(248..252)
            .collect();
        assert_eq!(picks_of(&after, 0x101), expected);
        assert_eq!(
            picks_of(&after, 0x100),
            picks_of(&reserved_layout(&settings, &gpus), 0x100)
        );
    }

    #[test]
    fn a_smaller_r_keeps_the_lowest_previous_picks() {
        let gpus = gpus_on_numa(2);
        let before = reserved_layout(&restricted(8), &gpus);
        let after = sticky(&restricted(4), &gpus, &before.gpu_reserved);
        for g in &gpus {
            let prev = picks_of(&before, g.stable_id);
            let now = picks_of(&after, g.stable_id);
            assert_eq!(now.len(), 8);
            assert!(
                now.iter().all(|c| prev.contains(c)),
                "only previous picks kept"
            );
        }
    }

    #[test]
    fn a_larger_r_keeps_previous_picks_and_tops_up() {
        let gpus = gpus_on_numa(2);
        let before = reserved_layout(&restricted(4), &gpus);
        let after = sticky(&restricted(8), &gpus, &before.gpu_reserved);
        for g in &gpus {
            let prev = picks_of(&before, g.stable_id);
            let now = picks_of(&after, g.stable_id);
            assert_eq!(now.len(), 16);
            assert!(prev.iter().all(|c| now.contains(c)));
        }
    }

    #[test]
    fn a_sticky_fill_that_cannot_fit_falls_back_to_the_fresh_rule() {
        // A (near cores 0-1) kept core 1; B is near core 1 only, so a sticky
        // fill leaves B nothing, while the fresh rule gives A core 0, B core 1.
        let mut gpus = gpus_on_numa(1);
        gpus.truncate(1);
        let mut b = gpus[0].clone();
        b.stable_id = 0x200;
        gpus[0].affinity_cpus = vec![0, 1];
        b.affinity_cpus = vec![1];
        gpus.push(b);
        let previous = [GpuReservedCpus {
            stable_id: 0x100,
            cpus: vec![1, 129],
        }];
        let after = sticky(&restricted(1), &gpus, &previous);
        assert_eq!(after, reserved_layout(&restricted(1), &gpus));
        assert_eq!(picks_of(&after, 0x200), vec![1, 129]);
    }

    #[test]
    fn kept_picks_follow_their_gpu_whatever_the_input_order() {
        let gpus = gpus_on_numa(2);
        // The fresh rule gives 0x100 cores 0-3; these picks swap the two NUMA-0 GPUs.
        let previous = [
            GpuReservedCpus {
                stable_id: 0x101,
                cpus: (0..4).chain(128..132).collect(),
            },
            GpuReservedCpus {
                stable_id: 0x100,
                cpus: (4..8).chain(132..136).collect(),
            },
        ];
        let forward = sticky(&restricted(4), &gpus, &previous);
        let mut reversed = gpus.clone();
        reversed.reverse();
        assert_eq!(sticky(&restricted(4), &reversed, &previous), forward);
        assert_eq!(
            picks_of(&forward, 0x100),
            (4..8).chain(132..136).collect::<Vec<u32>>()
        );
        assert_eq!(
            picks_of(&forward, 0x101),
            (0..4).chain(128..132).collect::<Vec<u32>>()
        );
    }

    #[test]
    fn a_short_lower_gpu_is_reported_before_a_later_gpu_without_affinity() {
        let mut gpus = gpus_on_numa(1);
        gpus[1].numa_node = None;
        let err =
            CoreLayout::compute(Some(&example_node()), &restricted(65), &gpus, 0, &[]).unwrap_err();
        assert_eq!(
            err,
            LayoutError::TooFewCores {
                stable_id: 0x100,
                wanted: 65,
                available: 64,
            }
        );
    }

    #[test]
    fn an_offline_thread_keeps_its_core_when_a_sibling_is_online() {
        let mut gpus = gpus_on_numa(1);
        gpus[0].affinity_cpus = (0..8).collect();
        let before = reserved_layout(&restricted(4), &gpus);
        let mut topology = example_node();
        topology.cores[2].cpus = vec![2];
        let after = CoreLayout::compute(
            Some(&topology),
            &restricted(4),
            &gpus,
            0,
            &before.gpu_reserved,
        )
        .unwrap()
        .unwrap();
        assert_eq!(picks_of(&after, 0x100), vec![0, 1, 2, 3, 128, 129, 131]);
    }

    #[test]
    fn reserved_cores_skip_spec_cores_and_earlier_gpus() {
        let settings = NodeCoreSettings {
            core_spec_count: Some(1),
            ..restricted(15)
        };
        let layout = reserved_layout(&settings, &gpus_on_numa(4));
        let reserved: Vec<u32> = layout
            .gpu_reserved
            .iter()
            .flat_map(|g| g.cpus.clone())
            .collect();
        assert!(!reserved.contains(&127) && !reserved.contains(&255));
        let unique: BTreeSet<u32> = reserved.iter().copied().collect();
        assert_eq!(unique.len(), reserved.len(), "no core is given to two GPUs");
        assert_eq!(layout.spec_cpus, vec![127, 255]);
    }

    #[test]
    fn configured_affinity_beats_the_numa_node() {
        let mut gpus = gpus_on_numa(1);
        gpus[0].affinity_cpus = vec![32, 33, 34, 35];
        let layout = reserved_layout(&restricted(2), &gpus);
        assert_eq!(layout.gpu_reserved[0].cpus, vec![32, 33, 160, 161]);
    }

    #[test]
    fn a_cores_cpu_beyond_the_nodes_topology_is_dropped_but_the_rest_kept() {
        let mut gpus = gpus_on_numa(1);
        gpus[0].affinity_cpus = vec![32, 33, 9999];
        let layout = reserved_layout(&restricted(2), &gpus);
        assert_eq!(layout.gpu_reserved[0].cpus, vec![32, 33, 160, 161]);
    }

    #[test]
    fn a_gpu_without_affinity_is_a_layout_error() {
        let mut gpus = gpus_on_numa(1);
        gpus[0].numa_node = None;
        assert_eq!(
            CoreLayout::compute(Some(&example_node()), &restricted(2), &gpus, 0, &[]),
            Err(LayoutError::GpuNoAffinity { stable_id: 0x100 })
        );
    }

    #[test]
    fn a_cpu_less_numa_node_is_a_layout_error() {
        let mut gpus = gpus_on_numa(1);
        gpus[0].numa_node = Some(7);
        assert_eq!(
            CoreLayout::compute(Some(&example_node()), &restricted(2), &gpus, 0, &[]),
            Err(LayoutError::GpuNoAffinity { stable_id: 0x100 })
        );
    }

    #[test]
    fn too_few_unique_cores_is_a_layout_error() {
        // Five GPUs on NUMA 0 at 16 cores each need 80 of its 64 cores.
        let gpus: Vec<GpuResource> = gpus_on_numa(5).into_iter().take(5).collect();
        assert_eq!(
            CoreLayout::compute(Some(&example_node()), &restricted(16), &gpus, 0, &[]),
            Err(LayoutError::TooFewCores {
                stable_id: 0x104,
                wanted: 16,
                available: 0
            })
        );
    }

    #[test]
    fn restricted_cores_without_topology_is_a_layout_error() {
        assert_eq!(
            CoreLayout::compute(None, &restricted(2), &gpus_on_numa(1), 0, &[]),
            Err(LayoutError::MissingTopology)
        );
    }

    #[test]
    fn restricted_cores_on_a_node_without_gpus_leave_everything_general() {
        let layout = reserved_layout(&restricted(8), &[]);
        assert!(layout.gpu_reserved.is_empty());
        assert_eq!(layout.general_cpus.len(), 256);
    }

    #[test]
    fn no_settings_still_means_no_layout() {
        assert_eq!(
            CoreLayout::compute(
                Some(&example_node()),
                &NodeCoreSettings::default(),
                &gpus_on_numa(4),
                0,
                &[]
            ),
            Ok(None)
        );
    }

    #[test]
    fn zero_restricted_cores_means_no_layout() {
        assert_eq!(
            CoreLayout::compute(
                Some(&example_node()),
                &restricted(0),
                &gpus_on_numa(4),
                0,
                &[]
            ),
            Ok(None)
        );
    }

    #[test]
    fn reserved_cores_change_the_fingerprint() {
        let eight = reserved_layout(&restricted(8), &gpus_on_numa(4));
        let four = reserved_layout(&restricted(4), &gpus_on_numa(4));
        assert_ne!(eight.fingerprint(), four.fingerprint());
    }

    fn capped(cap: u32) -> NodeCoreSettings {
        NodeCoreSettings {
            cpus_cap: cap,
            ..restricted(8)
        }
    }

    fn assert_cap_invariants(layout: &CoreLayout, cap: u32) {
        let reserved: BTreeSet<u32> = layout
            .gpu_reserved
            .iter()
            .flat_map(|g| g.cpus.iter().copied())
            .collect();
        let general: BTreeSet<u32> = layout.general_cpus.iter().copied().collect();
        let trimmed: BTreeSet<u32> = layout.trimmed_cpus.iter().copied().collect();
        let spec: BTreeSet<u32> = layout.spec_cpus.iter().copied().collect();
        assert!(trimmed.is_disjoint(&reserved) && trimmed.is_disjoint(&general));
        assert!(trimmed.is_disjoint(&spec));
        assert!(reserved.is_disjoint(&general));
        assert_eq!(reserved.len() + general.len(), cap as usize);
    }

    #[test]
    fn a_cpus_cap_keeps_the_lowest_cores_socket_major_like_slurm() {
        // 256 CPUs, 8 GPUs x 16 reserved = 128; a cap of 200 leaves 72 general.
        let layout = reserved_layout(&capped(200), &gpus_on_numa(4));
        let uncapped = reserved_layout(&restricted(8), &gpus_on_numa(4));
        assert_eq!(layout.gpu_reserved, uncapped.gpu_reserved);
        assert_eq!(layout.general_cpus.len(), 72);
        // The last socket's highest cores go first: socket 1 cores 36-63, with
        // their SMT siblings.
        let expected: Vec<u32> = (100..128).chain(228..256).collect();
        assert_eq!(layout.trimmed_cpus, expected);
        assert_cap_invariants(&layout, 200);
        let pools = layout.pools().unwrap();
        assert_eq!(pools.general + pools.reserved_total, 200);
    }

    #[test]
    fn an_odd_cpus_cap_rounds_down_to_whole_cores() {
        let layout = reserved_layout(&capped(201), &gpus_on_numa(4));
        assert_eq!(layout.general_cpus.len(), 72);
        let expected: Vec<u32> = (100..128).chain(228..256).collect();
        assert_eq!(layout.trimmed_cpus, expected);
        assert_cap_invariants(&layout, 200);
    }

    #[test]
    fn a_cpus_cap_at_or_above_the_usable_cpus_trims_nothing() {
        let uncapped = reserved_layout(&restricted(8), &gpus_on_numa(4));
        for cap in [256, 1000] {
            let layout = reserved_layout(&capped(cap), &gpus_on_numa(4));
            assert!(layout.trimmed_cpus.is_empty());
            assert_eq!(layout, uncapped);
            assert_eq!(layout.fingerprint(), uncapped.fingerprint());
        }
    }

    #[test]
    fn a_cpus_cap_counts_after_spec_cores() {
        let settings = NodeCoreSettings {
            core_spec_count: Some(1),
            ..capped(200)
        };
        let layout = reserved_layout(&settings, &gpus_on_numa(4));
        assert_eq!(layout.spec_cpus, vec![127, 255]);
        assert_eq!(layout.general_cpus.len(), 72);
        let expected: Vec<u32> = (100..127).chain(228..255).collect();
        assert_eq!(
            layout.trimmed_cpus, expected,
            "the spec core is not trimmed"
        );
        assert_cap_invariants(&layout, 200);
    }

    #[test]
    fn a_cpus_cap_equal_to_the_reserved_cpus_leaves_no_general_pool() {
        let mut gpus = gpus_on_numa(4);
        for gpu in &mut gpus {
            gpu.numa_node = Some(0);
        }
        let layout = reserved_layout(&capped(128), &gpus);
        assert!(layout.general_cpus.is_empty());
        let socket_1: Vec<u32> = (64..128).chain(192..256).collect();
        assert_eq!(layout.trimmed_cpus, socket_1);
        assert_cap_invariants(&layout, 128);
    }

    #[test]
    fn a_cpus_cap_too_small_for_the_reserved_cores_names_the_gpu_and_cap() {
        // 33 cores trimmed, all from socket 1: NUMA 1 keeps 31 cores, one short
        // of its four GPUs' 32, though 190 CPUs exceed the 128 reserved.
        let err = CoreLayout::compute(
            Some(&example_node()),
            &capped(191),
            &gpus_on_numa(4),
            0,
            &[],
        )
        .unwrap_err();
        assert_eq!(
            err,
            LayoutError::TooFewCoresWithinCap {
                stable_id: 0x107,
                wanted: 8,
                available: 7,
                cap: 191
            }
        );
        assert!(err.to_string().contains("within cpus = 191"), "{err}");
    }

    #[test]
    fn reserved_cores_are_picked_only_from_cpus_the_cap_keeps() {
        // The GPU's configured cores are the highest of socket 1, which a cap
        // of 200 trims.
        let mut gpus = gpus_on_numa(1);
        gpus[0].affinity_cpus = vec![126, 127];
        assert_eq!(
            CoreLayout::compute(Some(&example_node()), &capped(200), &gpus, 0, &[]),
            Err(LayoutError::TooFewCoresWithinCap {
                stable_id: 0x100,
                wanted: 8,
                available: 0,
                cap: 200
            })
        );
        gpus[0].affinity_cpus = (98..128).collect();
        let layout = reserved_layout(
            &NodeCoreSettings {
                cpus_cap: 200,
                ..restricted(2)
            },
            &gpus,
        );
        assert_eq!(layout.gpu_reserved[0].cpus, vec![98, 99, 226, 227]);
        assert_cap_invariants(&layout, 200);
    }

    #[test]
    fn a_cpus_cap_without_reserved_cores_leaves_the_layout_alone() {
        let settings = NodeCoreSettings {
            cpus_cap: 100,
            ..spec_count(1)
        };
        let layout = CoreLayout::compute(Some(&example_node()), &settings, &[], 0, &[])
            .unwrap()
            .unwrap();
        assert!(layout.trimmed_cpus.is_empty());
        assert_eq!(layout.general_cpus.len(), 254);
    }

    #[test]
    fn reserved_share_is_capped_by_both_sides() {
        assert_eq!(reserved_share(4, 16), 4);
        assert_eq!(reserved_share(40, 16), 16);
        assert_eq!(reserved_share(8, 0), 0);
    }

    #[test]
    fn reserved_for_counts_only_the_named_gpus() {
        let layout = reserved_layout(&restricted(8), &gpus_on_numa(4));
        assert_eq!(layout.reserved_for(&[0x100, 0x104]), 32);
        assert_eq!(layout.reserved_for(&[0xdead]), 0);
    }

    fn pools(general: u32, per_gpu: u32, gpus: u32) -> CpuPools {
        CpuPools {
            general,
            reserved_per_gpu: per_gpu,
            reserved_total: per_gpu * gpus,
        }
    }

    #[test]
    fn pools_are_absent_without_per_gpu_reservations() {
        let layout = CoreLayout::compute(Some(&example_node()), &spec_count(1), &[], 0, &[])
            .unwrap()
            .unwrap();
        assert_eq!(layout.pools(), None);
    }

    #[test]
    fn pools_count_general_and_reserved_cpus() {
        let layout = reserved_layout(&restricted(8), &gpus_on_numa(4));
        assert_eq!(layout.pools(), Some(pools(128, 16, 8)));
    }

    #[test]
    fn cpu_only_jobs_fit_the_general_pool_only() {
        let used = ResourceAllocations::with_scalar(6, 0);
        assert!(pools(8, 4, 4).admits(&used, 2, 0));
        assert!(
            !pools(8, 4, 4).admits(&used, 3, 0),
            "reserved CPUs are not general"
        );
    }

    #[test]
    fn gpu_jobs_need_only_what_their_reserved_cores_do_not_cover() {
        let used = ResourceAllocations::with_scalar(8, 0);
        assert!(
            pools(8, 4, 4).admits(&used, 8, 2),
            "fully covered by 2 GPUs"
        );
        assert!(
            !pools(8, 4, 4).admits(&used, 9, 2),
            "one over needs a general CPU"
        );
    }

    #[test]
    fn reserved_cpus_in_use_count_only_up_to_the_layout() {
        let mut used = ResourceAllocations::with_scalar(8, 0);
        used.reserved_cpus = 9;
        assert!(
            !pools(4, 2, 2).admits(&used, 1, 0),
            "4 of the 8 are general"
        );
        used.reserved_cpus = 4;
        assert!(!pools(4, 2, 2).admits(&used, 1, 0));
        used.cpus = 7;
        assert!(pools(4, 2, 2).admits(&used, 1, 0));
    }

    #[test]
    fn stamp_records_the_share_of_the_chosen_gpus_and_the_layout() {
        let layout = reserved_layout(&restricted(8), &gpus_on_numa(4));
        let mut alloc = ResourceAllocations::with_scalar(20, 0);
        alloc.devices.insert(
            "gpu".into(),
            vec![AllocatedDevice {
                device_id: 0x100,
                count: 1,
            }],
        );
        layout.stamp(&mut alloc);
        assert_eq!(alloc.reserved_cpus, 16);
        assert_eq!(alloc.layout_fingerprint, layout.fingerprint());
        alloc.cpus = 4;
        layout.stamp(&mut alloc);
        assert_eq!(alloc.reserved_cpus, 4, "never more than the job's CPUs");
    }
}
