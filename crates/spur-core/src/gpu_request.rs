// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! GPU request modeling and placement.
//!
//! Spur distinguishes three user-facing GPU requests, matching Slurm:
//!
//! - `--gpus=N` (`-G`): N GPUs *total* across the job, distributed across the
//!   job's nodes by availability (minimum one per node).
//! - `--gpus-per-node=K`: K GPUs on *every* node.
//! - `--gpus-per-task=K`: K GPUs per task; the per-node count follows the task
//!   layout.
//!
//! [`resolve_gpu_demand`] collapses a [`JobSpec`] into a single [`GpuDemand`]
//! the scheduler can act on, and [`distribute_total`] performs the greedy,
//! availability-aware placement for the total case.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::job::JobSpec;
use crate::step::{distribute_tasks, TaskDistribution};

/// A GPU resource request. The meaning of `count` depends on which `JobSpec`
/// field holds it (total, per-node, or per-task).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuRequest {
    pub count: u32,
    /// Optional GPU type filter (e.g. "mi300x"). `None` means "any".
    pub gpu_type: Option<String>,
}

impl GpuRequest {
    pub fn new(count: u32, gpu_type: Option<String>) -> Self {
        Self { count, gpu_type }
    }

    /// Parse a GPU flag value like `"4"` or `"mi300x:4"` into a `GpuRequest`.
    /// Returns `Ok(None)` for count 0 or empty/blank input (treated as "no request").
    pub fn parse_flag(value: &str) -> Result<Option<Self>, GpuRequestError> {
        let value = value.trim();
        if value.is_empty() {
            return Ok(None);
        }
        let spec_str = format!("gpu:{value}");
        let (_, gpu_type, count) =
            crate::resource::parse_gres(&spec_str).ok_or_else(|| GpuRequestError::InvalidSpec {
                value: value.to_string(),
            })?;
        if count == 0 {
            return Ok(None);
        }
        Ok(Some(Self { count, gpu_type }))
    }

    /// Convert a proto `GpuRequest` into the core form. Zero count or missing
    /// message yields `None`.
    pub fn from_proto(proto: Option<spur_proto::proto::GpuRequest>) -> Option<Self> {
        proto.and_then(|g| {
            if g.count == 0 {
                return None;
            }
            Some(Self {
                count: g.count,
                gpu_type: if g.gpu_type.is_empty() {
                    None
                } else {
                    Some(g.gpu_type)
                },
            })
        })
    }
}

impl From<&GpuRequest> for spur_proto::proto::GpuRequest {
    fn from(r: &GpuRequest) -> Self {
        spur_proto::proto::GpuRequest {
            count: r.count,
            gpu_type: r.gpu_type.clone().unwrap_or_default(),
        }
    }
}

/// The resolved GPU demand for a job, ready for the scheduler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpuDemand {
    /// No GPUs requested.
    None,
    /// A fixed per-node count vector (length == num_nodes). Used for
    /// `--gpus-per-node` (all equal) and `--gpus-per-task` (follows task
    /// layout, may be uneven).
    PerNode {
        counts: Vec<u32>,
        gpu_type: Option<String>,
    },
    /// A job total to distribute across the job's nodes by availability.
    Total {
        count: u32,
        gpu_type: Option<String>,
    },
}

impl GpuDemand {
    /// Total GPUs across the whole job.
    pub fn total(&self) -> u64 {
        match self {
            GpuDemand::None => 0,
            GpuDemand::PerNode { counts, .. } => counts.iter().map(|&c| c as u64).sum(),
            GpuDemand::Total { count, .. } => *count as u64,
        }
    }

    pub fn gpu_type(&self) -> Option<&str> {
        match self {
            GpuDemand::None => None,
            GpuDemand::PerNode { gpu_type, .. } | GpuDemand::Total { gpu_type, .. } => {
                gpu_type.as_deref()
            }
        }
    }

    pub fn is_none(&self) -> bool {
        matches!(self, GpuDemand::None)
    }
}

/// Errors surfaced at submit time for an invalid GPU request.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GpuRequestError {
    #[error("only one GPU request form (gpus, gpus_per_node, gpus_per_task, or a gpu gres entry) may be set")]
    Conflict,
    #[error("--gpus={total} is less than the number of nodes ({nodes}); each node needs at least one GPU")]
    TotalLessThanNodes { total: u32, nodes: u32 },
    #[error("--gpus-per-task requires at least one task")]
    PerTaskNeedsTasks,
    #[error("invalid GPU specification: {value}")]
    InvalidSpec { value: String },
}

