// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use spur_devices::topology::ObservationStatus;

use crate::fabric::{FabricBlock, FabricBlockKey, FabricMember};
use crate::resource::{ResourceAllocations, ResourceSet};

pub const FABRIC_BINDING_VERSION: u32 = 1;
pub const FILE_BINDING_VERSION: u32 = 2;

/// The controller's exact per-node selection, retained for dispatch and steps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FabricBinding {
    pub version: u32,
    pub node: String,
    pub inventory_generation: u64,
    pub key: FabricBlockKey,
    pub active_endpoints: Vec<String>,
    pub members: Vec<FabricMember>,
    #[serde(default)]
    pub file_revision: Option<String>,
    #[serde(default)]
    pub observed_key: Option<FabricBlockKey>,
}

impl FabricBinding {
    pub fn for_allocation(
        block: &FabricBlock,
        node: &str,
        allocation: &ResourceAllocations,
    ) -> Result<Self, String> {
        if !block.issues.is_empty() {
            return Err("fabric block is not consistent".into());
        }
        let ids: BTreeSet<_> = allocation.device_ids("gpu").into_iter().collect();
        let members: Vec<_> = block
            .members
            .iter()
            .filter(|m| m.device.node == node && ids.contains(&m.device.stable_id))
            .cloned()
            .collect();
        if members.len() != ids.len() {
            return Err("allocated GPUs are outside the fabric block".into());
        }
        Ok(Self {
            node: node.into(),
            inventory_generation: allocation.generation,
            version: if block.file_revision.is_some() {
                FILE_BINDING_VERSION
            } else {
                FABRIC_BINDING_VERSION
            },
            file_revision: block.file_revision.clone(),
            observed_key: block.observed_key.clone(),
            key: block.key.clone(),
            active_endpoints: block.active_endpoints.clone(),
            members,
        })
    }

    pub fn validate(
        &self,
        namespace: &str,
        node: &str,
        inventory: &ResourceSet,
        allocation: &ResourceAllocations,
    ) -> Result<(), String> {
        let expected_version = if self.file_revision.is_some() {
            FILE_BINDING_VERSION
        } else {
            FABRIC_BINDING_VERSION
        };
        if self.version != expected_version || inventory.fabric_binding_version < self.version {
            return Err("unsupported fabric binding version".into());
        }
        if self.key.namespace != namespace
            || namespace.is_empty()
            || self.key.physical_domain.is_empty()
            || self.key.fabric_domain.is_empty()
        {
            return Err("invalid fabric binding scope".into());
        }
        if self.node != node || self.inventory_generation != allocation.generation {
            return Err("fabric binding targets a different node or generation".into());
        }
        if allocation.generation == 0 || allocation.generation != inventory.generation {
            return Err("stale fabric binding inventory generation".into());
        }
        if let Some(revision) = &self.file_revision {
            if revision.len() != 64
                || !revision.bytes().all(|b| b.is_ascii_hexdigit())
                || inventory.block_file_revision.as_ref() != Some(revision)
            {
                return Err("stale or invalid block file revision".into());
            }
        } else if self.observed_key.is_some() {
            return Err("native binding cannot override its observed key".into());
        }
        let observed_key = self.observed_key.as_ref().unwrap_or(&self.key);
        if observed_key.namespace != namespace
            || observed_key.physical_domain != self.key.physical_domain
            || observed_key.fabric_domain.is_empty()
        {
            return Err("invalid observed block scope".into());
        }
        let active: BTreeSet<_> = self.active_endpoints.iter().collect();
        if active.is_empty() || active.len() != self.active_endpoints.len() {
            return Err("invalid fabric binding endpoints".into());
        }
        let allocated = allocation
            .devices
            .get("gpu")
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let ids: BTreeSet<_> = allocated.iter().map(|d| d.device_id).collect();
        let member_ids: BTreeSet<_> = self.members.iter().map(|m| m.device.stable_id).collect();
        if allocated.iter().any(|d| d.count != 1)
            || ids.len() != allocated.len()
            || ids != member_ids
            || member_ids.len() != self.members.len()
        {
            return Err("fabric binding does not match exact allocated GPUs".into());
        }
        for member in &self.members {
            if member.device.node != node || member.inventory_generation != allocation.generation {
                return Err("fabric binding has a different node or inventory generation".into());
            }
            let mut matches = inventory
                .gpus
                .iter()
                .filter(|g| g.stable_id == member.device.stable_id);
            let gpu = matches.next().ok_or("fabric binding GPU is missing")?;
            if matches.next().is_some() {
                return Err("fabric binding GPU identity is ambiguous".into());
            }
            if self.file_revision.is_some() && self.observed_key.is_none() {
                if gpu
                    .topology
                    .as_ref()
                    .and_then(|t| t.fabric.as_ref())
                    .is_some()
                    || member.endpoint
                        != crate::block_file::declared_endpoint(node, member.device.stable_id)
                    || !active.contains(&member.endpoint)
                {
                    return Err("declared membership no longer matches local inventory".into());
                }
                continue;
            }
            let observation = gpu
                .topology
                .as_ref()
                .and_then(|t| t.fabric.as_ref())
                .ok_or("fabric observation is missing")?;
            if observation.status != ObservationStatus::Observed
                || !observation.issues.is_empty()
                || observation.physical_domain_id.as_ref() != Some(&observed_key.physical_domain)
                || observation.fabric_domain_id.as_ref() != Some(&observed_key.fabric_domain)
                || observation.endpoint_id.as_ref() != Some(&member.endpoint)
                || !active.contains(&member.endpoint)
                || !observation.local_endpoints.contains(&member.endpoint)
                || observation.active_endpoints.iter().collect::<BTreeSet<_>>() != active
            {
                return Err("fabric observation no longer matches the bound membership".into());
            }
        }
        Ok(())
    }
}

