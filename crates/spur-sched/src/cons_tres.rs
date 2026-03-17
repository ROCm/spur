//! Consumable TRES resource selection.
//!
//! Tracks resources at core/socket/GPU granularity within a node.
//! This is the equivalent of Slurm's select/cons_tres plugin.

use spur_core::resource::{GpuResource, ResourceSet};

/// Per-node resource allocation state.
/// Tracks which specific cores and GPUs are allocated.
#[derive(Debug, Clone)]
pub struct NodeAllocation {
    pub node_name: String,
    /// Total CPUs on this node.
    pub total_cpus: u32,
    /// Bitmap of allocated CPUs (bit N = core N).
    pub allocated_cpus: Vec<bool>,
    /// Total memory in MB.
    pub total_memory_mb: u64,
    /// Allocated memory in MB.
    pub allocated_memory_mb: u64,
    /// GPU allocations: device_id → allocated?
    pub gpu_allocated: Vec<bool>,
    /// GPU info for type matching.
    pub gpus: Vec<GpuResource>,
}

impl NodeAllocation {
    pub fn new(name: String, resources: &ResourceSet) -> Self {
        let num_gpus = resources.gpus.len();
        Self {
            node_name: name,
            total_cpus: resources.cpus,
            allocated_cpus: vec![false; resources.cpus as usize],
            total_memory_mb: resources.memory_mb,
            allocated_memory_mb: 0,
            gpu_allocated: vec![false; num_gpus],
            gpus: resources.gpus.clone(),
        }
    }

    /// Available (unallocated) CPU count.
    pub fn free_cpus(&self) -> u32 {
        self.allocated_cpus.iter().filter(|&&a| !a).count() as u32
    }

    /// Available memory.
    pub fn free_memory_mb(&self) -> u64 {
        self.total_memory_mb.saturating_sub(self.allocated_memory_mb)
    }

    /// Available GPU count (optionally filtered by type).
    pub fn free_gpus(&self, gpu_type: Option<&str>) -> u32 {
        self.gpu_allocated
            .iter()
            .enumerate()
            .filter(|(i, allocated)| {
                if **allocated {
                    return false;
                }
                if let Some(gtype) = gpu_type {
                    if gtype != "any" && self.gpus.get(*i).map(|g| g.gpu_type.as_str()) != Some(gtype) {
                        return false;
                    }
                }
                true
            })
            .count() as u32
    }

    /// Try to allocate resources. Returns allocated core IDs and GPU IDs, or None if insufficient.
    pub fn try_allocate(
        &mut self,
        cpus: u32,
        memory_mb: u64,
        gpus: u32,
        gpu_type: Option<&str>,
    ) -> Option<AllocationResult> {
        // Check availability
        if self.free_cpus() < cpus
            || self.free_memory_mb() < memory_mb
            || self.free_gpus(gpu_type) < gpus
        {
            return None;
        }

        // Allocate CPUs (first-fit)
        let mut cpu_ids = Vec::new();
        for (i, allocated) in self.allocated_cpus.iter_mut().enumerate() {
            if !*allocated && cpu_ids.len() < cpus as usize {
                *allocated = true;
                cpu_ids.push(i as u32);
            }
        }

        // Allocate memory
        self.allocated_memory_mb += memory_mb;

        // Allocate GPUs (first-fit with type matching)
        let mut gpu_ids = Vec::new();
        for (i, allocated) in self.gpu_allocated.iter_mut().enumerate() {
            if *allocated || gpu_ids.len() >= gpus as usize {
                continue;
            }
            if let Some(gtype) = gpu_type {
                if gtype != "any" && self.gpus.get(i).map(|g| g.gpu_type.as_str()) != Some(gtype) {
                    continue;
                }
            }
            *allocated = true;
            gpu_ids.push(i as u32);
        }

        Some(AllocationResult {
            cpu_ids,
            gpu_ids,
            memory_mb,
        })
    }

    /// Release previously allocated resources.
    pub fn release(&mut self, alloc: &AllocationResult) {
        for &cpu in &alloc.cpu_ids {
            if let Some(a) = self.allocated_cpus.get_mut(cpu as usize) {
                *a = false;
            }
        }
        self.allocated_memory_mb = self.allocated_memory_mb.saturating_sub(alloc.memory_mb);
        for &gpu in &alloc.gpu_ids {
            if let Some(a) = self.gpu_allocated.get_mut(gpu as usize) {
                *a = false;
            }
        }
    }
}

