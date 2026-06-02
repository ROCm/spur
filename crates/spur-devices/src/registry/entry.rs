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

#[derive(Clone, Debug)]
pub struct DeviceEntry {
    pub index: u32,
    pub gres_name: String,
    pub resource_type: Option<String>,
    pub kind: String,
    pub device_id: u32,
    pub capability: DeviceCapability,
    pub memory_mb: u64,
    pub numa_node: Option<u32>,
    pub cores: Option<Vec<u32>>,
    pub links: Option<Vec<i32>>,
    pub link_type: Option<LinkType>,
    pub pci_bdf: Option<String>,
    pub vendor: Option<String>,
    pub inject_actions: InjectActions,
    pub capacity: u64,
    pub allocated: u64,
    pub device_edits: ContainerEdits,
    pub shared_edits: ContainerEdits,
    pub device_paths: Vec<String>,
}

impl DeviceEntry {
    pub fn is_injectable(&self) -> bool {
        self.capability == DeviceCapability::Injectable
    }

    pub fn is_countable_pool(&self) -> bool {
        self.capability == DeviceCapability::CountablePool
    }

    pub fn is_gres_sourced(&self) -> bool {
        self.kind == "spur.internal/gres"
    }
}

pub fn from_cdi(cached: &CachedDevice) -> DeviceEntry {
    let meta = DeviceMetadata::from_annotations(&cached.device.annotations);
    let device_edits = cached.device.container_edits.clone().unwrap_or_default();
    let shared_edits = cached.shared_edits.clone().unwrap_or_default();
    let device_paths = collect_device_paths(&device_edits, &shared_edits);
    let gres_name = kind_to_gres_name(&cached.kind);

    let vendor = vendor_from_kind(&cached.kind);
    let inject_actions = InjectActions {
        gpu_visibility_vars: gpu_visibility_for_vendor(vendor.as_deref()),
    };

    DeviceEntry {
        index: 0,
        gres_name,
        resource_type: meta.gpu_type.clone(),
        kind: cached.kind.clone(),
        device_id: 0,
        capability: DeviceCapability::Injectable,
        memory_mb: meta.memory_mb,
        numa_node: meta.numa_node,
        cores: meta.cores,
        links: meta.links,
        link_type: meta.link_type,
        pci_bdf: meta.pci_bdf,
        vendor,
        inject_actions,
        capacity: 1,
        allocated: 0,
        device_edits,
        shared_edits,
        device_paths,
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

pub(crate) fn collect_device_paths(
    device_edits: &ContainerEdits,
    shared_edits: &ContainerEdits,
) -> Vec<String> {
    let mut paths: Vec<String> = device_edits
        .device_nodes
        .iter()
        .map(|dn| dn.effective_host_path().to_owned())
        .collect();
    paths.extend(
        shared_edits
            .device_nodes
            .iter()
            .map(|dn| dn.effective_host_path().to_owned()),
    );
    paths
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
            index: 0,
            gres_name: "gpu".into(),
            resource_type: None,
            kind: "amd.com/gpu".into(),
            device_id: 0,
            capability: DeviceCapability::Injectable,
            memory_mb: 0,
            numa_node: None,
            cores: None,
            links: None,
            link_type: None,
            pci_bdf: None,
            vendor: None,
            inject_actions: InjectActions::default(),
            capacity: 1,
            allocated: 0,
            device_edits: ContainerEdits::default(),
            shared_edits: ContainerEdits::default(),
            device_paths: Vec::new(),
        }
    }
}
