// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::cdi::annotations::DeviceMetadata;
use crate::cdi::cache::CachedDevice;
use crate::cdi::spec::{vendor_from_kind, ContainerEdits};
use crate::inject::{gpu_visibility_for_vendor, InjectActions};
use crate::types::LinkType;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceCapability {
    Injectable,
    CountablePool,
}

/// What a device is for, which decides the runtime env a job gets for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceClass {
    Gpu,
    Nic,
    Other,
}

impl DeviceClass {
    pub fn of(gres_name: &str) -> Self {
        match gres_name {
            "gpu" => Self::Gpu,
            "nic" | "vnic" => Self::Nic,
            _ => Self::Other,
        }
    }
}

#[derive(Clone, Debug)]
pub struct DeviceEntry {
    pub gres_name: String,
    pub resource_type: Option<String>,
    pub kind: String,
    pub device_id: u32,
    pub capability: DeviceCapability,
    pub memory_mb: u64,
    pub links: Option<Vec<i32>>,
    pub link_type: Option<LinkType>,
    pub pci_bdf: Option<String>,
    pub vendor: Option<String>,
    pub inject_actions: InjectActions,
    pub capacity: u64,
    pub device_edits: ContainerEdits,
    pub shared_edits: ContainerEdits,
    pub stable_id: u64,
    pub topology: Option<crate::topology::DeviceTopology>,
    pub numa_node: Option<u32>,
    /// CPUs near the device (`[[devices.gres]] cores=`); feeds per-GPU core placement.
    pub cores: Option<Vec<u32>>,
}

impl DeviceEntry {
    pub fn is_injectable(&self) -> bool {
        self.capability == DeviceCapability::Injectable
    }

    pub fn is_countable_pool(&self) -> bool {
        self.capability == DeviceCapability::CountablePool
    }

    pub fn class(&self) -> DeviceClass {
        DeviceClass::of(&self.gres_name)
    }

    /// Host paths of this device's own nodes, excluding nodes shared by its kind.
    pub fn node_paths(&self) -> impl Iterator<Item = &str> {
        self.device_edits
            .device_nodes
            .iter()
            .map(|dn| dn.effective_host_path())
    }
}

pub fn from_cdi(cached: &CachedDevice) -> DeviceEntry {
    let meta = DeviceMetadata::from_annotations(&cached.device.annotations);
    let device_edits = cached.device.container_edits.clone().unwrap_or_default();
    let shared_edits = cached.shared_edits.clone().unwrap_or_default();
    let gres_name = kind_to_gres_name(&cached.kind);

    let vendor = vendor_from_kind(&cached.kind);
    let gpu_visibility_vars = match DeviceClass::of(&gres_name) {
        DeviceClass::Gpu => gpu_visibility_for_vendor(vendor.as_deref()),
        DeviceClass::Nic | DeviceClass::Other => Default::default(),
    };
    let inject_actions = InjectActions {
        gpu_visibility_vars,
    };

    DeviceEntry {
        gres_name,
        resource_type: meta.gpu_type.clone(),
        kind: cached.kind.clone(),
        device_id: 0,
        capability: DeviceCapability::Injectable,
        memory_mb: meta.memory_mb,
        links: meta.links,
        link_type: meta.link_type,
        pci_bdf: meta.pci_bdf,
        vendor,
        inject_actions,
        capacity: 1,
        device_edits,
        shared_edits,
        topology: meta.topology,
        stable_id: meta.stable_id.unwrap_or(0),
        numa_node: meta.numa_node,
        cores: meta.cores,
    }
}

pub fn resolve_link_type(entry: &DeviceEntry) -> LinkType {
    if let Some(lt) = entry.link_type {
        return lt;
    }
    if let Some(ref links) = entry.links {
        if links.iter().any(|&w| w > 0) {
            return match entry.vendor.as_deref() {
                Some("amd") => LinkType::Xgmi,
                Some("nvidia") => LinkType::Nvlink,
                _ => LinkType::Pcie,
            };
        }
    }
    LinkType::Pcie
}

pub fn kind_to_gres_name(kind: &str) -> String {
    kind.split_once('/')
        .map(|(_, class)| class.to_owned())
        .unwrap_or_else(|| kind.to_owned())
}

pub fn entry_matches_gres_name(entry: &DeviceEntry, gres_name: &str) -> bool {
    entry.gres_name == gres_name || kind_matches_gres_name(&entry.kind, gres_name)
}

pub fn kind_matches_gres_name(kind: &str, gres_name: &str) -> bool {
    kind.split_once('/')
        .map(|(_, class)| class == gres_name)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_link_type_explicit() {
        let entry = DeviceEntry {
            link_type: Some(LinkType::Nvlink),
            links: Some(vec![-1, 4, 4, 0]),
            vendor: Some("amd".into()),
            ..test_gpu_entry()
        };
        assert_eq!(resolve_link_type(&entry), LinkType::Nvlink);
    }

    #[test]
    fn test_resolve_link_type_from_links_amd() {
        let entry = DeviceEntry {
            link_type: None,
            links: Some(vec![-1, 4, 4, 0]),
            vendor: Some("amd".into()),
            ..test_gpu_entry()
        };
        assert_eq!(resolve_link_type(&entry), LinkType::Xgmi);
    }

    #[test]
    fn test_resolve_link_type_from_links_unknown_vendor() {
        let entry = DeviceEntry {
            link_type: None,
            links: Some(vec![-1, 2, 0]),
            vendor: None,
            ..test_gpu_entry()
        };
        assert_eq!(resolve_link_type(&entry), LinkType::Pcie);
    }

    #[test]
    fn test_resolve_link_type_default_pcie() {
        let entry = DeviceEntry {
            link_type: None,
            links: None,
            vendor: Some("amd".into()),
            ..test_gpu_entry()
        };
        assert_eq!(resolve_link_type(&entry), LinkType::Pcie);
    }

    fn test_gpu_entry() -> DeviceEntry {
        DeviceEntry {
            numa_node: None,
            cores: None,
            gres_name: "gpu".into(),
            resource_type: None,
            kind: "amd.com/gpu".into(),
            device_id: 0,
            capability: DeviceCapability::Injectable,
            memory_mb: 0,
            links: None,
            link_type: None,
            pci_bdf: None,
            vendor: None,
            inject_actions: InjectActions::default(),
            capacity: 1,
            device_edits: ContainerEdits::default(),
            shared_edits: ContainerEdits::default(),
            topology: None,
            stable_id: 0,
        }
    }
}
