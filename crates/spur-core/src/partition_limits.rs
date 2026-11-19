// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Per-partition resource limits as one unit (for replication and the API), and the
//! per-job memory policy a partition applies to a job's request.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::gpu_request::{resolve_gpu_demand, tasks_per_node_counts, GpuDemand};
use crate::job::JobSpec;
use crate::partition::{is_valid_partition_tres_key, Partition, TRES_CPU};
use crate::wal::clear_or_set;

/// A partition's resource caps and memory policy, replicated together so a
/// reconfigure or API update can never leave half of them applied.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionResourceLimits {
    #[serde(default)]
    pub max_tres_per_node: BTreeMap<String, u64>,
    #[serde(default)]
    pub max_mem_per_node: Option<u64>,
    #[serde(default)]
    pub max_mem_per_cpu: Option<u64>,
    #[serde(default)]
    pub def_mem_per_node: Option<u64>,
    #[serde(default)]
    pub def_mem_per_cpu: Option<u64>,
    #[serde(default)]
    pub def_mem_per_gpu: Option<u64>,
    #[serde(default)]
    pub def_cpu_per_gpu: Option<u32>,
}

impl PartitionResourceLimits {
    pub fn of(partition: &Partition) -> Self {
        Self {
            max_tres_per_node: partition.max_tres_per_node.clone(),
            max_mem_per_node: partition.max_mem_per_node,
            max_mem_per_cpu: partition.max_mem_per_cpu,
            def_mem_per_node: partition.def_mem_per_node,
            def_mem_per_cpu: partition.def_mem_per_cpu,
            def_mem_per_gpu: partition.def_mem_per_gpu,
            def_cpu_per_gpu: partition.def_cpu_per_gpu,
        }
    }

    pub fn apply_to(&self, partition: &mut Partition) {
        partition.max_tres_per_node = self.max_tres_per_node.clone();
        partition.max_mem_per_node = self.max_mem_per_node;
        partition.max_mem_per_cpu = self.max_mem_per_cpu;
        partition.def_mem_per_node = self.def_mem_per_node;
        partition.def_mem_per_cpu = self.def_mem_per_cpu;
        partition.def_mem_per_gpu = self.def_mem_per_gpu;
        partition.def_cpu_per_gpu = self.def_cpu_per_gpu;
    }
}

/// `max_tres_per_node` with the `max_cpus_per_node` alias folded into `cpu`. Setting
/// both is an error: an operator could not see which spelling wins.
pub fn fold_max_cpus_per_node(
    mut max_tres_per_node: BTreeMap<String, u64>,
    max_cpus_per_node: Option<u32>,
) -> Result<BTreeMap<String, u64>, String> {
    let Some(cpus) = max_cpus_per_node else {
        return Ok(max_tres_per_node);
    };
    if let Some(cpu) = max_tres_per_node.get(TRES_CPU) {
        return Err(format!(
            "max_cpus_per_node = {cpus} conflicts with max_tres_per_node.cpu = {cpu} (set only one)"
        ));
    }
    max_tres_per_node.insert(TRES_CPU.to_string(), u64::from(cpus));
    Ok(max_tres_per_node)
}

/// A change to a partition's caps and memory policy, merged into its limits when applied
/// so updates to different knobs never overwrite each other. `None` fields are unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionLimitsUpdate {
    /// Replaces the whole cap map.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tres_per_node: Option<BTreeMap<String, u64>>,
    /// Sets (`Some`) or clears (`None`) only the `cpu` cap.
    #[serde(
        default,
        deserialize_with = "clear_or_set",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_cpus_per_node: Option<Option<u32>>,
    #[serde(
        default,
        deserialize_with = "clear_or_set",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_mem_per_node: Option<Option<u64>>,
    #[serde(
        default,
        deserialize_with = "clear_or_set",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_mem_per_cpu: Option<Option<u64>>,
    #[serde(
        default,
        deserialize_with = "clear_or_set",
        skip_serializing_if = "Option::is_none"
    )]
    pub def_mem_per_node: Option<Option<u64>>,
    #[serde(
        default,
        deserialize_with = "clear_or_set",
        skip_serializing_if = "Option::is_none"
    )]
    pub def_mem_per_cpu: Option<Option<u64>>,
    #[serde(
        default,
        deserialize_with = "clear_or_set",
        skip_serializing_if = "Option::is_none"
    )]
    pub def_mem_per_gpu: Option<Option<u64>>,
    /// Not in the `def_mem_*` group: it sizes CPUs and is set independently.
    #[serde(
        default,
        deserialize_with = "clear_or_set",
        skip_serializing_if = "Option::is_none"
    )]
    pub def_cpu_per_gpu: Option<Option<u32>>,
}

impl PartitionLimitsUpdate {
    /// An update that sets every cap and knob to `limits`.
    pub fn replacing(limits: &PartitionResourceLimits) -> Self {
        Self {
            max_tres_per_node: Some(limits.max_tres_per_node.clone()),
            max_cpus_per_node: None,
            max_mem_per_node: Some(limits.max_mem_per_node),
            max_mem_per_cpu: Some(limits.max_mem_per_cpu),
            def_mem_per_node: Some(limits.def_mem_per_node),
            def_mem_per_cpu: Some(limits.def_mem_per_cpu),
            def_mem_per_gpu: Some(limits.def_mem_per_gpu),
            def_cpu_per_gpu: Some(limits.def_cpu_per_gpu),
        }
    }