/// Number of tasks placed on each of `num_nodes` nodes.
///
/// Prefers an explicit `tasks_per_node` (uniform); otherwise derives the
/// per-node counts from the task distribution policy.
fn tasks_per_node_counts(spec: &JobSpec, num_nodes: u32) -> Vec<u32> {
    if let Some(tpn) = spec.tasks_per_node {
        return vec![tpn; num_nodes as usize];
    }
    let dist = spec
        .distribution
        .as_deref()
        .map(|d| d.parse::<TaskDistribution>().unwrap_or_default())
        .unwrap_or_default();
    let mapping = distribute_tasks(spec.num_tasks, num_nodes, dist);
    let mut counts = vec![0u32; num_nodes as usize];
    for node in mapping {
        if let Some(slot) = counts.get_mut(node as usize) {
            *slot += 1;
        }
    }
    counts
}

/// Parse the per-node GPU count carried in `gres` (e.g. "gpu:mi300x:4"),
/// returning the count and optional type. Sums multiple gpu entries.
/// Errors if two entries specify different named GPU types.
fn gres_gpu(spec: &JobSpec) -> Result<Option<(u32, Option<String>)>, GpuRequestError> {
    let mut count = 0u32;
    let mut gpu_type: Option<String> = None;
    for g in &spec.gres {
        if let Some((name, gtype, c)) = crate::resource::parse_gres(g) {
            if name == "gpu" {
                count += c;
                if let Some(t) = gtype {
                    if let Some(ref existing) = gpu_type {
                        if existing != &t {
                            return Err(GpuRequestError::Conflict);
                        }
                    } else {
                        gpu_type = Some(t);
                    }
                }
            }
        }
    }
    Ok((count > 0).then_some((count, gpu_type)))
}

/// Collapse a job's GPU-related fields into a single [`GpuDemand`].
///
/// Enforces that at most one of `gpus` / `gpus_per_node` / `gpus_per_task` is
/// set (a `gpu:` entry in `gres` counts as an implicit `gpus_per_node`), and
/// that `--gpus=N` provides at least one GPU per node.
pub fn resolve_gpu_demand(spec: &JobSpec) -> Result<GpuDemand, GpuRequestError> {
    resolve_gpu_demand_for(spec, spec.num_nodes)
}

/// Like [`resolve_gpu_demand`] but with an explicit node count (avoids cloning
/// a full `JobSpec` when the caller needs to override `num_nodes`).
pub fn resolve_gpu_demand_for(
    spec: &JobSpec,
    num_nodes: u32,
) -> Result<GpuDemand, GpuRequestError> {
    let num_nodes = num_nodes.max(1);
    let gres = gres_gpu(spec)?;

    let explicit = [
        spec.gpus.is_some(),
        spec.gpus_per_node.is_some(),
        spec.gpus_per_task.is_some(),
    ]
    .iter()
    .filter(|&&b| b)
    .count();
    if explicit > 1 {
        return Err(GpuRequestError::Conflict);
    }
    if explicit == 1 && gres.is_some() {
        return Err(GpuRequestError::Conflict);
    }

    if let Some(req) = &spec.gpus {
        if req.count == 0 {
            return Ok(GpuDemand::None);
        }
        if req.count < num_nodes {
            return Err(GpuRequestError::TotalLessThanNodes {
                total: req.count,
                nodes: num_nodes,
            });
        }
        return Ok(GpuDemand::Total {
            count: req.count,
            gpu_type: req.gpu_type.clone(),
        });
    }

    if let Some(req) = &spec.gpus_per_node {
        if req.count == 0 {
            return Ok(GpuDemand::None);
        }
        return Ok(GpuDemand::PerNode {
            counts: vec![req.count; num_nodes as usize],
            gpu_type: req.gpu_type.clone(),
        });
    }

    if let Some(req) = &spec.gpus_per_task {
        if req.count == 0 {
            return Ok(GpuDemand::None);
        }
        if spec.num_tasks == 0 {
            return Err(GpuRequestError::PerTaskNeedsTasks);
        }
        let counts = tasks_per_node_counts(spec, num_nodes)
            .into_iter()
            .map(|t| t * req.count)
            .collect();
        return Ok(GpuDemand::PerNode {
            counts,
            gpu_type: req.gpu_type.clone(),
        });
    }

    if let Some((count, gpu_type)) = gres {
        return Ok(GpuDemand::PerNode {
            counts: vec![count; num_nodes as usize],
            gpu_type,
        });
    }

    Ok(GpuDemand::None)
}

