// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Per-partition, per-node resource caps (`max_tres_per_node`).

use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use spur_core::gpu_request::resolve_gpu_demand;
use spur_core::job::{Job, PartitionTresOvershoot};
use spur_core::partition::{
    requested_partition_names, tres_key_report_rank, typed_gpu_tres_key, Partition, TRES_CPU,
    TRES_GPU, TRES_MEM,
};
use spur_core::partition_limits::MemoryAdjustment;
use spur_core::resource::{ResourceAllocations, ResourceSet};

/// Amount a placement charges against partition caps, per TRES key (`mem` in
/// MiB). Kept as TRES counts rather than device allocations because whether a
/// GPU counts against a typed key depends on how it was requested, not on the
/// device it landed on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TresCharge(BTreeMap<String, u64>);

/// What one running job holds on a node, and until when. Each job keeps its
/// own end so another partition's long job cannot stretch this charge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningCharge {
    pub tres: TresCharge,
    pub until: DateTime<Utc>,
}

/// Partition name -> node name -> the partition's running jobs there.
pub type PartitionNodeUsage = HashMap<String, HashMap<String, Vec<RunningCharge>>>;

impl TresCharge {
    /// Charge for `cpus`/`mem`/`gpus`. GPUs from a typed request count against
    /// both `gres/gpu:<type>` and `gres/gpu`; untyped ones only against `gres/gpu`.
    pub fn new(cpus: u64, memory_mb: u64, gpus: u64, typed_gpu: Option<&str>) -> Self {
        let mut tres = BTreeMap::new();
        let typed_key = typed_gpu.map(typed_gpu_tres_key);
        let entries = [
            (Some(TRES_CPU.to_string()), cpus),
            (Some(TRES_MEM.to_string()), memory_mb),
            (Some(TRES_GPU.to_string()), gpus),
            (typed_key, gpus),
        ];
        for (key, amount) in entries {
            if let (Some(key), true) = (key, amount > 0) {
                tres.insert(key, amount);
            }
        }
        Self(tres)
    }

    pub fn from_request(request: &ResourceSet, typed_gpu: Option<&str>) -> Self {
        Self::new(
            u64::from(request.cpus),
            request.memory_mb,
            request.gpus.len() as u64,
            typed_gpu,
        )
    }

    pub fn from_allocation(alloc: &ResourceAllocations, typed_gpu: Option<&str>) -> Self {
        Self::new(
            u64::from(alloc.cpus),
            alloc.memory_mb,
            alloc.total_device_count("gpu"),
            typed_gpu,
        )
    }

    pub fn get(&self, key: &str) -> u64 {
        self.0.get(key).copied().unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn add(&mut self, other: &TresCharge) {
        for (key, amount) in &other.0 {
            let entry = self.0.entry(key.clone()).or_insert(0);
            *entry = entry.saturating_add(*amount);
        }
    }

    pub fn subtract(&mut self, other: &TresCharge) {
        for (key, amount) in &other.0 {
            if let Some(entry) = self.0.get_mut(key) {
                *entry = entry.saturating_sub(*amount);
            }
        }
        self.0.retain(|_, amount| *amount > 0);
    }
}

impl<K: Into<String>> FromIterator<(K, u64)> for TresCharge {
    fn from_iter<I: IntoIterator<Item = (K, u64)>>(iter: I) -> Self {
        Self(
            iter.into_iter()
                .map(|(key, amount)| (key.into(), amount))
                .filter(|(_, amount)| *amount > 0)
                .collect(),
        )
    }
}

pub fn is_capped(partition: &Partition) -> bool {
    !partition.max_tres_per_node.is_empty()
}

/// Whether adding `add` to `usage` keeps `partition` within its cap on `node`.
/// Returns the first violated key in `cpu`, `mem`, GRES order. A key the
/// request does not use is not checked, so a cap lowered below current usage
/// never blocks jobs that would not add to it.
pub fn partition_cap_ok(
    partition: &Partition,
    node: &str,
    usage: &TresCharge,
    add: &TresCharge,
) -> Result<(), PartitionTresOvershoot> {
    let violation = partition
        .max_tres_per_node
        .iter()
        .filter(|(key, &limit)| {
            let requested = add.get(key);
            requested > 0 && usage.get(key).saturating_add(requested) > limit
        })
        .min_by_key(|(key, _)| (tres_key_report_rank(key), key.as_str()));
    match violation {
        None => Ok(()),
        Some((key, &limit)) => Err(PartitionTresOvershoot {
            node: node.to_string(),
            resource: key.clone(),
            used: usage.get(key),
            requested: add.get(key),
            limit,
        }),
    }
}

/// GPU type a job's request names, which decides whether its GPUs count
/// against typed caps. `None` for untyped (generic) or no GPU requests.
pub fn job_typed_gpu(job: &Job) -> Option<String> {
    resolve_gpu_demand(&job.spec)
        .ok()?
        .gpu_type()
        .filter(|ty| *ty != "any")
        .map(str::to_string)
}

/// What a running job's allocation on one node charges to its partition.
pub fn job_node_charge(job: &Job, alloc: &ResourceAllocations) -> TresCharge {
    TresCharge::from_allocation(alloc, job_typed_gpu(job).as_deref())
}

/// The job as `partition` would run it, with its memory policy applied to
/// the request. Borrowed when the policy changes nothing.
pub fn job_in_partition<'a>(job: &'a Job, partition: &Partition) -> Cow<'a, Job> {
    let adjusted = partition.memory_adjusted(&job.spec);
    if adjusted == MemoryAdjustment::of(&job.spec) {
        return Cow::Borrowed(job);
    }
    let mut job = job.clone();
    adjusted.apply_to(&mut job.spec);
    Cow::Owned(job)
}