    /// Rejects what the request alone gets wrong: unknown cap keys, both spellings of the
    /// CPU cap, or two knobs of one memory group set together.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(map) = &self.max_tres_per_node {
            if let Some(key) = map.keys().find(|key| !is_valid_partition_tres_key(key)) {
                return Err(format!(
                    "max_tres_per_node: unknown key {key:?} (expected cpu, mem, gres/gpu, or gres/gpu:<type>)"
                ));
            }
            if let (Some(cpus), Some(cpu)) = (self.max_cpus_per_node, map.get(TRES_CPU)) {
                let alias = cpus.map_or("cleared".to_string(), |c| c.to_string());
                return Err(format!(
                    "max_cpus_per_node ({alias}) conflicts with max_tres_per_node.cpu = {cpu} (set only one)"
                ));
            }
        }
        let groups = [
            vec![
                ("max_mem_per_node", self.max_mem_per_node),
                ("max_mem_per_cpu", self.max_mem_per_cpu),
            ],
            vec![
                ("def_mem_per_node", self.def_mem_per_node),
                ("def_mem_per_cpu", self.def_mem_per_cpu),
                ("def_mem_per_gpu", self.def_mem_per_gpu),
            ],
        ];
        for group in groups {
            let set: Vec<String> = group
                .into_iter()
                .filter_map(|(name, change)| change.flatten().map(|v| format!("{name} = {v}")))
                .collect();
            if set.len() > 1 {
                return Err(format!("{} (set at most one)", set.join(", ")));
            }
        }
        Ok(())
    }

    /// The limits `current` ends up with; `None` when nothing changes. Setting one
    /// knob of a memory group clears the others, as in Slurm, so a merge of a
    /// [`validate`](Self::validate)d update into valid limits is always valid.
    pub fn merged(&self, current: &Partition) -> Option<PartitionResourceLimits> {
        if *self == Self::default() {
            return None;
        }
        let mut limits = PartitionResourceLimits::of(current);
        if let Some(map) = &self.max_tres_per_node {
            limits.max_tres_per_node = map.clone();
        }
        match self.max_cpus_per_node {
            Some(Some(cpus)) => {
                limits
                    .max_tres_per_node
                    .insert(TRES_CPU.to_string(), u64::from(cpus));
            }
            Some(None) => {
                limits.max_tres_per_node.remove(TRES_CPU);
            }
            None => {}
        }
        set_one_of(
            &mut [&mut limits.max_mem_per_node, &mut limits.max_mem_per_cpu],
            [self.max_mem_per_node, self.max_mem_per_cpu],
        );
        set_one_of(
            &mut [
                &mut limits.def_mem_per_node,
                &mut limits.def_mem_per_cpu,
                &mut limits.def_mem_per_gpu,
            ],
            [
                self.def_mem_per_node,
                self.def_mem_per_cpu,
                self.def_mem_per_gpu,
            ],
        );
        if let Some(change) = self.def_cpu_per_gpu {
            limits.def_cpu_per_gpu = change;
        }
        Some(limits)
    }
}

/// Apply changes to mutually exclusive knobs: a knob set to a value clears the others.
fn set_one_of<const N: usize>(
    fields: &mut [&mut Option<u64>; N],
    changes: [Option<Option<u64>>; N],
) {
    for (i, change) in changes.iter().enumerate() {
        let Some(value) = change else { continue };
        *fields[i] = *value;
        if value.is_some() {
            for (j, field) in fields.iter_mut().enumerate() {
                if j != i {
                    **field = None;
                }
            }
        }
    }
}

/// The request fields a partition's memory policy may change. Recorded on the
/// job when it starts so `scontrol show job` reports what it actually got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryAdjustment {
    pub cpus_per_task: u32,
    #[serde(default)]
    pub memory_per_node_mb: Option<u64>,
    #[serde(default)]
    pub memory_per_cpu_mb: Option<u64>,
    #[serde(default)]
    pub memory_per_gpu_mb: Option<u64>,
    /// Absent from adjustments recorded before it existed, which never changed it,
    /// so `None` leaves the spec's value alone.
    #[serde(default)]
    pub cpus_per_gpu: Option<u32>,
}

impl MemoryAdjustment {
    pub fn of(spec: &JobSpec) -> Self {
        Self {
            cpus_per_task: spec.cpus_per_task,
            memory_per_node_mb: spec.memory_per_node_mb,
            memory_per_cpu_mb: spec.memory_per_cpu_mb,
            memory_per_gpu_mb: spec.memory_per_gpu_mb,
            cpus_per_gpu: spec.cpus_per_gpu,
        }
    }

    pub fn apply_to(&self, spec: &mut JobSpec) {
        spec.cpus_per_task = self.cpus_per_task;
        spec.memory_per_node_mb = self.memory_per_node_mb;
        spec.memory_per_cpu_mb = self.memory_per_cpu_mb;
        spec.memory_per_gpu_mb = self.memory_per_gpu_mb;
        if self.cpus_per_gpu.is_some() {
            spec.cpus_per_gpu = self.cpus_per_gpu;
        }
    }
}

