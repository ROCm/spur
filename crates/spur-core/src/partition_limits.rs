// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Per-partition resource limits as one unit (for replication and the API),
//! and the per-job memory policy (`def_mem_*`, `max_mem_per_cpu`,
//! `max_mem_per_node`) a partition applies to a job's request.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::gpu_request::{resolve_gpu_demand, GpuDemand};
use crate::job::JobSpec;
use crate::partition::{Partition, TRES_CPU};

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
        }
    }

    pub fn apply_to(&self, partition: &mut Partition) {
        partition.max_tres_per_node = self.max_tres_per_node.clone();
        partition.max_mem_per_node = self.max_mem_per_node;
        partition.max_mem_per_cpu = self.max_mem_per_cpu;
        partition.def_mem_per_node = self.def_mem_per_node;
        partition.def_mem_per_cpu = self.def_mem_per_cpu;
        partition.def_mem_per_gpu = self.def_mem_per_gpu;
    }
}

/// `max_tres_per_node` with the `max_cpus_per_node` alias folded into its
/// `cpu` key. Setting both is an error: two spellings of one cap would leave
/// the effective value to a precedence rule an operator cannot see.
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

/// The request fields a partition's memory policy may change. Recorded on the
/// job when it starts so `scontrol show job` reports what it actually got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryAdjustment {
    pub cpus_per_task: u32,
    #[serde(default)]
    pub memory_per_node_mb: Option<u64>,
    #[serde(default)]
    pub memory_per_cpu_mb: Option<u64>,
}

impl MemoryAdjustment {
    pub fn of(spec: &JobSpec) -> Self {
        Self {
            cpus_per_task: spec.cpus_per_task,
            memory_per_node_mb: spec.memory_per_node_mb,
            memory_per_cpu_mb: spec.memory_per_cpu_mb,
        }
    }

    pub fn apply_to(&self, spec: &mut JobSpec) {
        spec.cpus_per_task = self.cpus_per_task;
        spec.memory_per_node_mb = self.memory_per_node_mb;
        spec.memory_per_cpu_mb = self.memory_per_cpu_mb;
    }

    /// Per-node CPUs and memory (MiB) of `spec` with this adjustment applied,
    /// for comparing the adjusted requests of several partitions.
    pub fn node_footprint(&self, spec: &JobSpec) -> (u64, u64) {
        let mut adjusted = spec.clone();
        self.apply_to(&mut adjusted);
        let cpus = node_cpus(&adjusted);
        (cpus, node_memory_mb(&adjusted, cpus))
    }
}

/// Tasks per node as the scheduler sizes a node's share (at least one).
fn tasks_per_node(spec: &JobSpec) -> u32 {
    spec.tasks_per_node
        .unwrap_or(spec.num_tasks / spec.num_nodes.max(1))
        .max(1)
}

/// CPUs the job asks for on one node, floored at one like the scheduler does.
pub fn node_cpus(spec: &JobSpec) -> u64 {
    (u64::from(tasks_per_node(spec)) * u64::from(spec.cpus_per_task)).max(1)
}

/// Memory (MiB) the job asks for on one node holding `cpus` of its CPUs.
pub fn node_memory_mb(spec: &JobSpec, cpus: u64) -> u64 {
    spec.memory_per_node_mb
        .or_else(|| spec.memory_per_cpu_mb.map(|m| m.saturating_mul(cpus)))
        .unwrap_or(0)
}

/// Largest GPU count the job asks for on any one node.
fn gpus_per_node(spec: &JobSpec) -> u64 {
    match resolve_gpu_demand(spec) {
        Ok(GpuDemand::PerNode { counts, .. }) => u64::from(counts.into_iter().max().unwrap_or(0)),
        Ok(GpuDemand::Total { count, .. }) => {
            u64::from(count).div_ceil(u64::from(spec.num_nodes.max(1)))
        }
        Ok(GpuDemand::None) | Err(_) => 0,
    }
}

impl Partition {
    /// Whether this partition's memory policy can change a job's request.
    pub fn has_memory_policy(&self) -> bool {
        self.max_mem_per_cpu.is_some()
            || self.def_mem_per_node.is_some()
            || self.def_mem_per_cpu.is_some()
            || self.def_mem_per_gpu.is_some()
    }

    /// The request as this partition runs it: the `def_mem_*` default when the
    /// job names no memory, then Slurm's `max_mem_per_cpu` rule, which gives
    /// the job more CPUs instead of rejecting it. Applying it twice for the
    /// same partition changes nothing.
    pub fn memory_adjusted(&self, spec: &JobSpec) -> MemoryAdjustment {
        let mut adjusted = MemoryAdjustment::of(spec);
        if adjusted.memory_per_node_mb.is_none() && adjusted.memory_per_cpu_mb.is_none() {
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
            let gpus = gpus_per_node(spec);
            if gpus > 0 {
                adjusted.memory_per_node_mb = Some(per_gpu.saturating_mul(gpus));
            }
        }
    }

    /// `(requested, limit)` when the job's per-node memory, as this partition
    /// would run it, exceeds `max_mem_per_node`.
    pub fn max_mem_per_node_exceeded(&self, spec: &JobSpec) -> Option<(u64, u64)> {
        let limit = self.max_mem_per_node?;
        let mut adjusted = spec.clone();
        self.memory_adjusted(spec).apply_to(&mut adjusted);
        let requested = node_memory_mb(&adjusted, node_cpus(&adjusted));
        (requested > limit).then_some((requested, limit))
    }
}

/// Slurm `_valid_pn_min_mem`: `--mem-per-cpu` over the limit multiplies the
/// CPUs per task by `ceil(mem / limit)` and divides memory per CPU by the same
/// factor; `--mem` over `cpus × limit` raises the CPUs to `ceil(mem / limit)`.
fn raise_cpus_for_memory(spec: &JobSpec, limit: u64, adjusted: &mut MemoryAdjustment) {
    if let Some(per_node) = adjusted.memory_per_node_mb.filter(|m| *m > 0) {
        let mut probe = spec.clone();
        adjusted.apply_to(&mut probe);
        let needed = per_node.div_ceil(limit);
        if needed <= node_cpus(&probe) {
            return;
        }
        let per_task = needed.div_ceil(u64::from(tasks_per_node(&probe)));
        adjusted.cpus_per_task = u32::try_from(per_task).unwrap_or(u32::MAX);
        return;
    }
    let Some(per_cpu) = adjusted.memory_per_cpu_mb.filter(|m| *m > limit) else {
        return;
    };
    let factor = per_cpu.div_ceil(limit);
    let cpus = u64::from(adjusted.cpus_per_task.max(1)).saturating_mul(factor);
    adjusted.cpus_per_task = u32::try_from(cpus).unwrap_or(u32::MAX);
    adjusted.memory_per_cpu_mb = Some(per_cpu.div_ceil(factor));
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(
            p.memory_adjusted(&gpu_job).memory_per_node_mb,
            Some(16 * GIB)
        );
        let cpu_job = spec(1, None, None);
        assert_eq!(p.memory_adjusted(&cpu_job), MemoryAdjustment::of(&cpu_job));
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
}