/// Greedily distribute `total` GPUs across nodes with free capacities `caps`,
/// which MUST be sorted in descending order by the caller.
///
/// Each node takes as many GPUs as it can while reserving exactly one GPU for
/// every later node: `take = min(caps[i], remaining - nodes_left_after)`. This
/// concentrates GPUs on the most-available nodes and leaves tail nodes at one.
///
/// Returns per-node counts aligned to `caps`, or `None` if no valid assignment
/// exists now (fewer GPUs than nodes, a node with zero free GPUs, or not enough
/// total free capacity).
pub fn distribute_total(total: u32, caps: &[u32]) -> Option<Vec<u32>> {
    let m = caps.len();
    if m == 0 || total < m as u32 {
        return None;
    }
    if caps.contains(&0) {
        return None;
    }
    if caps.iter().map(|&c| c as u64).sum::<u64>() < total as u64 {
        return None;
    }

    let mut give = vec![0u32; m];
    let mut remaining = total;
    for i in 0..m {
        let leave = (m - 1 - i) as u32;
        let take = caps[i].min(remaining - leave);
        give[i] = take;
        remaining -= take;
    }
    // Pre-checks guarantee full placement.
    debug_assert_eq!(remaining, 0);
    Some(give)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_with(num_nodes: u32, num_tasks: u32) -> JobSpec {
        JobSpec {
            num_nodes,
            num_tasks,
            ..Default::default()
        }
    }

    #[test]
    fn distribute_packs_first_node_on_ample_capacity() {
        assert_eq!(distribute_total(8, &[8, 8, 8, 8]), Some(vec![5, 1, 1, 1]));
        assert_eq!(distribute_total(5, &[8, 8]), Some(vec![4, 1]));
    }

    #[test]
    fn distribute_handles_scattered_capacity() {
        assert_eq!(distribute_total(8, &[5, 1, 1, 1]), Some(vec![5, 1, 1, 1]));
    }

    #[test]
    fn distribute_rejects_infeasible() {
        // Fewer GPUs than nodes.
        assert_eq!(distribute_total(3, &[8, 8, 8, 8]), None);
        // A node has zero free GPUs.
        assert_eq!(distribute_total(4, &[8, 0]), None);
        // Not enough total free capacity.
        assert_eq!(distribute_total(10, &[3, 3, 3]), None);
        // No nodes.
        assert_eq!(distribute_total(4, &[]), None);
    }

    #[test]
    fn distribute_exact_capacity() {
        assert_eq!(distribute_total(9, &[3, 3, 3]), Some(vec![3, 3, 3]));
    }

    #[test]
    fn resolve_none_when_no_gpu_request() {
        let spec = spec_with(2, 2);
        assert_eq!(resolve_gpu_demand(&spec).unwrap(), GpuDemand::None);
    }

    #[test]
    fn resolve_total() {
        let mut spec = spec_with(4, 4);
        spec.gpus = Some(GpuRequest::new(8, Some("mi300x".into())));
        assert_eq!(
            resolve_gpu_demand(&spec).unwrap(),
            GpuDemand::Total {
                count: 8,
                gpu_type: Some("mi300x".into())
            }
        );
    }

    #[test]
    fn resolve_total_rejects_fewer_gpus_than_nodes() {
        let mut spec = spec_with(4, 4);
        spec.gpus = Some(GpuRequest::new(2, None));
        assert_eq!(
            resolve_gpu_demand(&spec),
            Err(GpuRequestError::TotalLessThanNodes { total: 2, nodes: 4 })
        );
    }

    #[test]
    fn resolve_per_node_is_homogeneous() {
        let mut spec = spec_with(3, 3);
        spec.gpus_per_node = Some(GpuRequest::new(4, None));
        assert_eq!(
            resolve_gpu_demand(&spec).unwrap(),
            GpuDemand::PerNode {
                counts: vec![4, 4, 4],
                gpu_type: None
            }
        );
    }

    #[test]
    fn resolve_per_task_follows_uniform_layout() {
        let mut spec = spec_with(2, 4);
        spec.tasks_per_node = Some(2);
        spec.gpus_per_task = Some(GpuRequest::new(1, None));
        assert_eq!(
            resolve_gpu_demand(&spec).unwrap(),
            GpuDemand::PerNode {
                counts: vec![2, 2],
                gpu_type: None
            }
        );
    }

    #[test]
    fn resolve_per_task_uneven_block_layout() {
        // 5 tasks over 2 nodes, block => [3, 2] tasks; 1 GPU/task => [3, 2].
        let mut spec = spec_with(2, 5);
        spec.gpus_per_task = Some(GpuRequest::new(1, None));
        assert_eq!(
            resolve_gpu_demand(&spec).unwrap(),
            GpuDemand::PerNode {
                counts: vec![3, 2],
                gpu_type: None
            }
        );
    }

    #[test]
    fn resolve_gres_gpu_is_per_node() {
        let mut spec = spec_with(2, 2);
        spec.gres = vec!["gpu:mi300x:2".into()];
        assert_eq!(
            resolve_gpu_demand(&spec).unwrap(),
            GpuDemand::PerNode {
                counts: vec![2, 2],
                gpu_type: Some("mi300x".into())
            }
        );
    }

    #[test]
    fn resolve_rejects_conflicting_sources() {
        let mut spec = spec_with(2, 2);
        spec.gpus = Some(GpuRequest::new(4, None));
        spec.gpus_per_node = Some(GpuRequest::new(2, None));
        assert_eq!(resolve_gpu_demand(&spec), Err(GpuRequestError::Conflict));

        let mut spec = spec_with(2, 2);
        spec.gpus = Some(GpuRequest::new(4, None));
        spec.gres = vec!["gpu:2".into()];
        assert_eq!(resolve_gpu_demand(&spec), Err(GpuRequestError::Conflict));
    }

    #[test]
    fn resolve_rejects_mixed_gres_gpu_types() {
        let mut spec = spec_with(2, 2);
        spec.gres = vec!["gpu:mi300x:2".into(), "gpu:h100:2".into()];
        assert_eq!(resolve_gpu_demand(&spec), Err(GpuRequestError::Conflict));
    }

    #[test]
    fn resolve_allows_untyped_mixed_with_typed_gres() {
        let mut spec = spec_with(2, 2);
        spec.gres = vec!["gpu:2".into(), "gpu:mi300x:1".into()];
        assert_eq!(
            resolve_gpu_demand(&spec).unwrap(),
            GpuDemand::PerNode {
                counts: vec![3, 3],
                gpu_type: Some("mi300x".into())
            }
        );
    }

    #[test]
    fn parse_flag_count_only() {
        let r = GpuRequest::parse_flag("4").unwrap().unwrap();
        assert_eq!(r, GpuRequest::new(4, None));
    }

    #[test]
    fn parse_flag_typed() {
        let r = GpuRequest::parse_flag("mi300x:4").unwrap().unwrap();
        assert_eq!(r, GpuRequest::new(4, Some("mi300x".into())));
    }

    #[test]
    fn parse_flag_zero_is_none() {
        assert_eq!(GpuRequest::parse_flag("0").unwrap(), None);
        assert_eq!(GpuRequest::parse_flag("mi300x:0").unwrap(), None);
    }

    #[test]
    fn parse_flag_empty_is_none() {
        assert_eq!(GpuRequest::parse_flag("").unwrap(), None);
        assert_eq!(GpuRequest::parse_flag("  ").unwrap(), None);
    }

    #[test]
    fn parse_flag_invalid() {
        assert!(matches!(
            GpuRequest::parse_flag("::invalid"),
            Err(GpuRequestError::InvalidSpec { .. })
        ));
    }

    #[test]
    fn converter_round_trip() {
        let core = GpuRequest::new(8, Some("h100".into()));
        let proto: spur_proto::proto::GpuRequest = (&core).into();
        assert_eq!(proto.count, 8);
        assert_eq!(proto.gpu_type, "h100");

        let back = GpuRequest::from_proto(Some(proto)).unwrap();
        assert_eq!(back, core);
    }

    #[test]
    fn from_proto_zero_is_none() {
        let proto = spur_proto::proto::GpuRequest {
            count: 0,
            gpu_type: String::new(),
        };
        assert_eq!(GpuRequest::from_proto(Some(proto)), None);
        assert_eq!(GpuRequest::from_proto(None), None);
    }
}