/// A request a job could run with, and the candidate partitions whose memory
/// policy reshapes it into exactly that request.
#[derive(Debug, Clone)]
pub struct CandidateRequest<'a> {
    pub spec: JobSpec,
    pub partitions: Vec<&'a str>,
}

/// The distinct requests the job would run with in `candidates`, in candidate order;
/// with no candidates, just the job's own request.
pub fn candidate_requests<'a>(
    spec: &JobSpec,
    candidates: impl IntoIterator<Item = &'a Partition>,
) -> Vec<CandidateRequest<'a>> {
    let mut requests: Vec<(MemoryAdjustment, Vec<&'a str>)> = Vec::new();
    for partition in candidates {
        let adjusted = partition.memory_adjusted(spec);
        match requests.iter_mut().find(|(known, _)| *known == adjusted) {
            Some((_, names)) => names.push(&partition.name),
            None => requests.push((adjusted, vec![&partition.name])),
        }
    }
    if requests.is_empty() {
        requests.push((MemoryAdjustment::of(spec), Vec::new()));
    }
    requests
        .into_iter()
        .map(|(adjustment, partitions)| {
            let mut spec = spec.clone();
            adjustment.apply_to(&mut spec);
            CandidateRequest { spec, partitions }
        })
        .collect()
}

/// Tasks on the node holding the fewest of them (at least one).
fn min_tasks_per_node(spec: &JobSpec) -> u32 {
    spec.tasks_per_node
        .unwrap_or(spec.num_tasks / spec.num_nodes.max(1))
        .max(1)
}

/// CPUs on the node holding the smallest share, floored at one.
fn min_node_cpus(spec: &JobSpec) -> u64 {
    u64::from(node_cpu_counts(spec).into_iter().min().unwrap_or(1))
}

/// CPUs the job uses on each node, in task-layout order (e.g. `-N2 -n7` ->
/// `[4, 3]`), each floored at one. A `--gpus` total is assumed spread evenly,
/// so for a `--cpus-per-gpu` job this is an estimate; placement must size each
/// node from the GPUs it actually gives it.
pub fn node_cpu_counts(spec: &JobSpec) -> Vec<u32> {
    let gpus = match spec.cpus_per_gpu {
        Some(_) => crate::job::gpus_per_node_counts(spec),
        None => Vec::new(),
    };
    tasks_per_node_counts(spec, spec.num_nodes.max(1))
        .into_iter()
        .enumerate()
        .map(|(i, tasks)| {
            let gpus_here = gpus.get(i).copied().unwrap_or(0);
            spec.cpus_on_node(tasks, gpus_here).max(1)
        })
        .collect()
}

/// Memory (MiB) the job asks for on one node holding `cpus` of its CPUs and
/// `gpus` of its GPUs.
pub fn node_memory_mb(spec: &JobSpec, cpus: u64, gpus: u64) -> u64 {
    spec.memory_per_node_mb
        .or_else(|| spec.memory_per_cpu_mb.map(|m| m.saturating_mul(cpus)))
        .or_else(|| spec.memory_per_gpu_mb.map(|m| m.saturating_mul(gpus)))
        .unwrap_or(0)
}

/// Largest per-node GPU count at the most even spread of a GPU total: this spread
/// always exists and no split puts fewer on its fullest node.
pub fn even_node_gpus(spec: &JobSpec) -> u64 {
    match resolve_gpu_demand(spec) {
        Ok(GpuDemand::PerNode { counts, .. }) => u64::from(counts.into_iter().max().unwrap_or(0)),
        Ok(GpuDemand::Total { count, .. }) => {
            u64::from(count).div_ceil(u64::from(spec.num_nodes.max(1)))
        }
        Ok(GpuDemand::None) | Err(_) => 0,
    }
}

/// Memory (MiB) on the node needing the most, with CPUs at their actual
/// per-node shares and GPUs at their most even spread.
fn peak_node_memory_mb(spec: &JobSpec) -> u64 {
    let cpus = node_cpu_counts(spec).into_iter().max().unwrap_or(1);
    node_memory_mb(spec, u64::from(cpus), even_node_gpus(spec))
}

impl Partition {
    /// Whether this partition's memory policy can change a job's request.
    pub fn has_memory_policy(&self) -> bool {
        self.max_mem_per_cpu.is_some()
            || self.def_mem_per_node.is_some()
            || self.def_mem_per_cpu.is_some()
            || self.def_mem_per_gpu.is_some()
    }

