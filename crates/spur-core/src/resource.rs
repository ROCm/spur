// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// GPU interconnect type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GpuLinkType {
    PCIe,
    XGMI,
    NVLink,
}

/// A single GPU resource.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpuResource {
    pub device_id: u32,
    pub gpu_type: String,
    pub memory_mb: u64,
    pub peer_gpus: Vec<u32>,
    pub link_type: GpuLinkType,
}

/// A set of compute resources (node-level inventory).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResourceSet {
    pub cpus: u32,
    pub memory_mb: u64,
    pub gpus: Vec<GpuResource>,
    pub generic: HashMap<String, u64>,
}

/// Tracks consumed resources (per-node or per-job allocation accounting).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResourceAllocations {
    pub cpus: u32,
    pub memory_mb: u64,
    /// Key = gres_name ("gpu", "nic", "bandwidth", ...).
    pub devices: HashMap<String, Vec<AllocatedDevice>>,
}

/// A single allocated device entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AllocatedDevice {
    pub device_id: u32,
    /// 1 for injectable devices (GPU, NIC). >1 for countable (bandwidth, licenses).
    pub count: u64,
}

impl AllocatedDevice {
    pub fn injectable(device_id: u32) -> Self {
        Self {
            device_id,
            count: 1,
        }
    }
}

impl ResourceAllocations {
    pub fn is_empty(&self) -> bool {
        self.cpus == 0 && self.memory_mb == 0 && self.devices.is_empty()
    }

    pub fn has_devices(&self) -> bool {
        self.devices.values().any(|v| !v.is_empty())
    }

    pub fn total_device_count(&self, gres_name: &str) -> u64 {
        self.devices
            .get(gres_name)
            .map(|devs| devs.iter().map(|d| d.count).sum())
            .unwrap_or(0)
    }

    pub fn device_ids(&self, gres_name: &str) -> Vec<u32> {
        self.devices
            .get(gres_name)
            .map(|devs| devs.iter().map(|d| d.device_id).collect())
            .unwrap_or_default()
    }

    pub fn allocated_count(
        &self,
        gres_name: &str,
        device_type: Option<&str>,
        inventory: &ResourceSet,
    ) -> u32 {
        let Some(devs) = self.devices.get(gres_name) else {
            return 0;
        };
        if gres_name == "gpu" {
            devs.iter()
                .filter(|d| {
                    if let Some(gtype) = device_type {
                        if gtype == "any" {
                            return true;
                        }
                        inventory
                            .gpus
                            .iter()
                            .find(|g| g.device_id == d.device_id)
                            .map(|g| g.gpu_type.as_str())
                            == Some(gtype)
                    } else {
                        true
                    }
                })
                .map(|d| d.count as u32)
                .sum()
        } else {
            devs.iter().map(|d| d.count as u32).sum()
        }
    }

    pub fn generic_count(&self, name: &str) -> u64 {
        self.devices
            .get(name)
            .map(|devs| devs.iter().map(|d| d.count).sum())
            .unwrap_or(0)
    }

    pub fn add(&mut self, other: &ResourceAllocations) {
        self.cpus = self.cpus.saturating_add(other.cpus);
        self.memory_mb = self.memory_mb.saturating_add(other.memory_mb);
        for (gres_name, devices) in &other.devices {
            let entry = self.devices.entry(gres_name.clone()).or_default();
            for dev in devices {
                if let Some(existing) = entry.iter_mut().find(|d| d.device_id == dev.device_id) {
                    existing.count = existing.count.saturating_add(dev.count);
                } else {
                    entry.push(dev.clone());
                }
            }
        }
    }

    pub fn subtract(&mut self, other: &ResourceAllocations) {
        self.cpus = self.cpus.saturating_sub(other.cpus);
        self.memory_mb = self.memory_mb.saturating_sub(other.memory_mb);
        for (gres_name, devices) in &other.devices {
            let Some(entry) = self.devices.get_mut(gres_name) else {
                continue;
            };
            for dev in devices {
                if let Some(existing) = entry.iter_mut().find(|d| d.device_id == dev.device_id) {
                    existing.count = existing.count.saturating_sub(dev.count);
                }
            }
            entry.retain(|d| d.count > 0);
            if entry.is_empty() {
                self.devices.remove(gres_name);
            }
        }
    }

    pub fn from_device_ids(gres_name: impl Into<String>, device_ids: &[u32]) -> Self {
        let mut alloc = ResourceAllocations::default();
        if !device_ids.is_empty() {
            alloc.devices.insert(
                gres_name.into(),
                device_ids
                    .iter()
                    .map(|&id| AllocatedDevice::injectable(id))
                    .collect(),
            );
        }
        alloc
    }

