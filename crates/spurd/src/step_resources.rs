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
    pub gpus: &'a [u64],
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
    let cpus = request
        .cpus_per_task
        .map(|per_task| resolve_cpus(per_task, placement.tasks_on_node, job.cpus))
        .transpose()?;
    let memory_mb = request
        .memory_mb
        .map(|mb| resolve_memory(mb, job.memory_mb))
        .transpose()?;
    let gpus = resolve_gpu_count(request, placement)?;
    if let Some(count) = gpus {
        if count > job.gpus.len() {
            return Err(format!(
                "the step asks for {count} GPUs on this node but its job holds {}",
                job.gpus.len()
            ));
        }
    }
    Ok(NodeRequest {
        cpus,
        memory_mb,
        gpus,
    })
}

fn resolve_cpus(per_task: u32, tasks_on_node: u32, job_cpus: u32) -> Result<u32, String> {
    if per_task == 0 {
        return Err("--cpus-per-task must be at least 1".into());
    }
    let cpus = per_task.saturating_mul(tasks_on_node.max(1));
    if job_cpus > 0 && cpus > job_cpus {
        return Err(format!(
            "the step asks for {cpus} CPUs on this node but its job holds {job_cpus}"
        ));
    }
    Ok(cpus)
}

fn resolve_memory(memory_mb: u64, job_memory_mb: u64) -> Result<u64, String> {
    if memory_mb == 0 {
        return Err("--mem must be greater than zero".into());
    }
    if job_memory_mb > 0 && memory_mb > job_memory_mb {
        return Err(format!(
            "the step asks for {memory_mb} MiB on this node but its job holds {job_memory_mb} MiB"
        ));
    }
    Ok(memory_mb)
}

fn resolve_gpu_count(
    request: &StepResources,
    placement: StepPlacement,
) -> Result<Option<usize>, String> {
    let forms = [
        request.gpus.is_some(),
        request.gpus_per_node.is_some(),
        request.gpus_per_task.is_some(),
    ];
    if forms.iter().filter(|set| **set).count() > 1 {
        return Err("only one of --gpus, --gpus-per-node, --gpus-per-task may be set".into());
    }
    let per_node = if let Some(per_node) = request.gpus_per_node {
        per_node
    } else if let Some(per_task) = request.gpus_per_task {
        per_task.saturating_mul(placement.tasks_on_node.max(1))
    } else if let Some(total) = request.gpus {
        let nodes = placement.step_nodes.max(1);
        if total % nodes != 0 {
            return Err(format!(
                "--gpus={total} does not split evenly across the step's {nodes} nodes; \
                 use --gpus-per-node"
            ));
        }
        total / nodes
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
pub type StepGpuKey = (u32, u32, StepId);

/// Not enough of the job's GPUs are free for the step.
#[derive(Debug, PartialEq, Eq)]
pub struct GpusExhausted {
    pub wanted: usize,
    pub free: usize,
    pub total: usize,
}

impl std::fmt::Display for GpusExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the step asks for {} GPUs but only {} of its job's {} GPUs on this node are not \
             held by other steps",
            self.wanted, self.free, self.total
        )
    }
}

/// GPUs that launching steps have picked but not yet handed to a supervisor,
/// whose descriptor holds them from then on.
#[derive(Clone, Debug, Default)]
pub struct PendingStepGpus(Arc<Mutex<HashMap<StepGpuKey, Vec<u64>>>>);

impl PendingStepGpus {
    /// Take the first `count` of `job_gpus`, in order, that neither `held` nor
    /// another pending launch of the same job attempt has. The caller must keep
    /// the source of `held` locked across this call, so no launch can move its
    /// GPUs from pending to held unseen.
    pub fn reserve(
        &self,
        key: StepGpuKey,
        job_gpus: &[u64],
        held: impl IntoIterator<Item = u64>,
        count: usize,
    ) -> Result<StepGpuReservation, GpusExhausted> {
        let mut pending = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let (job_id, run_attempt, _) = key;
        let mut taken: HashSet<u64> = held.into_iter().collect();
        for (other, gpus) in pending.iter() {
            if other.0 == job_id && other.1 == run_attempt && *other != key {
                taken.extend(gpus.iter().copied());
            }
        }
        let free: Vec<u64> = job_gpus
            .iter()
            .copied()
            .filter(|gpu| !taken.contains(gpu))
            .collect();
        if free.len() < count {
            return Err(GpusExhausted {
                wanted: count,
                free: free.len(),
                total: job_gpus.len(),
            });
        }
        let gpus = free[..count].to_vec();
        pending.insert(key, gpus.clone());
        Ok(StepGpuReservation {
            pending: self.clone(),
            key,
            gpus,
        })
    }
}

/// Keeps a launching step's GPUs out of other steps' reach until dropped.
#[derive(Debug)]
pub struct StepGpuReservation {
    pending: PendingStepGpus,
    key: StepGpuKey,
    gpus: Vec<u64>,
}

impl StepGpuReservation {
    pub fn gpus(&self) -> &[u64] {
        &self.gpus
    }
}

impl Drop for StepGpuReservation {
    fn drop(&mut self) {
        let mut pending = self.pending.0.lock().unwrap_or_else(|e| e.into_inner());
        pending.remove(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(gpus: &[u64]) -> JobShare<'_> {
        JobShare {
            cpus: 16,
            memory_mb: 65536,
            gpus,
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
            gpus: &[],
        };
        let resolved = resolve(&request, ONE_NODE_TWO_TASKS, &unbounded).unwrap();
        assert_eq!(resolved.cpus, Some(128));
        assert_eq!(resolved.memory_mb, Some(1 << 20));
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

    #[test]
    fn reservations_skip_held_and_pending_gpus_of_the_same_job_attempt() {
        let pending = PendingStepGpus::default();
        let job_gpus = [10, 11, 12, 13];
        let first = pending.reserve((7, 0, 1), &job_gpus, [10], 1).unwrap();
        assert_eq!(first.gpus(), [11]);
        let second = pending.reserve((7, 0, 2), &job_gpus, [10], 2).unwrap();
        assert_eq!(second.gpus(), [12, 13]);
        assert_eq!(
            pending.reserve((7, 0, 3), &job_gpus, [10], 1).err(),
            Some(GpusExhausted {
                wanted: 1,
                free: 0,
                total: 4
            })
        );
    }

    #[test]
    fn another_job_or_attempt_does_not_hold_this_ones_gpus() {
        let pending = PendingStepGpus::default();
        let _other_job = pending.reserve((8, 0, 1), &[10, 11], [], 2).unwrap();
        let _other_attempt = pending.reserve((7, 1, 1), &[10, 11], [], 2).unwrap();
        let mine = pending.reserve((7, 0, 1), &[10, 11], [], 2).unwrap();
        assert_eq!(mine.gpus(), [10, 11]);
    }

    #[test]
    fn dropping_a_reservation_frees_its_gpus() {
        let pending = PendingStepGpus::default();
        let held = pending.reserve((7, 0, 1), &[10], [], 1).unwrap();
        assert!(pending.reserve((7, 0, 2), &[10], [], 1).is_err());
        drop(held);
        assert_eq!(
            pending.reserve((7, 0, 2), &[10], [], 1).unwrap().gpus(),
            [10]
        );
    }

    #[test]
    fn a_zero_gpu_reservation_always_succeeds_and_holds_nothing() {
        let pending = PendingStepGpus::default();
        let _all = pending.reserve((7, 0, 1), &[10], [], 1).unwrap();
        let none = pending.reserve((7, 0, 2), &[10], [], 0).unwrap();
        assert!(none.gpus().is_empty());
    }
}