    /// The request as this partition runs it: `def_mem_*` if no memory is named, then
    /// Slurm's `max_mem_per_cpu` rule (more CPUs, not rejection). Idempotent per partition.
    pub fn memory_adjusted(&self, spec: &JobSpec) -> MemoryAdjustment {
        let mut adjusted = MemoryAdjustment::of(spec);
        // Spur has no whole-node `--mem=0`: the API already reads 0 as "no
        // memory request", so the policy must too.
        adjusted.memory_per_node_mb = adjusted.memory_per_node_mb.filter(|m| *m > 0);
        adjusted.memory_per_cpu_mb = adjusted.memory_per_cpu_mb.filter(|m| *m > 0);
        adjusted.memory_per_gpu_mb = adjusted.memory_per_gpu_mb.filter(|m| *m > 0);
        let named = [
            adjusted.memory_per_node_mb,
            adjusted.memory_per_cpu_mb,
            adjusted.memory_per_gpu_mb,
        ];
        if named.iter().all(Option::is_none) {
            self.apply_default_memory(spec, &mut adjusted);
        }
        if let Some(limit) = self.max_mem_per_cpu.filter(|l| *l > 0) {
            raise_cpus_for_memory(spec, limit, &mut adjusted);
        }
        adjusted
    }

    fn apply_default_memory(&self, spec: &JobSpec, adjusted: &mut MemoryAdjustment) {
        if let Some(per_node) = self.def_mem_per_node {
            adjusted.memory_per_node_mb = Some(per_node);
        } else if let Some(per_cpu) = self.def_mem_per_cpu {
            adjusted.memory_per_cpu_mb = Some(per_cpu);
        } else if let Some(per_gpu) = self.def_mem_per_gpu {
            if even_node_gpus(spec) > 0 {
                adjusted.memory_per_gpu_mb = Some(per_gpu);
            }
        }
    }

    /// `(requested, limit)` when the job's per-node memory, as this partition
    /// would run it, exceeds `max_mem_per_node`.
    pub fn max_mem_per_node_exceeded(&self, spec: &JobSpec) -> Option<(u64, u64)> {
        let limit = self.max_mem_per_node?;
        let mut adjusted = spec.clone();
        self.memory_adjusted(spec).apply_to(&mut adjusted);
        let requested = peak_node_memory_mb(&adjusted);
        (requested > limit).then_some((requested, limit))
    }

    /// Whether one node's share (`memory_mb` over `cpus` CPUs) stays within the per-job
    /// memory limits; binds only where shares differ per node, as with uneven GPUs.
    pub fn node_memory_allowed(&self, cpus: u64, memory_mb: u64) -> bool {
        let per_node_ok = self.max_mem_per_node.is_none_or(|limit| memory_mb <= limit);
        let per_cpu_ok = self
            .max_mem_per_cpu
            .filter(|limit| *limit > 0)
            .is_none_or(|limit| memory_mb <= limit.saturating_mul(cpus));
        per_node_ok && per_cpu_ok
    }
}

/// Slurm `_valid_pn_min_mem`: over-limit `--mem-per-cpu` scales CPUs up and memory per CPU
/// down by `ceil(mem / limit)`; over-limit `--mem` (or per-GPU, evenly spread) raises CPUs.
/// Unlike Slurm, the per-CPU split scales `--cpus-per-gpu` too, so the job keeps its memory.
///
/// A `--cpus-per-gpu` job never gets `cpus_per_task` raised, by any of these branches:
/// Slurm's own rule has no branch for a per-GPU CPU count at all (job_mgr.c
/// `_valid_pn_min_mem`), so there is no parity target to extend, and raising
/// `cpus_per_task` here would leave the job with both fields set, which submit
/// validation forbids (though nothing re-validates this dispatch-time adjustment).
/// With `--mem-per-gpu` it scales `cpus_per_gpu` directly from the job's own per-GPU
/// memory. With per-node `--mem` instead, the per-node branch below scales
/// `cpus_per_gpu` from the per-node need and the fewest GPUs any one node holds —
/// that node needs the highest per-GPU CPU count to reach the same per-node total.
fn raise_cpus_for_memory(spec: &JobSpec, limit: u64, adjusted: &mut MemoryAdjustment) {
    if let (Some(per_gpu_mem), Some(cpg)) = (adjusted.memory_per_gpu_mb, adjusted.cpus_per_gpu) {
        let need = per_gpu_mem.div_ceil(limit);
        if need > u64::from(cpg) {
            adjusted.cpus_per_gpu = Some(u32::try_from(need).unwrap_or(u32::MAX));
        }
        return;
    }
    let mut probe = spec.clone();
    adjusted.apply_to(&mut probe);
    let per_node = adjusted.memory_per_node_mb.or_else(|| {
        adjusted
            .memory_per_gpu_mb
            .map(|m| m.saturating_mul(even_node_gpus(&probe)))
    });
    if let Some(per_node) = per_node.filter(|m| *m > 0) {
        let needed = per_node.div_ceil(limit);
        if needed <= min_node_cpus(&probe) {
            return;
        }
        if let Some(cpg) = adjusted.cpus_per_gpu {
            // A node with no GPUs of its own has nothing sound to scale cpus_per_gpu
            // from; leave the job unchanged rather than guess at a value for it.
            let Some(fewest) = crate::job::gpus_per_node_counts(&probe)
                .into_iter()
                .min()
                .filter(|&gpus| gpus > 0)
            else {
                return;
            };
            let fewest = u64::from(fewest);
            if needed > u64::from(cpg) * fewest {
                adjusted.cpus_per_gpu =
                    Some(u32::try_from(needed.div_ceil(fewest)).unwrap_or(u32::MAX));
            }
            return;
        }
        let per_task = needed.div_ceil(u64::from(min_tasks_per_node(&probe)));
        adjusted.cpus_per_task = u32::try_from(per_task).unwrap_or(u32::MAX);
        return;
    }
    let Some(per_cpu) = adjusted.memory_per_cpu_mb.filter(|m| *m > limit) else {
        return;
    };
    let factor = per_cpu.div_ceil(limit);
    adjusted.cpus_per_task = scale_cpus(adjusted.cpus_per_task.max(1), factor);
    adjusted.cpus_per_gpu = adjusted.cpus_per_gpu.map(|cpus| scale_cpus(cpus, factor));
    adjusted.memory_per_cpu_mb = Some(per_cpu.div_ceil(factor));
}