pub fn to_proto(binding: &FabricBinding) -> spur_proto::proto::FabricBinding {
    use spur_proto::proto;
    proto::FabricBinding {
        version: binding.version,
        node: binding.node.clone(),
        inventory_generation: binding.inventory_generation,
        file_revision: binding.file_revision.clone(),
        observed_key: binding
            .observed_key
            .as_ref()
            .map(|k| proto::FabricBlockKey {
                namespace: k.namespace.clone(),
                physical_domain: k.physical_domain.clone(),
                fabric_domain: k.fabric_domain.clone(),
            }),
        key: Some(proto::FabricBlockKey {
            namespace: binding.key.namespace.clone(),
            physical_domain: binding.key.physical_domain.clone(),
            fabric_domain: binding.key.fabric_domain.clone(),
        }),
        active_endpoints: binding.active_endpoints.clone(),
        members: binding
            .members
            .iter()
            .map(|member| proto::FabricMember {
                device: Some(proto::FabricDeviceKey {
                    node: member.device.node.clone(),
                    stable_id: member.device.stable_id,
                }),
                endpoint: member.endpoint.clone(),
                inventory_generation: member.inventory_generation,
            })
            .collect(),
    }
}

pub fn from_proto(binding: spur_proto::proto::FabricBinding) -> FabricBinding {
    use crate::fabric::FabricDeviceKey;
    let key = binding.key.unwrap_or_default();
    FabricBinding {
        version: binding.version,
        node: binding.node.clone(),
        inventory_generation: binding.inventory_generation,
        file_revision: binding.file_revision,
        observed_key: binding.observed_key.map(|k| FabricBlockKey {
            namespace: k.namespace,
            physical_domain: k.physical_domain,
            fabric_domain: k.fabric_domain,
        }),
        key: FabricBlockKey {
            namespace: key.namespace,
            physical_domain: key.physical_domain,
            fabric_domain: key.fabric_domain,
        },
        active_endpoints: binding.active_endpoints,
        members: binding
            .members
            .into_iter()
            .map(|member| {
                let device = member.device.unwrap_or_default();
                FabricMember {
                    device: FabricDeviceKey {
                        node: device.node,
                        stable_id: device.stable_id,
                    },
                    endpoint: member.endpoint,
                    inventory_generation: member.inventory_generation,
                }
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fabric::{FabricDeviceKey, FabricMember};
    use crate::resource::{AllocatedDevice, DeviceTopology, GpuLinkType, GpuResource};
    use spur_devices::topology::FabricObservation;

    fn fixture() -> (FabricBinding, ResourceSet, ResourceAllocations) {
        let inventory = ResourceSet {
            generation: 7,
            fabric_binding_version: 1,
            block_file_revision: None,
            gpus: vec![GpuResource {
                device_id: 0,
                stable_id: 42,
                gpu_type: "test".into(),
                memory_mb: 100,
                peer_gpus: vec![],
                link_type: GpuLinkType::PCIe,
                topology: Some(DeviceTopology {
                    fabric: Some(FabricObservation {
                        source: "test".into(),
                        status: ObservationStatus::Observed,
                        physical_domain_id: Some("rack-a".into()),
                        fabric_domain_id: Some("7".into()),
                        endpoint_id: Some("1".into()),
                        active_endpoints: vec!["1".into()],
                        local_endpoints: vec!["1".into()],
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
            }],
            ..Default::default()
        };
        let allocation = ResourceAllocations {
            generation: 7,
            devices: [("gpu".into(), vec![AllocatedDevice::injectable(42)])].into(),
            ..Default::default()
        };
        let block = FabricBlock {
            file_revision: None,
            observed_key: None,
            key: FabricBlockKey {
                namespace: "test".into(),
                physical_domain: "rack-a".into(),
                fabric_domain: "7".into(),
            },
            members: vec![FabricMember {
                device: FabricDeviceKey {
                    node: "n1".into(),
                    stable_id: 42,
                },
                endpoint: "1".into(),
                inventory_generation: 7,
            }],
            active_endpoints: vec!["1".into()],
            physical_capacity: Some(8),
            fabric_capacity: Some(1),
            issues: vec![],
        };
        (
            FabricBinding::for_allocation(&block, "n1", &allocation).unwrap(),
            inventory,
            allocation,
        )
    }

    #[test]
    fn binds_exact_devices_and_survives_wire_and_persistence() {
        let (binding, inventory, mut allocation) = fixture();
        binding
            .validate("test", "n1", &inventory, &allocation)
            .unwrap();
        assert_eq!(from_proto(to_proto(&binding)), binding);
        allocation.fabric_binding = Some(binding);
        let json = serde_json::to_string(&allocation).unwrap();
        assert_eq!(
            serde_json::from_str::<ResourceAllocations>(&json).unwrap(),
            allocation
        );
    }

    #[test]
    fn old_inventory_and_allocation_remain_readable() {
        let inventory: ResourceSet = serde_json::from_str(
            r#"{"cpus":4,"memory_mb":1024,"gpus":[],"generic":{},"generation":7}"#,
        )
        .unwrap();
        let allocation: ResourceAllocations =
            serde_json::from_str(r#"{"cpus":4,"memory_mb":1024,"devices":{},"generation":7}"#)
                .unwrap();
        assert_eq!(inventory.fabric_binding_version, 0);
        assert!(inventory.block_file_revision.is_none());
        assert!(allocation.fabric_binding.is_none());
        let binding: FabricBinding = serde_json::from_str(r#"{"version":1,"node":"n1","inventory_generation":7,"key":{"namespace":"test","physical_domain":"rack-a","fabric_domain":"7"},"active_endpoints":["1"],"members":[{"device":{"node":"n1","stable_id":42},"endpoint":"1","inventory_generation":7}]}"#).unwrap();
        assert!(binding.file_revision.is_none());
        assert!(binding.observed_key.is_none());
        let (_, inventory, allocation) = fixture();
        binding
            .validate("test", "n1", &inventory, &allocation)
            .unwrap();

        assert!(allocation.fabric_binding.is_none());
    }

    #[test]
    fn rejects_legacy_agents_and_stale_or_unset_generation() {
        let (binding, mut inventory, mut allocation) = fixture();
        inventory.fabric_binding_version = 0;
        assert!(binding
            .validate("test", "n1", &inventory, &allocation)
            .is_err());
        inventory.fabric_binding_version = 1;
        inventory.generation = 8;
        assert!(binding
            .validate("test", "n1", &inventory, &allocation)
            .is_err());
        inventory.generation = 7;
        allocation.generation = 0;
        assert!(binding
            .validate("test", "n1", &inventory, &allocation)
            .is_err());
    }

    #[test]
    fn rejects_membership_change_even_with_same_generation() {
        let (binding, mut inventory, allocation) = fixture();
        inventory.gpus[0]
            .topology
            .as_mut()
            .unwrap()
            .fabric
            .as_mut()
            .unwrap()
            .fabric_domain_id = Some("8".into());
        assert!(binding
            .validate("test", "n1", &inventory, &allocation)
            .is_err());
    }

    #[test]
    fn rejects_changed_scope_devices_and_unknown_binding_versions() {
        let (mut binding, inventory, mut allocation) = fixture();
        assert!(binding
            .validate("other", "n1", &inventory, &allocation)
            .is_err());
        assert!(binding
            .validate("test", "n2", &inventory, &allocation)
            .is_err());
        allocation
            .devices
            .get_mut("gpu")
            .unwrap()
            .push(AllocatedDevice::injectable(43));
        assert!(binding
            .validate("test", "n1", &inventory, &allocation)
            .is_err());
        allocation.devices.get_mut("gpu").unwrap().pop();
        binding.version = 2;
        assert!(binding
            .validate("test", "n1", &inventory, &allocation)
            .is_err());
        let malformed = from_proto(spur_proto::proto::FabricBinding {
            version: 1,
            ..Default::default()
        });
        assert!(malformed
            .validate("test", "n1", &inventory, &allocation)
            .is_err());
    }
}
