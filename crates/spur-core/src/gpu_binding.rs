// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::resource::{DeviceTopology, GpuResource, ResourceAllocations, ResourceSet};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const GPU_PROPERTIES_CAPABILITY: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundGpu {
    pub stable_id: u64,
    pub gpu_type: String,
    pub topology: Option<DeviceTopology>,
}

impl From<&GpuResource> for BoundGpu {
    fn from(gpu: &GpuResource) -> Self {
        Self {
            stable_id: gpu.stable_id,
            gpu_type: gpu.gpu_type.clone(),
            topology: gpu.topology.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuBinding {
    pub node: String,
    pub generation: u64,
    pub gpus: Vec<BoundGpu>,
}

impl GpuBinding {
    pub fn new(
        node: &str,
        inventory: &ResourceSet,
        allocation: &ResourceAllocations,
    ) -> Result<Self, String> {
        let ids: BTreeSet<_> = allocation.device_ids("gpu").into_iter().collect();
        let binding = Self {
            node: node.into(),
            generation: allocation.generation,
            gpus: inventory
                .gpus
                .iter()
                .filter(|gpu| ids.contains(&gpu.stable_id))
                .map(BoundGpu::from)
                .collect(),
        };
        binding.validate(node, inventory, allocation)?;
        Ok(binding)
    }

    pub fn validate(
        &self,
        node: &str,
        inventory: &ResourceSet,
        allocation: &ResourceAllocations,
    ) -> Result<(), String> {
        if node.is_empty()
            || self.node != node
            || inventory.fabric_binding_version < GPU_PROPERTIES_CAPABILITY
        {
            return Err(
                "GPU property binding has an invalid node or unsupported agent capability".into(),
            );
        }
        if self.generation == 0
            || self.generation != inventory.generation
            || self.generation != allocation.generation
        {
            return Err("stale GPU property binding inventory generation".into());
        }
        let devices = allocation
            .devices
            .get("gpu")
            .map(Vec::as_slice)
            .unwrap_or_default();
        let ids: BTreeSet<_> = devices.iter().map(|d| d.device_id).collect();
        let bound: BTreeSet<_> = self.gpus.iter().map(|g| g.stable_id).collect();
        if ids != bound
            || ids.len() != devices.len()
            || bound.len() != self.gpus.len()
            || devices.iter().any(|d| d.count != 1)
        {
            return Err("GPU property binding does not match exact allocated devices".into());
        }
        for expected in &self.gpus {
            let mut found = inventory
                .gpus
                .iter()
                .filter(|g| g.stable_id == expected.stable_id);
            if !found.next().is_some_and(|g| BoundGpu::from(g) == *expected)
                || found.next().is_some()
            {
                return Err("GPU properties changed or the allocated device disappeared".into());
            }
        }
        Ok(())
    }
}

pub fn to_proto(binding: &GpuBinding) -> spur_proto::proto::GpuPropertyBinding {
    spur_proto::proto::GpuPropertyBinding {
        node: binding.node.clone(),
        generation: binding.generation,
        gpus: binding
            .gpus
            .iter()
            .map(|g| spur_proto::proto::BoundGpuProperties {
                stable_id: g.stable_id,
                gpu_type: g.gpu_type.clone(),
                topology: g.topology.as_ref().map(crate::device_topology::to_proto),
            })
            .collect(),
    }
}

pub fn from_proto(binding: spur_proto::proto::GpuPropertyBinding) -> GpuBinding {
    GpuBinding {
        node: binding.node,
        generation: binding.generation,
        gpus: binding
            .gpus
            .into_iter()
            .map(|g| BoundGpu {
                stable_id: g.stable_id,
                gpu_type: g.gpu_type,
                topology: g.topology.map(crate::device_topology::from_proto),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::{AllocatedDevice, GpuLinkType};

    fn fixture() -> (ResourceSet, ResourceAllocations) {
        let inventory = ResourceSet {
            generation: 7,
            fabric_binding_version: GPU_PROPERTIES_CAPABILITY,
            gpus: vec![GpuResource {
                device_id: 0,
                stable_id: 128,
                gpu_type: "test".into(),
                memory_mb: 24,
                peer_gpus: vec![],
                link_type: GpuLinkType::PCIe,
                topology: Some(DeviceTopology {
                    pci_root: Some("pci0001:00".into()),
                    numa_node: Some(0),
                    vram_bytes: Some(24),
                    ..Default::default()
                }),
            }],
            ..Default::default()
        };
        let mut allocation = ResourceAllocations {
            generation: 7,
            ..Default::default()
        };
        allocation
            .devices
            .insert("gpu".into(), vec![AllocatedDevice::injectable(128)]);
        (inventory, allocation)
    }

    #[test]
    fn property_binding_checks_exact_snapshots_and_capability() {
        let (inventory, allocation) = fixture();
        let binding = GpuBinding::new("n0", &inventory, &allocation).unwrap();
        assert_eq!(from_proto(to_proto(&binding)), binding);
        for mutate in [0, 1, 2, 3, 4, 5, 6] {
            let mut changed = inventory.clone();
            match mutate {
                0 => changed.generation += 1,
                1 => changed.fabric_binding_version = 2,
                2 => changed.gpus[0].gpu_type = "changed".into(),
                3 => changed.gpus[0].topology.as_mut().unwrap().pci_root = Some("other".into()),
                4 => changed.gpus[0].topology.as_mut().unwrap().numa_node = None,
                5 => changed.gpus[0].topology.as_mut().unwrap().vram_bytes = Some(0),
                _ => changed.gpus.clear(),
            }
            assert!(binding.validate("n0", &changed, &allocation).is_err());
        }
        assert!(binding.validate("n1", &inventory, &allocation).is_err());
        let mut wrong = allocation.clone();
        wrong
            .devices
            .get_mut("gpu")
            .unwrap()
            .push(AllocatedDevice::injectable(128));
        assert!(binding.validate("n0", &inventory, &wrong).is_err());
        wrong = allocation.clone();
        wrong.generation = 0;
        assert!(binding.validate("n0", &inventory, &wrong).is_err());
    }

    #[test]
    fn frozen_legacy_allocation_decodes_without_gpu_binding() {
        let old = r#"{"cpus":1,"memory_mb":32,"devices":{"gpu":[{"device_id":128,"count":1}]},"generation":7}"#;
        let allocation: ResourceAllocations = serde_json::from_str(old).unwrap();
        assert!(allocation.gpu_binding.is_none());
    }
}
