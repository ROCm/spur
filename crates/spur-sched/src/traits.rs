// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;

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