/// Result of a successful allocation.
#[derive(Debug, Clone)]
pub struct AllocationResult {
    /// Allocated core IDs.
    pub cpu_ids: Vec<u32>,
    /// Allocated GPU device IDs.
    pub gpu_ids: Vec<u32>,
    /// Allocated memory in MB.
    pub memory_mb: u64,
}

impl AllocationResult {
    /// Format CPU IDs as a taskset-compatible string.
    pub fn cpu_list(&self) -> String {
        self.cpu_ids
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Format GPU IDs as ROCR_VISIBLE_DEVICES/CUDA_VISIBLE_DEVICES string.
    pub fn gpu_list(&self) -> String {
        self.gpu_ids
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(",")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_core::resource::GpuLinkType;

    fn make_node(cpus: u32, mem: u64, num_gpus: usize, gpu_type: &str) -> NodeAllocation {
        let gpus: Vec<GpuResource> = (0..num_gpus)
            .map(|i| GpuResource {
                device_id: i as u32,
                gpu_type: gpu_type.into(),
                memory_mb: 192_000,
                peer_gpus: vec![],
                link_type: GpuLinkType::XGMI,
            })
            .collect();

        let resources = ResourceSet {
            cpus,
            memory_mb: mem,
            gpus,
            ..Default::default()
        };

        NodeAllocation::new("node001".into(), &resources)
    }

    #[test]
    fn test_initial_state() {
        let node = make_node(64, 256_000, 8, "mi300x");
        assert_eq!(node.free_cpus(), 64);
        assert_eq!(node.free_memory_mb(), 256_000);
        assert_eq!(node.free_gpus(None), 8);
    }

    #[test]
    fn test_allocate_cpus() {
        let mut node = make_node(64, 256_000, 0, "");
        let alloc = node.try_allocate(16, 0, 0, None).unwrap();
        assert_eq!(alloc.cpu_ids.len(), 16);
        assert_eq!(node.free_cpus(), 48);
    }

    #[test]
    fn test_allocate_gpus() {
        let mut node = make_node(64, 256_000, 8, "mi300x");
        let alloc = node.try_allocate(0, 0, 4, Some("mi300x")).unwrap();
        assert_eq!(alloc.gpu_ids.len(), 4);
        assert_eq!(node.free_gpus(Some("mi300x")), 4);
    }

    #[test]
    fn test_allocate_wrong_gpu_type() {
        let mut node = make_node(64, 256_000, 8, "mi300x");
        let result = node.try_allocate(0, 0, 4, Some("h100"));
        assert!(result.is_none());
    }

    #[test]
    fn test_allocate_any_gpu() {
        let mut node = make_node(64, 256_000, 8, "mi300x");
        let alloc = node.try_allocate(0, 0, 4, Some("any")).unwrap();
        assert_eq!(alloc.gpu_ids.len(), 4);
    }

    #[test]
    fn test_insufficient_resources() {
        let mut node = make_node(32, 128_000, 4, "mi300x");
        assert!(node.try_allocate(64, 0, 0, None).is_none()); // Too many CPUs
        assert!(node.try_allocate(0, 256_000, 0, None).is_none()); // Too much memory
        assert!(node.try_allocate(0, 0, 8, None).is_none()); // Too many GPUs
    }

    #[test]
    fn test_release() {
        let mut node = make_node(64, 256_000, 8, "mi300x");
        let alloc = node.try_allocate(32, 128_000, 4, None).unwrap();
        assert_eq!(node.free_cpus(), 32);
        assert_eq!(node.free_gpus(None), 4);

        node.release(&alloc);
        assert_eq!(node.free_cpus(), 64);
        assert_eq!(node.free_memory_mb(), 256_000);
        assert_eq!(node.free_gpus(None), 8);
    }

    #[test]
    fn test_multiple_allocations() {
        let mut node = make_node(64, 256_000, 8, "mi300x");
        let a1 = node.try_allocate(16, 64_000, 2, None).unwrap();
        let a2 = node.try_allocate(16, 64_000, 2, None).unwrap();
        assert_eq!(node.free_cpus(), 32);
        assert_eq!(node.free_gpus(None), 4);

        // CPU IDs should not overlap
        let overlap: Vec<_> = a1.cpu_ids.iter().filter(|id| a2.cpu_ids.contains(id)).collect();
        assert!(overlap.is_empty());
    }

    #[test]
    fn test_allocation_result_format() {
        let alloc = AllocationResult {
            cpu_ids: vec![0, 1, 2, 3],
            gpu_ids: vec![0, 1],
            memory_mb: 128_000,
        };
        assert_eq!(alloc.cpu_list(), "0,1,2,3");
        assert_eq!(alloc.gpu_list(), "0,1");
    }
}
