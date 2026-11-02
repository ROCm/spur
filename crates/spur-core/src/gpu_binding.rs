// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::resource::{DeviceTopology, GpuResource, ResourceAllocations, ResourceSet};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const GPU_PROPERTIES_CAPABILITY: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingCheck {
    Dispatch,
    Step,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundGpu {
    pub stable_id: u64,
    pub gpu_type: String,
    pub topology: Option<DeviceTopology>,
}

impl BoundGpu {
    /// Disagreement between devices is block health, checked at placement, not part of
    /// one device's binding.
    fn device_facts(&self) -> Self {
        let mut facts = self.clone();
        if let Some(fabric) = facts.topology.as_mut().and_then(|t| t.fabric.as_mut()) {
            *fabric = fabric.without_peer_conflicts();
        }
        facts
    }

    /// Keeps only this device's own entry in shared endpoint lists, so a step is not
    /// fenced by changes to other accelerators.
    fn own_facts(&self) -> Self {
        let mut facts = self.device_facts();
        if let Some(fabric) = facts.topology.as_mut().and_then(|t| t.fabric.as_mut()) {
            let own = fabric.endpoint_id.clone();
            fabric.active_endpoints.retain(|e| Some(e) == own.as_ref());
            fabric.local_endpoints.retain(|e| Some(e) == own.as_ref());
            fabric.physical_capacity = None;
            fabric.fabric_capacity = None;
        }
        facts
    }
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
        self.check(node, inventory, allocation, BindingCheck::Dispatch)
    }

    /// Re-attests the bound devices' own properties without the node-wide generation fence.
    pub fn validate_step(
        &self,
        node: &str,
        inventory: &ResourceSet,
        allocation: &ResourceAllocations,
    ) -> Result<(), String> {
        self.check(node, inventory, allocation, BindingCheck::Step)
    }

    fn check(
        &self,
        node: &str,
        inventory: &ResourceSet,
        allocation: &ResourceAllocations,
        check: BindingCheck,
    ) -> Result<(), String> {
        if node.is_empty()
            || self.node != node
            || inventory.fabric_binding_version < GPU_PROPERTIES_CAPABILITY
        {
            return Err(
                "GPU property binding has an invalid node or unsupported agent capability".into(),
            );
        }
        if self.generation != allocation.generation
            || (check == BindingCheck::Dispatch
                && !binding_generation_matches(self.generation, inventory, allocation))
        {
            return Err("stale GPU property binding inventory generation".into());
        }
        if !binding_gpu_ids_match(allocation, self.gpus.iter().map(|gpu| gpu.stable_id)) {
            return Err("GPU property binding does not match exact allocated devices".into());
        }
        for expected in &self.gpus {
            let mut found = inventory
                .gpus
                .iter()
                .filter(|g| g.stable_id == expected.stable_id);
            let same = |gpu: &GpuResource| match check {
                BindingCheck::Dispatch => {
                    BoundGpu::from(gpu).device_facts() == expected.device_facts()
                }
                BindingCheck::Step => BoundGpu::from(gpu).own_facts() == expected.own_facts(),
            };
            if !found.next().is_some_and(same) || found.next().is_some() {
                return Err("GPU properties changed or the allocated device disappeared".into());
            }
        }
        Ok(())
    }
}

pub(crate) fn binding_generation_matches(
    generation: u64,
    inventory: &ResourceSet,
    allocation: &ResourceAllocations,
) -> bool {
    generation != 0 && generation == inventory.generation && generation == allocation.generation
}

pub(crate) fn binding_gpu_ids_match(
    allocation: &ResourceAllocations,
    bound_ids: impl Iterator<Item = u64>,
) -> bool {
    let devices = allocation
        .devices
        .get("gpu")
        .map(Vec::as_slice)
        .unwrap_or_default();
    let allocated: BTreeSet<_> = devices.iter().map(|device| device.device_id).collect();
    let mut bound = BTreeSet::new();
    for id in bound_ids {
        if !bound.insert(id) {
            return false;
        }
    }
    allocated == bound
        && allocated.len() == devices.len()
        && devices.iter().all(|device| device.count == 1)
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
    fn step_check_ignores_generation_and_vpod_wide_fields_only() {
        use spur_devices::topology::{FabricObservation, ObservationIssueKind, ObservationStatus};
        let (mut inventory, allocation) = fixture();
        inventory.gpus[0].topology.as_mut().unwrap().fabric = Some(FabricObservation {
            status: ObservationStatus::Consistent,
            fabric_domain_id: Some("15".into()),
            endpoint_id: Some("1".into()),
            active_endpoints: vec!["1".into(), "2".into()],
            fabric_capacity: Some(2),
            ..Default::default()
        });
        let binding = GpuBinding::new("n0", &inventory, &allocation).unwrap();
        let mut changed = inventory.clone();
        changed.generation += 1;
        let fabric = changed.gpus[0]
            .topology
            .as_mut()
            .unwrap()
            .fabric
            .as_mut()
            .unwrap();
        fabric.active_endpoints = vec!["1".into()];
        fabric.fabric_capacity = Some(1);
        assert!(binding.validate("n0", &changed, &allocation).is_err());
        binding.validate_step("n0", &changed, &allocation).unwrap();
        let mut peer = inventory.clone();
        let fabric = peer.gpus[0]
            .topology
            .as_mut()
            .unwrap()
            .fabric
            .as_mut()
            .unwrap();
        fabric.status = ObservationStatus::Inconsistent;
        fabric.add_issue("vpod_size", ObservationIssueKind::PeerConflict);
        binding.validate("n0", &peer, &allocation).unwrap();
        binding.validate_step("n0", &peer, &allocation).unwrap();
        for mutate in 0..5 {
            let mut own = changed.clone();
            let topology = own.gpus[0].topology.as_mut().unwrap();
            let fabric = topology.fabric.as_mut().unwrap();
            match mutate {
                0 => fabric.fabric_domain_id = Some("16".into()),
                1 => fabric.status = ObservationStatus::Unavailable,
                2 => fabric.active_endpoints = vec!["2".into()],
                3 => fabric.add_issue("accel_id", ObservationIssueKind::Conflict),
                _ => topology.numa_node = Some(1),
            }
            assert!(binding.validate_step("n0", &own, &allocation).is_err());
        }
    }

    #[test]
    fn frozen_legacy_allocation_decodes_without_gpu_binding() {
        let old = r#"{"cpus":1,"memory_mb":32,"devices":{"gpu":[{"device_id":128,"count":1}]},"generation":7}"#;
        let allocation: ResourceAllocations = serde_json::from_str(old).unwrap();
        assert!(allocation.gpu_binding.is_none());
    }
}