    pub fn with_scalar(cpus: u32, memory_mb: u64) -> Self {
        Self {
            cpus,
            memory_mb,
            devices: HashMap::new(),
        }
    }
}

impl ResourceSet {
    /// Check if this inventory can satisfy a count-based request with no prior allocation.
    pub fn can_satisfy(&self, request: &ResourceSet) -> bool {
        self.can_satisfy_with_allocated(&ResourceAllocations::default(), request)
    }

    /// Check if available resources (inventory minus allocated) can satisfy a request.
    pub fn can_satisfy_with_allocated(
        &self,
        allocated: &ResourceAllocations,
        request: &ResourceSet,
    ) -> bool {
        let avail_cpus = self.cpus.saturating_sub(allocated.cpus);
        let avail_mem = self.memory_mb.saturating_sub(allocated.memory_mb);
        if avail_cpus < request.cpus || avail_mem < request.memory_mb {
            return false;
        }

        let mut avail_gpus = self.gpu_counts();
        let total_avail: u32 = avail_gpus.values().sum();
        for dev in allocated.devices.get("gpu").into_iter().flatten() {
            if let Some(gpu) = self.gpus.iter().find(|g| g.device_id == dev.device_id) {
                *avail_gpus.entry(gpu.gpu_type.clone()).or_insert(0) = avail_gpus
                    .get(&gpu.gpu_type)
                    .copied()
                    .unwrap_or(0)
                    .saturating_sub(dev.count as u32);
            }
        }

        let req_gpus = request.gpu_counts();
        for (gpu_type, count) in &req_gpus {
            if gpu_type == "any" {
                if total_avail.saturating_sub(allocated.total_device_count("gpu") as u32) < *count {
                    return false;
                }
            } else if avail_gpus.get(gpu_type).copied().unwrap_or(0) < *count {
                return false;
            }
        }

        for (name, count) in &request.generic {
            let avail = self
                .generic
                .get(name)
                .copied()
                .unwrap_or(0)
                .saturating_sub(allocated.generic_count(name));
            if avail < *count {
                return false;
            }
        }
        true
    }

