use spur_core::job::{Job, JobId};
use spur_core::node::Node;
use spur_core::partition::Partition;
use spur_core::reservation::Reservation;

/// An assignment of a job to one or more nodes.
#[derive(Debug, Clone)]
pub struct Assignment {
    pub job_id: JobId,
    pub nodes: Vec<String>,
}

/// Cluster state visible to the scheduler.
pub struct ClusterState<'a> {
    pub nodes: &'a [Node],
    pub partitions: &'a [Partition],
    pub reservations: &'a [Reservation],
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