/// Partitions a job may be charged to: the requested ones that exist, or the
/// cluster default when it requested none. Request order is not meaningful.
pub fn candidate_partitions<'a>(job: &Job, partitions: &'a [Partition]) -> Vec<&'a Partition> {
    let requested: Vec<&str> = requested_partition_names(job.spec.partition.as_deref()).collect();
    if requested.is_empty() {
        return partitions.iter().filter(|p| p.is_default).collect();
    }
    partitions
        .iter()
        .filter(|p| requested.contains(&p.name.as_str()))
        .collect()
}

/// Ordering key for choosing among candidate partitions, as Slurm does for a
/// multi-partition request: the highest-tier partition that can start the job
/// now wins; when none can, the earliest start wins (ties to the higher tier).
/// Request order is ignored; the name only makes ties deterministic.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ResolutionRank<'a> {
    StartsNow(Reverse<u32>, DateTime<Utc>, &'a str),
    Reserves(DateTime<Utc>, Reverse<u32>, &'a str),
}

pub fn resolution_rank(
    partition: &Partition,
    start: DateTime<Utc>,
    now: DateTime<Utc>,
) -> ResolutionRank<'_> {
    let tier = Reverse(partition.priority_tier);
    let name = partition.name.as_str();
    if start <= now {
        ResolutionRank::StartsNow(tier, start, name)
    } else {
        ResolutionRank::Reserves(start, tier, name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_core::job::JobSpec;

    fn capped(name: &str, caps: &[(&str, u64)]) -> Partition {
        Partition {
            name: name.into(),
            max_tres_per_node: caps.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            ..Default::default()
        }
    }

    fn charge(entries: &[(&str, u64)]) -> TresCharge {
        entries.iter().copied().collect()
    }

    #[test]
    fn typed_gpu_request_charges_typed_and_generic_keys() {
        let c = TresCharge::new(4, 1024, 2, Some("a100"));
        assert_eq!(c.get(TRES_CPU), 4);
        assert_eq!(c.get(TRES_MEM), 1024);
        assert_eq!(c.get(TRES_GPU), 2);
        assert_eq!(c.get("gres/gpu:a100"), 2);
    }

    #[test]
    fn generic_gpu_request_charges_generic_key_only() {
        let c = TresCharge::new(4, 0, 2, None);
        assert_eq!(c.get(TRES_GPU), 2);
        assert_eq!(c, charge(&[("cpu", 4), ("gres/gpu", 2)]));
    }

    #[test]
    fn charge_add_and_subtract_drop_zero_entries() {
        let mut usage = charge(&[("cpu", 4)]);
        usage.add(&charge(&[("cpu", 2), ("mem", 10)]));
        assert_eq!(usage, charge(&[("cpu", 6), ("mem", 10)]));
        usage.subtract(&charge(&[("cpu", 6), ("mem", 20)]));
        assert!(usage.is_empty());
    }

    #[test]
    fn partition_cap_ok_returns_the_expected_violation_details() {
        let p = capped("cpu", &[("cpu", 64)]);
        let err = partition_cap_ok(&p, "n03", &charge(&[("cpu", 48)]), &charge(&[("cpu", 24)]))
            .unwrap_err();
        assert_eq!(
            err,
            PartitionTresOvershoot {
                node: "n03".into(),
                resource: "cpu".into(),
                used: 48,
                requested: 24,
                limit: 64,
            }
        );
        assert!(
            partition_cap_ok(&p, "n03", &charge(&[("cpu", 48)]), &charge(&[("cpu", 16)])).is_ok()
        );
    }

    #[test]
    fn first_violation_follows_cpu_mem_gres_order() {
        let p = capped("p", &[("gres/gpu", 1), ("mem", 100), ("cpu", 4)]);
        let add = charge(&[("cpu", 8), ("mem", 200), ("gres/gpu", 2)]);
        let err = partition_cap_ok(&p, "n1", &TresCharge::default(), &add).unwrap_err();
        assert_eq!(err.resource, "cpu");

        let add = charge(&[("cpu", 1), ("mem", 200), ("gres/gpu", 2)]);
        let err = partition_cap_ok(&p, "n1", &TresCharge::default(), &add).unwrap_err();
        assert_eq!(err.resource, "mem");
    }

    #[test]
    fn cpu_cap_binds_exactly_at_the_limit() {
        let p = capped("cpu", &[("cpu", 8)]);
        let idle = TresCharge::default();
        assert!(partition_cap_ok(&p, "n1", &idle, &charge(&[("cpu", 8)])).is_ok());
        assert!(partition_cap_ok(&p, "n1", &idle, &charge(&[("cpu", 9)])).is_err());
    }

    #[test]
    fn absent_or_empty_cap_is_unlimited() {
        let p = Partition::default();
        let huge = charge(&[("cpu", u64::MAX), ("gres/gpu", 1_000)]);
        assert!(partition_cap_ok(&p, "n1", &huge, &huge).is_ok());
    }

    #[test]
    fn zero_limit_forbids_only_that_resource() {
        let p = capped("cpuonly", &[("gres/gpu", 0)]);
        let idle = TresCharge::default();
        assert!(partition_cap_ok(&p, "n1", &idle, &TresCharge::new(4, 0, 1, None)).is_err());
        assert!(
            partition_cap_ok(&p, "n1", &idle, &TresCharge::new(4, 0, 1, Some("a100"))).is_err()
        );
        assert!(partition_cap_ok(&p, "n1", &idle, &TresCharge::new(64, 0, 0, None)).is_ok());

        let no_cpu = capped("none", &[("cpu", 0)]);
        assert!(partition_cap_ok(&no_cpu, "n1", &idle, &charge(&[("cpu", 1)])).is_err());
    }

    #[test]
    fn generic_request_is_not_blocked_by_a_typed_cap() {
        let p = capped("gpu", &[("gres/gpu:a100", 2)]);
        let used = TresCharge::new(0, 0, 2, Some("a100"));
        assert!(partition_cap_ok(&p, "n1", &used, &TresCharge::new(1, 0, 4, None)).is_ok());
        let err =
            partition_cap_ok(&p, "n1", &used, &TresCharge::new(1, 0, 1, Some("a100"))).unwrap_err();
        assert_eq!(err.resource, "gres/gpu:a100");
        assert_eq!((err.used, err.requested, err.limit), (2, 1, 2));
    }

    #[test]
    fn typed_request_is_also_bounded_by_the_generic_cap() {
        let p = capped("gpu", &[("gres/gpu", 2)]);
        let used = TresCharge::new(0, 0, 2, None);
        let err =
            partition_cap_ok(&p, "n1", &used, &TresCharge::new(1, 0, 1, Some("a100"))).unwrap_err();
        assert_eq!(err.resource, "gres/gpu");
    }

    #[test]
    fn typed_cap_for_a_type_absent_from_the_cluster_is_vacuous() {
        let p = capped("gpu", &[("gres/gpu:h100", 1)]);
        let idle = TresCharge::default();
        assert!(
            partition_cap_ok(&p, "n1", &idle, &TresCharge::new(1, 0, 8, Some("mi300x"))).is_ok()
        );
        assert!(partition_cap_ok(&p, "n1", &idle, &TresCharge::new(1, 0, 8, None)).is_ok());
    }

    #[test]
    fn a_cap_lowered_below_usage_only_blocks_jobs_adding_to_it() {
        let p = capped("gpu", &[("gres/gpu", 1)]);
        let used = TresCharge::new(4, 0, 4, None);
        assert!(partition_cap_ok(&p, "n1", &used, &charge(&[("cpu", 2)])).is_ok());
        assert!(partition_cap_ok(&p, "n1", &used, &TresCharge::new(2, 0, 1, None)).is_err());
    }

    #[test]
    fn mem_cap_counts_memory_in_mib() {
        let p = capped("cpu", &[("mem", 131_072)]);
        let used = charge(&[("mem", 65_536 * 2)]);
        let err = partition_cap_ok(&p, "n1", &used, &charge(&[("mem", 65_536)])).unwrap_err();
        assert_eq!((err.resource.as_str(), err.used), ("mem", 131_072));
    }

    fn job_with(partition: Option<&str>, gres: &[&str]) -> Job {
        Job::new(
            1,
            JobSpec {
                partition: partition.map(str::to_string),
                gres: gres.iter().map(|g| g.to_string()).collect(),
                num_nodes: 1,
                num_tasks: 1,
                cpus_per_task: 1,
                ..Default::default()
            },
        )
    }

    #[test]
    fn job_typed_gpu_distinguishes_typed_from_generic_requests() {
        assert_eq!(
            job_typed_gpu(&job_with(None, &["gpu:a100:2"])).as_deref(),
            Some("a100")
        );
        assert_eq!(job_typed_gpu(&job_with(None, &["gpu:2"])), None);
        assert_eq!(job_typed_gpu(&job_with(None, &[])), None);
    }

    #[test]
    fn job_node_charge_uses_the_request_type() {
        let mut alloc = ResourceAllocations::with_scalar(4, 512);
        alloc.devices.insert(
            "gpu".into(),
            (0u64..2)
                .map(spur_core::resource::AllocatedDevice::injectable)
                .collect(),
        );
        let typed = job_node_charge(&job_with(None, &["gpu:a100:2"]), &alloc);
        assert_eq!(typed, TresCharge::new(4, 512, 2, Some("a100")));
        let generic = job_node_charge(&job_with(None, &["gpu:2"]), &alloc);
        assert_eq!(generic, TresCharge::new(4, 512, 2, None));
    }

    #[test]
    fn candidates_are_the_requested_partitions_or_the_default() {
        let parts = vec![
            Partition {
                name: "debug".into(),
                is_default: true,
                ..Default::default()
            },
            capped("cpu", &[("cpu", 8)]),
            capped("gpu", &[("gres/gpu", 8)]),
        ];
        let names = |job: &Job| -> Vec<String> {
            candidate_partitions(job, &parts)
                .iter()
                .map(|p| p.name.clone())
                .collect()
        };
        assert_eq!(names(&job_with(Some("gpu, cpu"), &[])), ["cpu", "gpu"]);
        assert_eq!(names(&job_with(Some("cpu,nope"), &[])), ["cpu"]);
        assert_eq!(names(&job_with(None, &[])), ["debug"]);
        assert_eq!(names(&job_with(Some(""), &[])), ["debug"]);
    }

    fn tiered(name: &str, priority_tier: u32) -> Partition {
        Partition {
            name: name.into(),
            priority_tier,
            ..Default::default()
        }
    }

    #[test]
    fn rank_prefers_a_partition_starting_now_over_a_higher_tier_that_waits() {
        let now = Utc::now();
        let later = now + chrono::Duration::hours(2);
        let (low, high) = (tiered("a", 1), tiered("z", 5));
        assert!(resolution_rank(&low, now, now) < resolution_rank(&high, later, now));
    }

    #[test]
    fn rank_among_partitions_starting_now_prefers_the_higher_tier() {
        let now = Utc::now();
        let (low, high) = (tiered("a", 1), tiered("z", 5));
        assert!(resolution_rank(&high, now, now) < resolution_rank(&low, now, now));
    }

    #[test]
    fn rank_among_waiting_partitions_prefers_earliest_start_then_tier() {
        let now = Utc::now();
        let h1 = now + chrono::Duration::hours(1);
        let h2 = now + chrono::Duration::hours(2);
        let (low, high, other) = (tiered("a", 1), tiered("z", 5), tiered("b", 1));
        assert!(resolution_rank(&low, h1, now) < resolution_rank(&high, h2, now));
        assert!(resolution_rank(&high, h1, now) < resolution_rank(&low, h1, now));
        assert!(resolution_rank(&low, h1, now) < resolution_rank(&other, h1, now));
    }
}