    /// Return unallocated device IDs for an injectable gres type.
    pub fn available_device_ids(
        &self,
        allocated: &ResourceAllocations,
        gres_name: &str,
        device_type: Option<&str>,
    ) -> Vec<u32> {
        let allocated_ids: std::collections::HashSet<u32> = allocated
            .devices
            .get(gres_name)
            .map(|devs| devs.iter().map(|d| d.device_id).collect())
            .unwrap_or_default();

        if gres_name == "gpu" {
            self.gpus
                .iter()
                .filter(|g| {
                    if allocated_ids.contains(&g.device_id) {
                        return false;
                    }
                    match device_type {
                        None | Some("any") => true,
                        Some(t) => g.gpu_type == t,
                    }
                })
                .map(|g| g.device_id)
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Pick the first `count` available device IDs and build an allocation fragment.
    pub fn pick_devices(
        &self,
        allocated: &ResourceAllocations,
        gres_name: &str,
        device_type: Option<&str>,
        count: u32,
    ) -> Vec<AllocatedDevice> {
        self.available_device_ids(allocated, gres_name, device_type)
            .into_iter()
            .take(count as usize)
            .map(AllocatedDevice::injectable)
            .collect()
    }

    /// Count GPUs by type.
    pub fn gpu_counts(&self) -> HashMap<String, u32> {
        let mut counts = HashMap::new();
        for gpu in &self.gpus {
            *counts.entry(gpu.gpu_type.clone()).or_insert(0) += 1;
        }
        counts
    }

    pub fn total_gpus(&self) -> u32 {
        self.gpus.len() as u32
    }
}

/// Parse a GRES string like "gpu:mi300x:4" or "gpu:2".
///
/// The input is trimmed and empty strings are rejected, so callers that split a
/// comma-list (CLI `--gres`, `SLURM_GRES`, REST, FFI) can pass raw segments
/// without worrying about incidental whitespace or trailing commas.
pub fn parse_gres(gres: &str) -> Option<(String, Option<String>, u32)> {
    let gres = gres.trim();
    if gres.is_empty() {
        return None;
    }
    let parts: Vec<&str> = gres.split(':').collect();
    match parts.len() {
        1 => Some((parts[0].to_string(), None, 1)),
        2 => {
            if let Ok(count) = parts[1].parse::<u32>() {
                Some((parts[0].to_string(), None, count))
            } else {
                Some((parts[0].to_string(), Some(parts[1].to_string()), 1))
            }
        }
        3 => {
            let count = parts[2].parse::<u32>().ok()?;
            Some((parts[0].to_string(), Some(parts[1].to_string()), count))
        }
        _ => None,
    }
}

/// Build a full-node allocation for exclusive jobs.
pub fn build_exclusive_allocation(inventory: &ResourceSet, memory_mb: u64) -> ResourceAllocations {
    let mut alloc = ResourceAllocations::with_scalar(inventory.cpus, memory_mb);
    if !inventory.gpus.is_empty() {
        alloc.devices.insert(
            "gpu".into(),
            inventory
                .gpus
                .iter()
                .map(|g| AllocatedDevice::injectable(g.device_id))
                .collect(),
        );
    }
    for (name, count) in &inventory.generic {
        if *count > 0 {
            alloc
                .devices
                .entry(name.clone())
                .or_default()
                .push(AllocatedDevice {
                    device_id: 0,
                    count: *count,
                });
        }
    }
    alloc
}

/// Sum per-node allocations into a job-level total.
pub fn aggregate_allocations(
    allocs: impl IntoIterator<Item = ResourceAllocations>,
) -> ResourceAllocations {
    let mut total = ResourceAllocations::default();
    for a in allocs {
        total.add(&a);
    }
    total
}

/// Build per-node allocation with real device IDs from inventory.
pub fn build_node_allocation(
    inventory: &ResourceSet,
    current_alloc: &ResourceAllocations,
    request: &ResourceSet,
) -> ResourceAllocations {
    let mut alloc = ResourceAllocations::with_scalar(request.cpus, request.memory_mb);

    let req_gpus = request.gpu_counts();
    for (gpu_type, count) in req_gpus {
        let dtype = if gpu_type == "any" {
            None
        } else {
            Some(gpu_type.as_str())
        };
        let mut effective_alloc = current_alloc.clone();
        effective_alloc.add(&alloc);
        let picked = inventory.pick_devices(&effective_alloc, "gpu", dtype, count);
        alloc
            .devices
            .entry("gpu".into())
            .or_default()
            .extend(picked);
    }

    for (name, count) in &request.generic {
        if *count > 0 {
            alloc
                .devices
                .entry(name.clone())
                .or_default()
                .push(AllocatedDevice {
                    device_id: 0,
                    count: *count,
                });
        }
    }

    alloc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_inventory() -> ResourceSet {
        ResourceSet {
            cpus: 64,
            memory_mb: 256_000,
            gpus: vec![
                GpuResource {
                    device_id: 0,
                    gpu_type: "mi300x".into(),
                    memory_mb: 192_000,
                    peer_gpus: vec![1],
                    link_type: GpuLinkType::XGMI,
                },
                GpuResource {
                    device_id: 1,
                    gpu_type: "mi300x".into(),
                    memory_mb: 192_000,
                    peer_gpus: vec![0],
                    link_type: GpuLinkType::XGMI,
                },
            ],
            generic: HashMap::new(),
        }
    }

    #[test]
    fn test_can_satisfy() {
        let avail = sample_inventory();
        let req = ResourceSet {
            cpus: 32,
            memory_mb: 128_000,
            gpus: vec![GpuResource {
                device_id: 0,
                gpu_type: "mi300x".into(),
                memory_mb: 0,
                peer_gpus: vec![],
                link_type: GpuLinkType::XGMI,
            }],
            generic: HashMap::new(),
        };
        assert!(avail.can_satisfy(&req));
    }

    #[test]
    fn test_can_satisfy_with_allocated() {
        let inventory = sample_inventory();
        let mut allocated = ResourceAllocations::with_scalar(0, 0);
        allocated
            .devices
            .insert("gpu".into(), vec![AllocatedDevice::injectable(0)]);

        let req_one = ResourceSet {
            cpus: 1,
            memory_mb: 1,
            gpus: vec![GpuResource {
                device_id: 0,
                gpu_type: "mi300x".into(),
                memory_mb: 0,
                peer_gpus: vec![],
                link_type: GpuLinkType::XGMI,
            }],
            generic: HashMap::new(),
        };
        assert!(inventory.can_satisfy_with_allocated(&allocated, &req_one));

        let req_two = ResourceSet {
            gpus: vec![
                GpuResource {
                    device_id: 0,
                    gpu_type: "mi300x".into(),
                    memory_mb: 0,
                    peer_gpus: vec![],
                    link_type: GpuLinkType::XGMI,
                },
                GpuResource {
                    device_id: 1,
                    gpu_type: "mi300x".into(),
                    memory_mb: 0,
                    peer_gpus: vec![],
                    link_type: GpuLinkType::XGMI,
                },
            ],
            ..Default::default()
        };
        assert!(!inventory.can_satisfy_with_allocated(&allocated, &req_two));
    }

    #[test]
    fn test_allocations_add_subtract() {
        let mut a = ResourceAllocations::with_scalar(4, 1024);
        a.devices
            .insert("gpu".into(), vec![AllocatedDevice::injectable(0)]);

        let b = ResourceAllocations::with_scalar(2, 512);
        a.add(&b);
        assert_eq!(a.cpus, 6);
        assert_eq!(a.memory_mb, 1536);

        a.subtract(&b);
        assert_eq!(a.cpus, 4);
        assert_eq!(a.memory_mb, 1024);
        assert_eq!(a.device_ids("gpu"), vec![0]);
    }

    #[test]
    fn test_available_device_ids() {
        let inventory = sample_inventory();
        let allocated = ResourceAllocations::from_device_ids("gpu", &[0]);
        let free = inventory.available_device_ids(&allocated, "gpu", Some("mi300x"));
        assert_eq!(free, vec![1]);
    }

    #[test]
    fn test_build_node_allocation() {
        let inventory = sample_inventory();
        let request = ResourceSet {
            cpus: 8,
            memory_mb: 16_000,
            gpus: vec![GpuResource {
                device_id: 0,
                gpu_type: "mi300x".into(),
                memory_mb: 0,
                peer_gpus: vec![],
                link_type: GpuLinkType::XGMI,
            }],
            generic: HashMap::new(),
        };
        let alloc = build_node_allocation(&inventory, &ResourceAllocations::default(), &request);
        assert_eq!(alloc.cpus, 8);
        assert_eq!(alloc.device_ids("gpu"), vec![0]);
    }

    #[test]
    fn test_build_node_allocation_multi_type() {
        let inventory = ResourceSet {
            cpus: 64,
            memory_mb: 256_000,
            gpus: vec![
                GpuResource {
                    device_id: 0,
                    gpu_type: "mi300x".into(),
                    memory_mb: 192_000,
                    peer_gpus: vec![],
                    link_type: GpuLinkType::XGMI,
                },
                GpuResource {
                    device_id: 1,
                    gpu_type: "mi300x".into(),
                    memory_mb: 192_000,
                    peer_gpus: vec![],
                    link_type: GpuLinkType::XGMI,
                },
                GpuResource {
                    device_id: 2,
                    gpu_type: "h100".into(),
                    memory_mb: 80_000,
                    peer_gpus: vec![],
                    link_type: GpuLinkType::NVLink,
                },
                GpuResource {
                    device_id: 3,
                    gpu_type: "h100".into(),
                    memory_mb: 80_000,
                    peer_gpus: vec![],
                    link_type: GpuLinkType::NVLink,
                },
            ],
            generic: HashMap::new(),
        };
        let request = ResourceSet {
            cpus: 8,
            memory_mb: 16_000,
            gpus: vec![
                GpuResource {
                    device_id: 0,
                    gpu_type: "mi300x".into(),
                    memory_mb: 0,
                    peer_gpus: vec![],
                    link_type: GpuLinkType::XGMI,
                },
                GpuResource {
                    device_id: 1,
                    gpu_type: "h100".into(),
                    memory_mb: 0,
                    peer_gpus: vec![],
                    link_type: GpuLinkType::NVLink,
                },
            ],
            generic: HashMap::new(),
        };
        let alloc = build_node_allocation(&inventory, &ResourceAllocations::default(), &request);
        let mut ids = alloc.device_ids("gpu");
        ids.sort();
        assert_eq!(ids, vec![0, 2]);
    }

    #[test]
    fn test_parse_gres() {
        assert_eq!(
            parse_gres("gpu:mi300x:4"),
            Some(("gpu".into(), Some("mi300x".into()), 4))
        );
        assert_eq!(parse_gres("gpu:2"), Some(("gpu".into(), None, 2)));
        assert_eq!(parse_gres("license"), Some(("license".into(), None, 1)));
    }

    #[test]
    fn test_parse_gres_trims_and_rejects_empty() {
        assert_eq!(parse_gres(" gpu:2 "), Some(("gpu".into(), None, 2)));
        assert_eq!(parse_gres(""), None);
        assert_eq!(parse_gres("   "), None);
    }
}
