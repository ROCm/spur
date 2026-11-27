// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::cdi::annotations::DeviceMetadata;
use crate::cdi::cache::CachedDevice;
use crate::cdi::spec::{vendor_from_kind, ContainerEdits};
use crate::inject::{gpu_visibility_for_vendor, InjectActions};

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

    /// Its node paths minus any its kind shares (a GRES entry's `/dev/kfd`), so
    /// only nodes that identify this one device.
    pub fn own_node_paths(&self) -> impl Iterator<Item = &str> {
        self.node_paths().filter(move |path| {
            !self
                .shared_edits
                .device_nodes
                .iter()
                .any(|dn| dn.effective_host_path() == *path)
        })
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
