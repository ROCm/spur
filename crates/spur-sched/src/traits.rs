// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use spur_core::job::{Job, JobId};
use spur_core::node::Node;
use spur_core::partition::Partition;
use spur_core::reservation::Reservation;
use spur_core::resource::ResourceAllocations;
use spur_core::topology::TopologyTree;

/// An assignment of a job to one or more nodes.
#[derive(Debug, Clone)]
pub struct Assignment {
    pub job_id: JobId,
    pub nodes: Vec<String>,
    /// Controller-selected per-node resource allocation (real device IDs).
    pub per_node_alloc: HashMap<String, ResourceAllocations>,
}

/// Cluster state visible to the scheduler.
pub struct ClusterState<'a> {
    pub nodes: &'a [Node],
    pub partitions: &'a [Partition],
    pub reservations: &'a [Reservation],
    /// Fabric topology (switch hierarchy). None if topology is not configured.
    pub topology: Option<&'a TopologyTree>,
    /// Node name -> real time its last currently-running job is expected to
    /// end. Absent entries fall back to a conservative placeholder.
    pub busy_until: &'a HashMap<String, DateTime<Utc>>,
    /// Node name -> pending preemptor its victims' released capacity is held for.
    pub preemption_handoffs: &'a HashMap<String, JobId>,
}

impl ClusterState<'_> {
    /// Whether `job` may use `node`; reservation rules already shaped who owns its handoff.
    pub fn handoff_allows(&self, job: &Job, node: &str) -> bool {
        self.preemption_handoffs
            .get(node)
            .is_none_or(|owner| *owner == job.job_id)
    }

    /// An additive `--nodelist` needs every listed node, so one handed-off node blocks the job.
    pub fn listed_node_handed_off(
        &self,
        job: &Job,
        placement: &crate::node_match::NodePlacement,
    ) -> bool {
        !self.preemption_handoffs.is_empty()
            && placement.nodelist_is_additive()
            && self
                .nodes
                .iter()
                .any(|n| placement.is_listed(&n.name) && !self.handoff_allows(job, &n.name))
    }
}

/// Trait for pluggable scheduler implementations.
pub trait Scheduler: Send + Sync {
    /// Given pending jobs and current cluster state, produce assignments.
    ///
    /// Jobs are provided sorted by priority (highest first).
    /// The scheduler should return assignments for jobs it can schedule now.
    fn schedule(&mut self, pending: &[Job], cluster: &ClusterState) -> Vec<Assignment>;

    /// Name of this scheduler plugin.
    fn name(&self) -> &str;
}
