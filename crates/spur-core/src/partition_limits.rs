// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Per-partition resource limits as one unit (for replication and the API),
//! and the per-job memory policy (`def_mem_*`, `max_mem_per_cpu`,
//! `max_mem_per_node`) a partition applies to a job's request.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::gpu_request::{resolve_gpu_demand, tasks_per_node_counts, GpuDemand};
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
    #[serde(default)]
    pub memory_per_gpu_mb: Option<u64>,
}

impl MemoryAdjustment {
    pub fn of(spec: &JobSpec) -> Self {
        Self {
            cpus_per_task: spec.cpus_per_task,
            memory_per_node_mb: spec.memory_per_node_mb,
            memory_per_cpu_mb: spec.memory_per_cpu_mb,
            memory_per_gpu_mb: spec.memory_per_gpu_mb,
        }
    }

    pub fn apply_to(&self, spec: &mut JobSpec) {
        spec.cpus_per_task = self.cpus_per_task;
        spec.memory_per_node_mb = self.memory_per_node_mb;
        spec.memory_per_cpu_mb = self.memory_per_cpu_mb;
        spec.memory_per_gpu_mb = self.memory_per_gpu_mb;
    }
}

/// A request a job could run with, and the candidate partitions whose memory
/// policy reshapes it into exactly that request.
#[derive(Debug, Clone)]
pub struct CandidateRequest<'a> {
    pub spec: JobSpec,
    pub partitions: Vec<&'a str>,
}

/// The distinct requests the job would run with in `candidates`, in candidate
/// order; partitions that reshape it the same way share one entry. With no
/// candidates the job's own request is the only one.
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
    (u64::from(min_tasks_per_node(spec)) * u64::from(spec.cpus_per_task)).max(1)
}

/// CPUs the job uses on each node, in task-layout order (e.g. `-N2 -n7` ->
/// `[4, 3]`), each floored at one.
pub fn node_cpu_counts(spec: &JobSpec) -> Vec<u32> {
    let cpus_per_task = spec.cpus_per_task.max(1);
    tasks_per_node_counts(spec, spec.num_nodes.max(1))
        .into_iter()
        .map(|tasks| tasks.saturating_mul(cpus_per_task).max(1))
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

/// Largest GPU count on any one node when a GPU total is spread as evenly as
/// possible. A total may land unevenly, but this spread always exists.
fn even_node_gpus(spec: &JobSpec) -> u64 {
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

    /// The request as this partition runs it: the `def_mem_*` default when the
    /// job names no memory, then Slurm's `max_mem_per_cpu` rule, which gives
    /// the job more CPUs instead of rejecting it. Applying it twice for the
    /// same partition changes nothing.
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

    /// Whether one node's share of a job, `memory_mb` with `cpus` CPUs, stays
    /// within this partition's per-job memory limits. Binds only where the
    /// share differs per node, as GPUs spread unevenly do.
    pub fn node_memory_allowed(&self, cpus: u64, memory_mb: u64) -> bool {
        let per_node_ok = self.max_mem_per_node.is_none_or(|limit| memory_mb <= limit);
        let per_cpu_ok = self
            .max_mem_per_cpu
            .filter(|limit| *limit > 0)
            .is_none_or(|limit| memory_mb <= limit.saturating_mul(cpus));
        per_node_ok && per_cpu_ok
    }
}

/// Slurm `_valid_pn_min_mem`: `--mem-per-cpu` over the limit multiplies the
/// CPUs per task by `ceil(mem / limit)` and divides memory per CPU by the same
/// factor; `--mem` over `cpus × limit` raises the CPUs to `ceil(mem / limit)`.
/// Per-GPU memory is raised for like `--mem` at its most even GPU spread.
fn raise_cpus_for_memory(spec: &JobSpec, limit: u64, adjusted: &mut MemoryAdjustment) {
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
        let per_task = needed.div_ceil(u64::from(min_tasks_per_node(&probe)));
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
}