fn scale_cpus(cpus: u32, factor: u64) -> u32 {
    u32::try_from(u64::from(cpus).saturating_mul(factor)).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn per_gpu_job(nodes: u32, gres: &str, cpus_per_gpu: u32) -> JobSpec {
        JobSpec {
            num_nodes: nodes,
            num_tasks: nodes,
            cpus_per_task: 1,
            cpus_per_gpu: Some(cpus_per_gpu),
            gres: vec![gres.into()],
            ..Default::default()
        }
    }

    #[test]
    fn def_cpu_per_gpu_merges_independently_of_the_memory_knobs() {
        let current = Partition {
            def_mem_per_gpu: Some(8192),
            ..Default::default()
        };
        let set = PartitionLimitsUpdate {
            def_cpu_per_gpu: Some(Some(16)),
            ..Default::default()
        };
        let merged = set.merged(&current).unwrap();
        assert_eq!(merged.def_cpu_per_gpu, Some(16));
        assert_eq!(merged.def_mem_per_gpu, Some(8192));

        let mut partition = current.clone();
        merged.apply_to(&mut partition);
        assert_eq!(partition.def_cpu_per_gpu, Some(16));
        let clear = PartitionLimitsUpdate {
            def_cpu_per_gpu: Some(None),
            ..Default::default()
        };
        assert_eq!(clear.merged(&partition).unwrap().def_cpu_per_gpu, None);
        assert_eq!(
            PartitionLimitsUpdate::replacing(&merged).def_cpu_per_gpu,
            Some(Some(16))
        );
    }

    #[test]
    fn a_pre_change_limits_update_has_no_def_cpu_per_gpu() {
        let update: PartitionLimitsUpdate =
            serde_json::from_str(r#"{"def_mem_per_gpu":4096}"#).unwrap();
        assert_eq!(update.def_cpu_per_gpu, None);
        let limits: PartitionResourceLimits = serde_json::from_str("{}").unwrap();
        assert_eq!(limits.def_cpu_per_gpu, None);
    }

    #[test]
    fn node_cpu_counts_size_per_gpu_jobs_by_their_gpus() {
        let spec = per_gpu_job(2, "gpu:4", 8);
        assert_eq!(node_cpu_counts(&spec), vec![32, 32]);
        assert_eq!(min_node_cpus(&spec), 32);
    }

    #[test]
    fn node_cpu_counts_ignore_gpus_without_cpus_per_gpu() {
        let spec = JobSpec {
            num_nodes: 2,
            num_tasks: 2,
            cpus_per_task: 3,
            gpus: Some(crate::gpu_request::GpuRequest::new(3, None)),
            ..Default::default()
        };
        assert_eq!(node_cpu_counts(&spec), vec![3, 3]);
    }

    #[test]
    fn a_max_mem_per_cpu_raise_below_the_per_gpu_count_changes_nothing() {
        let p = Partition {
            max_mem_per_cpu: Some(1000),
            ..Default::default()
        };
        let spec = JobSpec {
            memory_per_node_mb: Some(4000),
            ..per_gpu_job(1, "gpu:1", 8)
        };
        // 4000 / 1000 needs 4 CPUs; the GPU already brings 8.
        assert_eq!(p.memory_adjusted(&spec).cpus_per_task, 1);
    }

    #[test]
    fn a_max_mem_per_cpu_split_keeps_a_per_gpu_jobs_memory() {
        let p = Partition {
            max_mem_per_cpu: Some(1024),
            ..Default::default()
        };
        let spec = JobSpec {
            memory_per_cpu_mb: Some(2048),
            ..per_gpu_job(1, "gpu:2", 8)
        };
        let before = peak_node_memory_mb(&spec);
        let mut adjusted = spec.clone();
        p.memory_adjusted(&spec).apply_to(&mut adjusted);
        assert_eq!(adjusted.memory_per_cpu_mb, Some(1024));
        assert_eq!(node_cpu_counts(&adjusted), vec![32]);
        assert_eq!(peak_node_memory_mb(&adjusted), before);
    }

    #[test]
    fn an_adjustment_recorded_without_cpus_per_gpu_keeps_the_jobs_value() {
        let recorded: MemoryAdjustment =
            serde_json::from_str(r#"{"cpus_per_task":2,"memory_per_cpu_mb":1024}"#).unwrap();
        assert_eq!(recorded.cpus_per_gpu, None);
        let mut spec = per_gpu_job(1, "gpu:2", 8);
        recorded.apply_to(&mut spec);
        assert_eq!(spec.cpus_per_gpu, Some(8));
        assert_eq!(spec.cpus_per_task, 2);
    }

    #[test]
    fn candidate_requests_group_partitions_that_reshape_the_job_alike() {
        let per_cpu = |name: &str, limit| Partition {
            name: name.into(),
            max_mem_per_cpu: Some(limit),
            ..Default::default()
        };
        let plain = Partition {
            name: "plain".into(),
            ..Default::default()
        };
        let candidates = [
            per_cpu("a", 2 * GIB),
            plain,
            per_cpu("b", GIB),
            per_cpu("a2", 2 * GIB),
        ];
        let job = spec(2, None, Some(3 * GIB));
        let requests = candidate_requests(&job, &candidates);
        let shapes: Vec<(u32, Vec<&str>)> = requests
            .iter()
            .map(|r| (r.spec.cpus_per_task, r.partitions.clone()))
            .collect();
        assert_eq!(
            shapes,
            [(4, vec!["a", "a2"]), (2, vec!["plain"]), (6, vec!["b"])]
        );

        let alone = candidate_requests(&job, []);
        assert_eq!(alone.len(), 1);
        assert_eq!(alone[0].spec.cpus_per_task, 2);
        assert!(alone[0].partitions.is_empty());
    }

    const GIB: u64 = 1024;

    fn spec(cpus_per_task: u32, mem_per_node: Option<u64>, mem_per_cpu: Option<u64>) -> JobSpec {
        JobSpec {
            cpus_per_task,
            memory_per_node_mb: mem_per_node,
            memory_per_cpu_mb: mem_per_cpu,
            ..Default::default()
        }
    }

    fn per_cpu_limit(limit: u64) -> Partition {
        Partition {
            max_mem_per_cpu: Some(limit),
            ..Default::default()
        }
    }

    #[test]
    fn mem_per_cpu_within_the_limit_is_unchanged() {
        let p = per_cpu_limit(2 * GIB);
        for mem in [GIB, 2 * GIB] {
            let s = spec(2, None, Some(mem));
            assert_eq!(p.memory_adjusted(&s), MemoryAdjustment::of(&s));
        }
    }

    #[test]
    fn mem_per_cpu_over_the_limit_multiplies_cpus() {
        let p = per_cpu_limit(2 * GIB);
        let three = p.memory_adjusted(&spec(2, None, Some(3 * GIB)));
        assert_eq!(three.cpus_per_task, 4);
        assert_eq!(three.memory_per_cpu_mb, Some(1536));

        let five = p.memory_adjusted(&spec(2, None, Some(5 * GIB)));
        assert_eq!(five.cpus_per_task, 6);
        assert_eq!(five.memory_per_cpu_mb, Some(1707));
    }

    #[test]
    fn mem_per_node_over_the_limit_raises_cpus() {
        let p = per_cpu_limit(2 * GIB);
        let adjusted = p.memory_adjusted(&spec(2, Some(8 * GIB), None));
        assert_eq!(adjusted.cpus_per_task, 4);
        assert_eq!(adjusted.memory_per_node_mb, Some(8 * GIB));

        let fits = spec(4, Some(8 * GIB), None);
        assert_eq!(p.memory_adjusted(&fits), MemoryAdjustment::of(&fits));
    }

    #[test]
    fn mem_per_node_raise_spreads_over_the_tasks_on_a_node() {
        let p = per_cpu_limit(GIB);
        let s = JobSpec {
            num_tasks: 4,
            tasks_per_node: Some(4),
            ..spec(1, Some(10 * GIB), None)
        };
        assert_eq!(p.memory_adjusted(&s).cpus_per_task, 3);
    }

    #[test]
    fn adjustment_is_idempotent_for_one_partition() {
        let p = per_cpu_limit(2 * GIB);
        for s in [
            spec(2, None, Some(5 * GIB)),
            spec(2, Some(8 * GIB), None),
            spec(1, None, None),
        ] {
            let mut once = s.clone();
            p.memory_adjusted(&s).apply_to(&mut once);
            assert_eq!(p.memory_adjusted(&once), MemoryAdjustment::of(&once));
        }
    }

    #[test]
    fn default_memory_applies_only_when_the_job_names_none() {
        let p = Partition {
            def_mem_per_cpu: Some(512),
            ..Default::default()
        };
        assert_eq!(
            p.memory_adjusted(&spec(2, None, None)).memory_per_cpu_mb,
            Some(512)
        );
        let named = spec(2, Some(GIB), None);
        assert_eq!(p.memory_adjusted(&named), MemoryAdjustment::of(&named));

        let per_node = Partition {
            def_mem_per_node: Some(4 * GIB),
            ..Default::default()
        };
        assert_eq!(
            per_node
                .memory_adjusted(&spec(1, None, None))
                .memory_per_node_mb,
            Some(4 * GIB)
        );
    }

    #[test]
    fn default_memory_per_gpu_scales_with_the_gpus_on_a_node() {
        let p = Partition {
            def_mem_per_gpu: Some(8 * GIB),
            ..Default::default()
        };
        let gpu_job = JobSpec {
            gres: vec!["gpu:2".into()],
            ..spec(1, None, None)
        };
        let adjusted = p.memory_adjusted(&gpu_job);
        assert_eq!(
            (adjusted.memory_per_node_mb, adjusted.memory_per_gpu_mb),
            (None, Some(8 * GIB))
        );
        let mut applied = gpu_job.clone();
        adjusted.apply_to(&mut applied);
        assert_eq!(node_memory_mb(&applied, 1, 3), 24 * GIB);
        assert_eq!(node_memory_mb(&applied, 1, 1), 8 * GIB);
        let cpu_job = spec(1, None, None);
        assert_eq!(p.memory_adjusted(&cpu_job), MemoryAdjustment::of(&cpu_job));
    }

    #[test]
    fn max_mem_per_node_checks_a_gpu_total_at_its_most_even_spread() {
        let p = Partition {
            def_mem_per_gpu: Some(8 * GIB),
            max_mem_per_node: Some(16 * GIB),
            ..Default::default()
        };
        let total = |gpus| JobSpec {
            num_nodes: 2,
            num_tasks: 2,
            gpus: Some(crate::gpu_request::GpuRequest {
                count: gpus,
                gpu_type: None,
            }),
            ..spec(1, None, None)
        };
        assert_eq!(p.max_mem_per_node_exceeded(&total(4)), None);
        assert_eq!(
            p.max_mem_per_node_exceeded(&total(5)),
            Some((24 * GIB, 16 * GIB))
        );
        assert!(p.node_memory_allowed(1, 16 * GIB));
        assert!(!p.node_memory_allowed(1, 24 * GIB));
    }

    #[test]
    fn per_gpu_memory_is_raised_for_at_its_most_even_spread() {
        let p = Partition {
            def_mem_per_gpu: Some(4 * GIB),
            max_mem_per_cpu: Some(2 * GIB),
            ..Default::default()
        };
        let job = JobSpec {
            gres: vec!["gpu:2".into()],
            ..spec(1, None, None)
        };
        let adjusted = p.memory_adjusted(&job);
        assert_eq!(adjusted.cpus_per_task, 4);
        assert!(p.node_memory_allowed(4, 8 * GIB));
        assert!(!p.node_memory_allowed(4, 12 * GIB));
    }

    #[test]
    fn mem_per_gpu_over_the_limit_scales_cpus_per_gpu_not_cpus_per_task() {
        let p = Partition {
            max_mem_per_cpu: Some(1024),
            ..Default::default()
        };
        // -N2 --gpus=3 --cpus-per-gpu=4 --mem-per-gpu=<mem>: an uneven 2+1 spread,
        // so a fix that still compared the fullest node's memory against the
        // smallest node's CPUs would size this job from the wrong node.
        let job = |mem_per_gpu_mb| JobSpec {
            num_nodes: 2,
            num_tasks: 2,
            cpus_per_gpu: Some(4),
            memory_per_gpu_mb: Some(mem_per_gpu_mb),
            gpus: Some(crate::gpu_request::GpuRequest {
                count: 3,
                gpu_type: None,
            }),
            ..Default::default()
        };

        // 4G / 1024 = 4: no larger than the existing per-GPU count, so unchanged.
        let fits = p.memory_adjusted(&job(4 * GIB));
        assert_eq!(fits.cpus_per_gpu, Some(4));
        assert_eq!(fits.cpus_per_task, 1);

        // 8G / 1024 = 8: raises cpus_per_gpu; cpus_per_task must stay untouched.
        let raised = p.memory_adjusted(&job(8 * GIB));
        assert_eq!(raised.cpus_per_gpu, Some(8));
        assert_eq!(raised.cpus_per_task, 1);
    }

    #[test]
    fn per_node_mem_over_the_limit_scales_cpus_per_gpu_from_the_thinnest_node() {
        let p = Partition {
            max_mem_per_cpu: Some(1024),
            ..Default::default()
        };
        // -N2 --gpus=3 --cpus-per-gpu=<cpg> --mem=8G: an uneven 2+1 spread, so the
        // 1-GPU node needs the whole 8 CPUs from cpus_per_gpu alone.
        let job = |cpus_per_gpu| JobSpec {
            num_nodes: 2,
            num_tasks: 2,
            cpus_per_gpu: Some(cpus_per_gpu),
            memory_per_node_mb: Some(8 * GIB),
            gpus: Some(crate::gpu_request::GpuRequest {
                count: 3,
                gpu_type: None,
            }),
            ..Default::default()
        };

        // needed = 8G / 1024 = 8 per node; cpus_per_gpu=2 only gives the 1-GPU
        // node 2 CPUs, so it is raised to cover the full per-node need.
        let raised = p.memory_adjusted(&job(2));
        assert_eq!(raised.cpus_per_gpu, Some(8));
        assert_eq!(raised.cpus_per_task, 1);

        // cpus_per_gpu=10 already gives the 1-GPU node 10 CPUs, more than the
        // 8 needed: no-op.
        let fits = job(10);
        assert_eq!(p.memory_adjusted(&fits), MemoryAdjustment::of(&fits));
    }

    #[test]
    fn default_then_per_cpu_limit_compose() {
        let p = Partition {
            def_mem_per_cpu: Some(3 * GIB),
            max_mem_per_cpu: Some(2 * GIB),
            ..Default::default()
        };
        let adjusted = p.memory_adjusted(&spec(2, None, None));
        assert_eq!(adjusted.cpus_per_task, 4);
        assert_eq!(adjusted.memory_per_cpu_mb, Some(1536));
    }

    #[test]
    fn max_mem_per_node_is_per_job_and_uses_the_adjusted_request() {
        let p = Partition {
            max_mem_per_node: Some(64 * GIB),
            ..Default::default()
        };
        assert_eq!(
            p.max_mem_per_node_exceeded(&spec(1, Some(64 * GIB), None)),
            None
        );
        assert_eq!(
            p.max_mem_per_node_exceeded(&spec(1, Some(65 * GIB), None)),
            Some((65 * GIB, 64 * GIB))
        );
        assert_eq!(
            p.max_mem_per_node_exceeded(&spec(4, None, Some(20 * GIB))),
            Some((80 * GIB, 64 * GIB))
        );

        let defaulted = Partition {
            def_mem_per_node: Some(100 * GIB),
            ..p
        };
        assert!(defaulted
            .max_mem_per_node_exceeded(&spec(1, None, None))
            .is_some());
    }

    #[test]
    fn max_mem_per_node_checks_the_largest_uneven_cpu_share() {
        let p = Partition {
            max_mem_per_node: Some(4 * GIB),
            ..Default::default()
        };
        let uneven = JobSpec {
            num_nodes: 2,
            num_tasks: 3,
            ..spec(1, None, Some(4 * GIB))
        };
        assert_eq!(
            p.max_mem_per_node_exceeded(&uneven),
            Some((8 * GIB, 4 * GIB))
        );
    }

    #[test]
    fn a_zero_memory_request_is_checked_like_no_memory_request() {
        let p = Partition {
            max_mem_per_node: Some(64 * GIB),
            def_mem_per_node: Some(100 * GIB),
            ..Default::default()
        };
        let unset = p.max_mem_per_node_exceeded(&spec(1, None, None));
        assert_eq!(unset, Some((100 * GIB, 64 * GIB)));
        assert_eq!(p.max_mem_per_node_exceeded(&spec(1, Some(0), None)), unset);
        assert_eq!(p.max_mem_per_node_exceeded(&spec(1, None, Some(0))), unset);

        let per_cpu_default = Partition {
            def_mem_per_cpu: Some(GIB),
            ..Default::default()
        };
        let adjusted = per_cpu_default.memory_adjusted(&spec(2, Some(0), None));
        assert_eq!(
            (adjusted.memory_per_node_mb, adjusted.memory_per_cpu_mb),
            (None, Some(GIB))
        );
    }

    #[test]
    fn partition_without_a_memory_policy_changes_nothing() {
        let p = Partition::default();
        assert!(!p.has_memory_policy());
        let s = spec(2, None, Some(64 * GIB));
        assert_eq!(p.memory_adjusted(&s), MemoryAdjustment::of(&s));
        assert_eq!(p.max_mem_per_node_exceeded(&s), None);
    }

    #[test]
    fn max_cpus_per_node_folds_into_the_cpu_key() {
        let folded = fold_max_cpus_per_node(BTreeMap::new(), Some(8)).unwrap();
        assert_eq!(folded, BTreeMap::from([("cpu".to_string(), 8)]));

        let map = BTreeMap::from([("mem".to_string(), 1024)]);
        assert_eq!(fold_max_cpus_per_node(map.clone(), None).unwrap(), map);

        let err =
            fold_max_cpus_per_node(BTreeMap::from([("cpu".to_string(), 8)]), Some(8)).unwrap_err();
        assert!(err.contains("max_cpus_per_node"), "{err}");
    }

    #[test]
    fn resource_limits_round_trip_through_a_partition() {
        let limits = PartitionResourceLimits {
            max_tres_per_node: BTreeMap::from([("cpu".to_string(), 8)]),
            max_mem_per_cpu: Some(2048),
            def_mem_per_gpu: Some(4096),
            ..Default::default()
        };
        let mut p = Partition::default();
        limits.apply_to(&mut p);
        assert_eq!(PartitionResourceLimits::of(&p), limits);

        PartitionResourceLimits::default().apply_to(&mut p);
        assert!(p.max_tres_per_node.is_empty());
        assert_eq!(p.max_mem_per_cpu, None);
    }

    #[test]
    fn a_limits_update_keeps_absent_and_cleared_knobs_apart_on_the_wire() {
        let update: PartitionLimitsUpdate =
            serde_json::from_str(r#"{"def_mem_per_cpu":null,"max_cpus_per_node":64}"#).unwrap();
        let expected = PartitionLimitsUpdate {
            max_cpus_per_node: Some(Some(64)),
            def_mem_per_cpu: Some(None),
            ..Default::default()
        };
        assert_eq!(update, expected);

        let wire = serde_json::to_string(&update).unwrap();
        assert_eq!(
            serde_json::from_str::<PartitionLimitsUpdate>(&wire).unwrap(),
            expected
        );
        assert_eq!(
            serde_json::from_str::<PartitionLimitsUpdate>("{}").unwrap(),
            PartitionLimitsUpdate::default()
        );
    }
}
