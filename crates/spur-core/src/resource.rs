// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};
pub use spur_devices::topology::DeviceTopology;
use std::collections::{BTreeMap, HashMap};

use crate::device::{Device, LegacyGpu, GPU_GRES};

/// GPU interconnect type. Survives only on the deprecated GPU-only wire and
/// serde shapes, which still carry it for older readers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum GpuLinkType {
    PCIe,
    XGMI,
    NVLink,
}

/// A set of compute resources (node-level inventory).
///
/// Serialized through [`ResourceSetIn`]/[`ResourceSetOut`]: besides `devices` it
/// still writes the deprecated GPU-only `gpus` list, so a controller rolled back
/// to the previous release can replay the log, and reads `gpus` from entries an
/// older build wrote.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(from = "ResourceSetIn")]
pub struct ResourceSet {
    pub cpus: u32,
    pub memory_mb: u64,
    /// Every allocatable device on the node, of every kind (GPUs, NICs, ...).
    pub devices: Vec<Device>,
    pub generic: HashMap<String, u64>,
    /// Bumped by the agent on every topology rebuild. An allocation records the
    /// generation it was made under; a repartition that reuses a stable_id value
    /// for a different logical device is caught by a generation mismatch.
    pub generation: u64,
    /// Shared monotonic capability: native fabric = 1, file = 2, GPU properties = 3.
    pub fabric_binding_version: u32,
    pub block_file_revision: Option<String>,
    /// Agent-reported CPU topology. Belongs in a node's raw report
    /// (`detected_resources`); `clamped` drops it when deriving schedulable
    /// resources from that report.
    pub topology: Option<crate::cpu_topology::CpuTopology>,
}

#[derive(Deserialize)]
struct ResourceSetIn {
    cpus: u32,
    memory_mb: u64,
    #[serde(default)]
    devices: Vec<Device>,
    /// Deprecated: an older build's GPU-only list, read only when `devices` is absent.
    #[serde(default)]
    gpus: Vec<Device>,
    #[serde(default)]
    generic: HashMap<String, u64>,
    #[serde(default)]
    generation: u64,
    #[serde(default)]
    fabric_binding_version: u32,
    #[serde(default)]
    block_file_revision: Option<String>,
    #[serde(default)]
    topology: Option<crate::cpu_topology::CpuTopology>,
}

impl From<ResourceSetIn> for ResourceSet {
    fn from(raw: ResourceSetIn) -> Self {
        let devices = if raw.devices.is_empty() {
            raw.gpus
        } else {
            raw.devices
        };
        Self {
            cpus: raw.cpus,
            memory_mb: raw.memory_mb,
            devices,
            generic: raw.generic,
            generation: raw.generation,
            fabric_binding_version: raw.fabric_binding_version,
            block_file_revision: raw.block_file_revision,
            topology: raw.topology,
        }
    }
}

#[derive(Serialize)]
struct ResourceSetOut<'a> {
    cpus: u32,
    memory_mb: u64,
    devices: &'a [Device],
    /// Deprecated, still written for one release: see [`ResourceSet`].
    gpus: Vec<LegacyGpu>,
    generic: &'a HashMap<String, u64>,
    generation: u64,
    fabric_binding_version: u32,
    block_file_revision: &'a Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    topology: &'a Option<crate::cpu_topology::CpuTopology>,
}

impl Serialize for ResourceSet {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        ResourceSetOut {
            cpus: self.cpus,
            memory_mb: self.memory_mb,
            devices: &self.devices,
            gpus: self.gpus().map(LegacyGpu::from).collect(),
            generic: &self.generic,
            generation: self.generation,
            fabric_binding_version: self.fabric_binding_version,
            block_file_revision: &self.block_file_revision,
            topology: &self.topology,
        }
        .serialize(serializer)
    }
}

pub type InventoryFingerprint = (
    u32,
    u64,
    Vec<Device>,
    Vec<(String, u64)>,
    u32,
    Option<String>,
    Option<crate::cpu_topology::CpuTopology>,
);

/// Tracks consumed resources (per-node or per-job allocation accounting).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResourceAllocations {
    pub cpus: u32,
    pub memory_mb: u64,
    /// Key = gres_name ("gpu", "nic", "bandwidth", ...).
    pub devices: HashMap<String, Vec<AllocatedDevice>>,
    /// The node inventory `generation` this allocation was stamped under. The
    /// agent rejects a dispatch whose generation no longer matches its live
    /// inventory, so a repartition between schedule and launch is caught.
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub fabric_binding: Option<crate::fabric_binding::FabricBinding>,
    #[serde(default)]
    pub gpu_binding: Option<crate::gpu_binding::GpuBinding>,
    /// Of `cpus`, how many come from the allocated GPUs' reserved cores.
    // Omitted at zero: allocations are in every job, step and Raft entry, and
    // most nodes reserve nothing.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub reserved_cpus: u32,
    /// Layout this allocation was placed under; one from an older layout is
    /// counted as entirely general. A tag, not summed by `add`.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub layout_fingerprint: u64,
}

fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

/// A single allocated device entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AllocatedDevice {
    /// For GPU allocations this carries the GPU's `stable_id` (u64). For
    /// countable/positional non-GPU devices it carries a positional id or 0.
    pub device_id: u64,
    /// 1 for injectable devices (GPU, NIC). >1 for countable (bandwidth, licenses).
    pub count: u64,
}

impl AllocatedDevice {
    pub fn injectable(device_id: u64) -> Self {
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

    pub fn device_ids(&self, gres_name: &str) -> Vec<u64> {
        self.devices
            .get(gres_name)
            .map(|devs| devs.iter().map(|d| d.device_id).collect())
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub fn allocated_count(
        &self,
        gres_name: &str,
        device_type: Option<&str>,
        inventory: &ResourceSet,
    ) -> u32 {
        let Some(devs) = self.devices.get(gres_name) else {
            return 0;
        };
        devs.iter()
            .filter(|d| match device_type {
                None | Some("any") => true,
                Some(t) => inventory
                    .devices_of(gres_name)
                    .find(|inv| inv.stable_id == d.device_id)
                    .is_none_or(|inv| inv.device_type == t),
            })
            .map(|d| d.count as u32)
            .sum()
    }

    pub fn generic_count(&self, name: &str) -> u64 {
        self.devices
            .get(name)
            .map(|devs| devs.iter().map(|d| d.count).sum())
            .unwrap_or(0)
    }

    /// Reserved CPUs this allocation holds under the layout `fingerprint`; one
    /// placed under any other layout is entirely general.
    pub fn reserved_under(&self, fingerprint: u64) -> u32 {
        if self.layout_fingerprint == fingerprint {
            self.reserved_cpus
        } else {
            0
        }
    }

    pub fn add(&mut self, other: &ResourceAllocations) {
        self.cpus = self.cpus.saturating_add(other.cpus);
        self.memory_mb = self.memory_mb.saturating_add(other.memory_mb);
        self.reserved_cpus = self.reserved_cpus.saturating_add(other.reserved_cpus);
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
        self.reserved_cpus = self.reserved_cpus.saturating_sub(other.reserved_cpus);
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

    pub fn from_device_ids(gres_name: impl Into<String>, device_ids: &[u64]) -> Self {
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
            ..Default::default()
        }
    }
}

/// A requested device kind: a gres name and an optional type (`None` takes any).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceKey {
    pub gres_name: String,
    pub device_type: Option<String>,
}

impl DeviceKey {
    pub fn new(gres_name: &str, device_type: Option<&str>) -> Self {
        Self {
            gres_name: gres_name.to_string(),
            device_type: device_type.map(str::to_string),
        }
    }

    /// The key a node's countable `generic` pool uses for this gres.
    pub fn generic_key(&self) -> String {
        match &self.device_type {
            Some(t) => format!("{}:{t}", self.gres_name),
            None => self.gres_name.clone(),
        }
    }
}

/// What one node must provide for a job. Demand, not inventory: devices are
/// counts by kind, never concrete devices.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResourceRequest {
    pub cpus: u32,
    pub memory_mb: u64,
    pub devices: BTreeMap<DeviceKey, u32>,
}

impl ResourceRequest {
    pub fn with_scalar(cpus: u32, memory_mb: u64) -> Self {
        Self {
            cpus,
            memory_mb,
            ..Default::default()
        }
    }

    pub fn add_devices(&mut self, gres_name: &str, device_type: Option<&str>, count: u32) {
        if count == 0 {
            return;
        }
        *self
            .devices
            .entry(DeviceKey::new(gres_name, device_type))
            .or_insert(0) += count;
    }

    /// Each gres's typed keys before its untyped one, so `nic:pollara400` gets
    /// its type before a plain `nic` takes from the same devices.
    pub fn devices_typed_first(&self) -> Vec<(&DeviceKey, u32)> {
        let mut keys: Vec<(&DeviceKey, u32)> = self
            .devices
            .iter()
            .map(|(key, &count)| (key, count))
            .collect();
        keys.sort_by_key(|(key, _)| (key.gres_name.as_str(), key.device_type.is_none()));
        keys
    }

    /// Replace every request for `gres_name` with `count` devices of `device_type`.
    pub fn set_devices(&mut self, gres_name: &str, device_type: Option<&str>, count: u32) {
        self.devices.retain(|key, _| key.gres_name != gres_name);
        self.add_devices(gres_name, device_type, count);
    }

    pub fn device_count(&self, gres_name: &str) -> u32 {
        self.devices
            .iter()
            .filter(|(key, _)| key.gres_name == gres_name)
            .map(|(_, count)| count)
            .sum()
    }
}

impl ResourceSet {
    pub fn devices_of<'a>(&'a self, gres_name: &'a str) -> impl Iterator<Item = &'a Device> {
        self.devices
            .iter()
            .filter(move |d| d.gres_name == gres_name)
    }

    pub fn gpus(&self) -> impl Iterator<Item = &Device> {
        self.devices_of(GPU_GRES)
    }

    pub fn gpus_mut(&mut self) -> impl Iterator<Item = &mut Device> {
        self.devices.iter_mut().filter(|d| d.is_gpu())
    }

    /// Backfill a stable id from the positional device id for legacy/mixed-version
    /// inventories that predate stable_id (serde/proto default it to 0).
    pub fn backfill_stable_ids(&mut self) {
        for g in self.gpus_mut() {
            if g.stable_id == 0 {
                g.stable_id = u64::from(g.index);
            }
        }
    }

    /// The same inventory with GPU ids moved to their current encoding.
    pub fn with_renamed_gpus(&self, renamed: &HashMap<u64, u64>) -> ResourceSet {
        let mut out = self.clone();
        for gpu in out.gpus_mut() {
            if let Some(&id) = renamed.get(&gpu.stable_id) {
                gpu.stable_id = id;
            }
        }
        out
    }

    /// Identity of the schedulable set, independent of `generation`. Two ticks
    /// with an equal fingerprint observed the same topology, so a debounce can
    /// tell "same change seen twice" from "two different partial reads".
    pub fn schedulable_fingerprint(&self) -> InventoryFingerprint {
        let mut devices = self.devices.clone();
        devices.sort();
        let mut generic: Vec<(String, u64)> =
            self.generic.iter().map(|(k, v)| (k.clone(), *v)).collect();
        generic.sort();
        (
            self.cpus,
            self.memory_mb,
            devices,
            generic,
            self.fabric_binding_version,
            self.block_file_revision.clone(),
            self.topology.clone(),
        )
    }

    /// Clamp this detected inventory to operator caps. `cpus_cap`/`memory_cap_mb`
    /// are caps (0 = unset); `reserved_memory_mb` is subtracted from detected memory;
    /// `spec_cpus` (cores set aside for node services) is subtracted from detected
    /// CPUs before `cpus_cap` is applied. The smaller of cap and (detected - reserved)
    /// wins; GPUs/generic are unaffected.
    pub fn clamped(
        &self,
        cpus_cap: u32,
        memory_cap_mb: u64,
        reserved_memory_mb: u64,
        spec_cpus: u32,
    ) -> ResourceSet {
        let usable = self.cpus.saturating_sub(spec_cpus);
        let cpus = if cpus_cap > 0 {
            usable.min(cpus_cap)
        } else {
            usable
        };
        let mem_cap = if memory_cap_mb > 0 {
            memory_cap_mb
        } else {
            self.memory_mb
        };
        let memory_mb = mem_cap.min(self.memory_mb.saturating_sub(reserved_memory_mb));
        ResourceSet {
            cpus,
            memory_mb,
            devices: self.devices.clone(),
            generic: self.generic.clone(),
            generation: self.generation,
            fabric_binding_version: self.fabric_binding_version,
            block_file_revision: self.block_file_revision.clone(),
            topology: None,
        }
    }

    /// A request for everything this node has, as an exclusive job needs.
    pub fn as_request(&self) -> ResourceRequest {
        let mut request = ResourceRequest::with_scalar(self.cpus, self.memory_mb);
        for device in &self.devices {
            let device_type =
                (!device.device_type.is_empty()).then_some(device.device_type.as_str());
            request.add_devices(&device.gres_name, device_type, 1);
        }
        for (pool, &count) in &self.generic {
            let name = crate::device::pool_gres_name(pool);
            let device_type = pool.split_once(':').map(|(_, t)| t);
            // A pool sharing its name with a kind handed out by id is not
            // requestable here; the job already takes every such device.
            if self.allocates_by_id(name) {
                continue;
            }
            request.add_devices(name, device_type, u32::try_from(count).unwrap_or(u32::MAX));
        }
        request
    }

    /// Check if this inventory can satisfy a request with no prior allocation.
    pub fn can_satisfy(&self, request: &ResourceRequest) -> bool {
        self.can_satisfy_with_allocated(&ResourceAllocations::default(), request)
    }

    /// Check if available resources (inventory minus allocated) can satisfy a request.
    pub fn can_satisfy_with_allocated(
        &self,
        allocated: &ResourceAllocations,
        request: &ResourceRequest,
    ) -> bool {
        let avail_cpus = self.cpus.saturating_sub(allocated.cpus);
        let avail_mem = self.memory_mb.saturating_sub(allocated.memory_mb);
        if avail_cpus < request.cpus || avail_mem < request.memory_mb {
            return false;
        }
        // Picks are simulated so typed and untyped keys of one gres cannot both
        // count the same free device.
        let mut picked = allocated.clone();
        for (key, count) in request.devices_typed_first() {
            if !self.allocates_by_id(&key.gres_name) {
                if self.free_pool_count(allocated, key) < u64::from(count) {
                    return false;
                }
                continue;
            }
            let free =
                self.available_device_ids(&picked, &key.gres_name, key.device_type.as_deref());
            if free.len() < count as usize {
                return false;
            }
            picked
                .devices
                .entry(key.gres_name.clone())
                .or_default()
                .extend(
                    free.into_iter()
                        .take(count as usize)
                        .map(|device_id| AllocatedDevice {
                            device_id,
                            count: 1,
                        }),
                );
        }
        true
    }

    /// What is left of the countable pool `key` names.
    fn free_pool_count(&self, allocated: &ResourceAllocations, key: &DeviceKey) -> u64 {
        let pool = key.generic_key();
        self.generic
            .get(&pool)
            .copied()
            .unwrap_or(0)
            .saturating_sub(allocated.generic_count(&pool))
    }

    /// Whether this node hands out `gres_name` as specific devices, not a count.
    pub fn allocates_by_id(&self, gres_name: &str) -> bool {
        crate::device::allocates_by_id(gres_name, self.devices_of(gres_name).next().is_some())
    }

    /// An allocated id the inventory lost means a held device may now be
    /// reported under another id, so none of this node's devices of that kind
    /// can be trusted free.
    fn holds_unknown_device(&self, allocated: &ResourceAllocations, gres_name: &str) -> bool {
        allocated
            .device_ids(gres_name)
            .iter()
            .any(|id| !self.devices_of(gres_name).any(|d| d.stable_id == *id))
    }

    /// Return unallocated device IDs for an injectable gres type.
    pub fn available_device_ids(
        &self,
        allocated: &ResourceAllocations,
        gres_name: &str,
        device_type: Option<&str>,
    ) -> Vec<u64> {
        if self.holds_unknown_device(allocated, gres_name) {
            return Vec::new();
        }
        let allocated_ids: std::collections::HashSet<u64> =
            allocated.device_ids(gres_name).into_iter().collect();
        self.devices_of(gres_name)
            .filter(|d| !allocated_ids.contains(&d.stable_id))
            .filter(|d| match device_type {
                None | Some("any") => true,
                Some(t) => d.device_type == t,
            })
            .map(|d| d.stable_id)
            .collect()
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
        for gpu in self.gpus() {
            *counts.entry(gpu.device_type.clone()).or_insert(0) += 1;
        }
        counts
    }

    pub fn total_gpus(&self) -> u32 {
        self.gpus().count() as u32
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
    alloc.generation = inventory.generation;
    for device in &inventory.devices {
        alloc
            .devices
            .entry(device.gres_name.clone())
            .or_default()
            .push(AllocatedDevice::injectable(device.stable_id));
    }
    for (name, count) in &inventory.generic {
        // Same rule as the whole-node request: a pool named like a kind handed
        // out by id is never allocated next to those devices.
        if *count > 0 && !inventory.allocates_by_id(crate::device::pool_gres_name(name)) {
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
    request: &ResourceRequest,
) -> ResourceAllocations {
    let mut alloc = ResourceAllocations::with_scalar(request.cpus, request.memory_mb);
    alloc.generation = inventory.generation;

    for (key, count) in request.devices_typed_first() {
        if count == 0 {
            continue;
        }
        if inventory.allocates_by_id(&key.gres_name) {
            let mut effective_alloc = current_alloc.clone();
            effective_alloc.add(&alloc);
            let picked = inventory.pick_devices(
                &effective_alloc,
                &key.gres_name,
                key.device_type.as_deref(),
                count,
            );
            alloc
                .devices
                .entry(key.gres_name.clone())
                .or_default()
                .extend(picked);
        } else {
            alloc
                .devices
                .entry(key.generic_key())
                .or_default()
                .push(AllocatedDevice {
                    device_id: 0,
                    count: u64::from(count),
                });
        }
    }

    alloc
}

#[cfg(test)]
mod tests {
    #[test]
    fn legacy_inventory_json_deserializes_without_topology() {
        let old = r#"{"cpus":8,"memory_mb":1024,"gpus":[{"device_id":0,"gpu_type":"amdgpu","memory_mb":4096,"peer_gpus":[],"link_type":"PCIe","stable_id":128}],"generic":{},"generation":7}"#;
        let inventory: super::ResourceSet = serde_json::from_str(old).unwrap();
        assert!(inventory.devices[0].topology.is_none());
        assert_eq!(inventory.generation, 7);
        assert_eq!(inventory.devices[0].stable_id, 128);
    }

    #[test]
    fn with_renamed_gpus_moves_only_mapped_ids() {
        let inventory = sample_inventory();
        let renamed = HashMap::from([(0, 100)]);
        let out = inventory.with_renamed_gpus(&renamed);
        assert_eq!(out.devices[0].stable_id, 100);
        assert_eq!(out.devices[1].stable_id, 1);
    }

    #[test]
    fn inventory_fingerprint_tracks_membership_and_gpu_properties() {
        use spur_devices::topology::{DeviceTopology, FabricObservation, ObservationStatus};
        let mut a = sample_inventory();
        a.devices[0].topology = Some(DeviceTopology {
            fabric: Some(FabricObservation {
                status: ObservationStatus::Consistent,
                fabric_domain_id: Some("15".into()),
                ..Default::default()
            }),
            ..Default::default()
        });
        let mut b = a.clone();
        b.generation += 1;
        assert_eq!(a.schedulable_fingerprint(), b.schedulable_fingerprint());
        b.devices[0]
            .topology
            .as_mut()
            .unwrap()
            .fabric
            .as_mut()
            .unwrap()
            .fabric_domain_id = Some("16".into());
        assert_ne!(a.schedulable_fingerprint(), b.schedulable_fingerprint());
        b = a.clone();
        b.devices[0].device_type = "mi250x".into();
        assert_ne!(a.schedulable_fingerprint(), b.schedulable_fingerprint());
        b = a.clone();
        b.devices.reverse();
        assert_eq!(a.schedulable_fingerprint(), b.schedulable_fingerprint());
    }

    use super::*;

    fn sample_inventory() -> ResourceSet {
        ResourceSet {
            fabric_binding_version: 0,
            block_file_revision: None,
            cpus: 64,
            memory_mb: 256_000,
            devices: vec![
                Device {
                    numa_node: None,
                    affinity_cpus: Vec::new(),
                    gres_name: "gpu".into(),
                    device_type: "mi300x".into(),
                    stable_id: 0,
                    index: 0,
                    topology: None,
                },
                Device {
                    numa_node: None,
                    affinity_cpus: Vec::new(),
                    gres_name: "gpu".into(),
                    device_type: "mi300x".into(),
                    stable_id: 1,
                    index: 1,
                    topology: None,
                },
            ],
            generic: HashMap::new(),
            generation: 0,
            topology: None,
        }
    }

    #[test]
    fn clamp_no_config_is_identity() {
        // cpus=0, memory_mb=0, reserved=0 means "no limits configured".
        let detected = sample_inventory();
        let clamped = detected.clamped(0, 0, 0, 0);
        assert_eq!(clamped.cpus, 64);
        assert_eq!(clamped.memory_mb, 256_000);
        // GPUs and generic pass through untouched.
        assert_eq!(clamped.gpus().count(), 2);
    }

    #[test]
    fn clamp_cpus_caps_below_detected() {
        let detected = sample_inventory();
        let clamped = detected.clamped(32, 0, 0, 0);
        assert_eq!(clamped.cpus, 32);
    }

    #[test]
    fn clamp_cpus_over_claim_capped_to_detected() {
        // Config asking for more CPUs than the host has cannot invent hardware.
        let detected = sample_inventory();
        let clamped = detected.clamped(128, 0, 0, 0);
        assert_eq!(clamped.cpus, 64);
    }

    #[test]
    fn clamp_memory_cap_below_detected() {
        let detected = sample_inventory();
        let clamped = detected.clamped(0, 200_000, 0, 0);
        assert_eq!(clamped.memory_mb, 200_000);
    }

    #[test]
    fn clamp_memory_over_claim_capped_to_detected() {
        let detected = sample_inventory();
        let clamped = detected.clamped(0, 512_000, 0, 0);
        assert_eq!(clamped.memory_mb, 256_000);
    }

    #[test]
    fn clamp_reserved_subtracts_from_detected() {
        let detected = sample_inventory();
        let clamped = detected.clamped(0, 0, 16_000, 0);
        assert_eq!(clamped.memory_mb, 240_000);
    }

    #[test]
    fn clamp_memory_levers_compose_smaller_wins() {
        // memory_mb cap = 250_000; detected - reserved = 256_000 - 16_000 = 240_000.
        // The smaller (reserved-derived) value wins.
        let detected = sample_inventory();
        let clamped = detected.clamped(0, 250_000, 16_000, 0);
        assert_eq!(clamped.memory_mb, 240_000);
    }

    #[test]
    fn clamp_reserved_saturates_at_zero() {
        // reserved >= detected must floor at 0, never underflow.
        let detected = sample_inventory();
        let clamped = detected.clamped(0, 0, 999_999, 0);
        assert_eq!(clamped.memory_mb, 0);
    }

    #[test]
    fn clamped_removes_spec_cpus_before_the_cap() {
        let detected = ResourceSet {
            cpus: 256,
            memory_mb: 2_000_000,
            ..Default::default()
        };
        assert_eq!(detected.clamped(0, 0, 0, 2).cpus, 254);
        // The cap limits schedulable CPUs, which already exclude spec CPUs.
        assert_eq!(detected.clamped(200, 0, 0, 2).cpus, 200);
        assert_eq!(detected.clamped(0, 0, 0, 300).cpus, 0);
    }

    #[test]
    fn clamped_never_copies_the_topology() {
        let detected = ResourceSet {
            cpus: 2,
            topology: Some(crate::cpu_topology::CpuTopology {
                cores: vec![crate::cpu_topology::CoreInfo {
                    socket: 0,
                    numa_node: None,
                    cpus: vec![0, 1],
                }],
            }),
            ..Default::default()
        };
        assert!(detected.clamped(0, 0, 0, 0).topology.is_none());
    }

    #[test]
    fn a_resource_set_without_topology_keeps_its_old_json_shape() {
        let set = ResourceSet {
            cpus: 4,
            ..Default::default()
        };
        let json = serde_json::to_value(&set).unwrap();
        assert!(json.get("topology").is_none(), "{json}");
        let old = r#"{"cpus":4,"memory_mb":0,"gpus":[],"generic":{}}"#;
        let decoded: ResourceSet = serde_json::from_str(old).unwrap();
        assert!(decoded.topology.is_none());
        assert_eq!(decoded.cpus, 4);
    }

    #[test]
    fn test_can_satisfy() {
        let avail = sample_inventory();
        let mut req = ResourceRequest::with_scalar(32, 128_000);
        req.add_devices("gpu", Some("mi300x"), 1);
        assert!(avail.can_satisfy(&req));
    }

    #[test]
    fn test_can_satisfy_with_allocated() {
        let inventory = sample_inventory();
        let mut allocated = ResourceAllocations::with_scalar(0, 0);
        allocated
            .devices
            .insert("gpu".into(), vec![AllocatedDevice::injectable(0)]);

        let mut req_one = ResourceRequest::with_scalar(1, 1);
        req_one.add_devices("gpu", Some("mi300x"), 1);
        assert!(inventory.can_satisfy_with_allocated(&allocated, &req_one));

        let mut req_two = ResourceRequest::default();
        req_two.add_devices("gpu", Some("mi300x"), 2);
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
        let mut inventory = sample_inventory();
        // Positional device_id equals stable_id here so the classic meaning holds.
        inventory.devices[0].stable_id = 0;
        inventory.devices[1].stable_id = 1;
        let allocated = ResourceAllocations::from_device_ids("gpu", &[0]);
        let free = inventory.available_device_ids(&allocated, "gpu", Some("mi300x"));
        assert_eq!(free, vec![1]);
    }

    /// After an out-of-band repartition a GPU's positional `device_id` and its
    /// stable identity diverge; allocation must key on `stable_id` so the ids
    /// handed to the agent match its stable_id-based accounting.
    #[test]
    fn available_device_ids_returns_stable_ids_not_positional() {
        let inventory = ResourceSet {
            fabric_binding_version: 0,
            block_file_revision: None,
            cpus: 64,
            memory_mb: 256_000,
            devices: vec![
                Device {
                    numa_node: None,
                    affinity_cpus: Vec::new(),
                    gres_name: "gpu".into(),
                    device_type: "mi300x".into(),
                    stable_id: 128,
                    index: 0,
                    topology: None,
                },
                Device {
                    numa_node: None,
                    affinity_cpus: Vec::new(),
                    gres_name: "gpu".into(),
                    device_type: "mi300x".into(),
                    stable_id: 129,
                    index: 1,
                    topology: None,
                },
            ],
            generic: HashMap::new(),
            generation: 7,
            topology: None,
        };
        // Nothing allocated: both stable ids are free.
        let free =
            inventory.available_device_ids(&ResourceAllocations::default(), "gpu", Some("mi300x"));
        assert_eq!(free, vec![128, 129]);

        // Allocating stable_id 128 leaves 129 free (not positional index 1).
        let allocated = ResourceAllocations::from_device_ids("gpu", &[128]);
        let free = inventory.available_device_ids(&allocated, "gpu", Some("mi300x"));
        assert_eq!(free, vec![129]);
    }

    fn two_gpu_inventory(a: u64, b: u64) -> ResourceSet {
        let mut inventory = sample_inventory();
        inventory.devices[0].stable_id = a;
        inventory.devices[1].stable_id = b;
        inventory
    }

    #[test]
    fn a_node_holding_an_id_it_no_longer_has_offers_no_gpu() {
        let inventory = two_gpu_inventory(0x11_0000, 0x12_0000);

        let orphaned = ResourceAllocations::from_device_ids("gpu", &[0x11_0100]);
        assert!(inventory
            .available_device_ids(&orphaned, "gpu", None)
            .is_empty());

        let held = ResourceAllocations::from_device_ids("gpu", &[0x11_0000]);
        assert_eq!(
            inventory.available_device_ids(&held, "gpu", None),
            vec![0x12_0000]
        );
    }

    #[test]
    fn a_node_holding_an_id_it_no_longer_has_cannot_satisfy_a_gpu_request() {
        let inventory = two_gpu_inventory(0x11_0000, 0x12_0000);
        let mut one_gpu = ResourceRequest::with_scalar(1, 0);
        one_gpu.add_devices("gpu", Some("mi300x"), 1);
        let cpu_only = ResourceRequest::with_scalar(1, 0);

        let orphaned = ResourceAllocations::from_device_ids("gpu", &[0x11_0100]);
        assert!(!inventory.can_satisfy_with_allocated(&orphaned, &one_gpu));
        assert!(inventory.can_satisfy_with_allocated(&orphaned, &cpu_only));

        let held = ResourceAllocations::from_device_ids("gpu", &[0x11_0000]);
        assert!(inventory.can_satisfy_with_allocated(&held, &one_gpu));
    }

    #[test]
    fn test_build_node_allocation() {
        let inventory = sample_inventory();
        let mut request = ResourceRequest::with_scalar(8, 16_000);
        request.add_devices("gpu", Some("mi300x"), 1);
        let alloc = build_node_allocation(&inventory, &ResourceAllocations::default(), &request);
        assert_eq!(alloc.cpus, 8);
        assert_eq!(alloc.device_ids("gpu"), vec![0]);
    }

    #[test]
    fn test_build_node_allocation_multi_type() {
        let inventory = ResourceSet {
            fabric_binding_version: 0,
            block_file_revision: None,
            cpus: 64,
            memory_mb: 256_000,
            devices: vec![
                Device {
                    numa_node: None,
                    affinity_cpus: Vec::new(),
                    gres_name: "gpu".into(),
                    device_type: "mi300x".into(),
                    stable_id: 0,
                    index: 0,
                    topology: None,
                },
                Device {
                    numa_node: None,
                    affinity_cpus: Vec::new(),
                    gres_name: "gpu".into(),
                    device_type: "mi300x".into(),
                    stable_id: 1,
                    index: 1,
                    topology: None,
                },
                Device {
                    numa_node: None,
                    affinity_cpus: Vec::new(),
                    gres_name: "gpu".into(),
                    device_type: "h100".into(),
                    stable_id: 2,
                    index: 2,
                    topology: None,
                },
                Device {
                    numa_node: None,
                    affinity_cpus: Vec::new(),
                    gres_name: "gpu".into(),
                    device_type: "h100".into(),
                    stable_id: 3,
                    index: 3,
                    topology: None,
                },
            ],
            generic: HashMap::new(),
            generation: 0,
            topology: None,
        };
        let mut request = ResourceRequest::with_scalar(8, 16_000);
        request.add_devices("gpu", Some("mi300x"), 1);
        request.add_devices("gpu", Some("h100"), 1);
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

#[cfg(test)]
mod accounting_tests {
    use super::*;

    fn gpu(device_id: u32, gpu_type: &str) -> Device {
        Device::gpu(u64::from(device_id), gpu_type, device_id)
    }

    fn inventory() -> ResourceSet {
        ResourceSet {
            fabric_binding_version: 0,
            block_file_revision: None,
            cpus: 64,
            memory_mb: 131_072,
            devices: vec![gpu(0, "mi300x"), gpu(1, "mi300x"), gpu(2, "mi210")],
            generic: HashMap::from([("bandwidth".to_string(), 100u64)]),
            generation: 0,
            topology: None,
        }
    }

    #[test]
    fn reserved_cpus_are_summed_and_released_with_the_allocation() {
        let mut used = ResourceAllocations::default();
        let job = ResourceAllocations {
            cpus: 24,
            reserved_cpus: 16,
            ..Default::default()
        };
        used.add(&job);
        used.add(&job);
        assert_eq!(used.reserved_cpus, 32);
        used.subtract(&job);
        assert_eq!(used.reserved_cpus, 16);
        used.subtract(&ResourceAllocations {
            reserved_cpus: 99,
            ..Default::default()
        });
        assert_eq!(used.reserved_cpus, 0, "clamps at zero like cpus");
    }

    #[test]
    fn allocations_without_reserved_fields_deserialize_with_zero() {
        let alloc: ResourceAllocations =
            serde_json::from_str(r#"{"cpus":4,"memory_mb":0,"devices":{}}"#).unwrap();
        assert_eq!(alloc.reserved_cpus, 0);
        assert_eq!(alloc.layout_fingerprint, 0);
        let json = serde_json::to_string(&alloc).unwrap();
        assert!(
            !json.contains("reserved_cpus") && !json.contains("layout_fingerprint"),
            "{json}"
        );
    }

    /// Releasing more than is held must clamp at zero rather than wrap, and a
    /// device drained to zero must leave the map so `has_devices` goes false.
    #[test]
    fn subtract_clamps_at_zero_and_prunes_drained_devices() {
        let mut held = ResourceAllocations::with_scalar(8, 1024);
        held.devices.insert(
            "gpu".into(),
            vec![
                AllocatedDevice::injectable(0),
                AllocatedDevice::injectable(1),
            ],
        );

        let mut release = ResourceAllocations::with_scalar(100, 99_999);
        release
            .devices
            .insert("gpu".into(), vec![AllocatedDevice::injectable(0)]);
        held.subtract(&release);

        assert_eq!(held.cpus, 0, "over-release clamps instead of wrapping");
        assert_eq!(held.memory_mb, 0);
        assert_eq!(held.device_ids("gpu"), [1], "drained device 0 is pruned");

        let mut drain_rest = ResourceAllocations::default();
        drain_rest
            .devices
            .insert("gpu".into(), vec![AllocatedDevice::injectable(1)]);
        held.subtract(&drain_rest);

        assert!(!held.devices.contains_key("gpu"), "emptied gres is removed");
        assert!(held.is_empty());
    }

    #[test]
    fn subtract_ignores_gres_that_was_never_held() {
        let mut held = ResourceAllocations::with_scalar(4, 512);
        let mut release = ResourceAllocations::default();
        release
            .devices
            .insert("nic".into(), vec![AllocatedDevice::injectable(7)]);

        held.subtract(&release);

        assert_eq!(held.cpus, 4);
        assert!(held.devices.is_empty());
    }

    #[test]
    fn allocated_count_filters_gpus_by_type() {
        let inv = inventory();
        let mut alloc = ResourceAllocations::default();
        alloc.devices.insert(
            "gpu".into(),
            vec![
                AllocatedDevice::injectable(0),
                AllocatedDevice::injectable(2),
            ],
        );

        assert_eq!(alloc.allocated_count("gpu", Some("mi300x"), &inv), 1);
        assert_eq!(alloc.allocated_count("gpu", Some("mi210"), &inv), 1);
        assert_eq!(alloc.allocated_count("gpu", Some("any"), &inv), 2);
        assert_eq!(alloc.allocated_count("gpu", None, &inv), 2);
        assert_eq!(alloc.allocated_count("gpu", Some("mi100"), &inv), 0);
        assert_eq!(alloc.allocated_count("nic", None, &inv), 0);
    }

    /// Non-GPU gres is countable, so a single entry carries a count instead of
    /// one entry per device.
    #[test]
    fn allocated_count_sums_countable_non_gpu_gres() {
        let inv = inventory();
        let mut alloc = ResourceAllocations::default();
        alloc.devices.insert(
            "bandwidth".into(),
            vec![AllocatedDevice {
                device_id: 0,
                count: 40,
            }],
        );

        assert_eq!(alloc.allocated_count("bandwidth", None, &inv), 40);
        assert_eq!(alloc.generic_count("bandwidth"), 40);
        assert_eq!(alloc.total_device_count("bandwidth"), 40);
        assert_eq!(alloc.generic_count("absent"), 0);
    }

    /// `is_empty` inspects the map while `has_devices` inspects inside it, so a
    /// leftover empty vector reads as non-empty yet holds nothing.
    #[test]
    fn is_empty_and_has_devices_disagree_on_empty_device_vec() {
        let mut alloc = ResourceAllocations::default();
        assert!(alloc.is_empty());
        assert!(!alloc.has_devices());

        alloc.devices.insert("gpu".into(), Vec::new());
        assert!(!alloc.is_empty());
        assert!(!alloc.has_devices());

        alloc
            .devices
            .insert("gpu".into(), vec![AllocatedDevice::injectable(3)]);
        assert!(alloc.has_devices());
        assert_eq!(alloc.total_device_count("gpu"), 1);
    }

    #[test]
    fn build_exclusive_allocation_takes_every_cpu_and_gpu() {
        let inv = inventory();
        let alloc = build_exclusive_allocation(&inv, 4096);

        assert_eq!(alloc.cpus, inv.cpus, "exclusive claims every CPU");
        assert_eq!(alloc.memory_mb, 4096, "memory comes from the caller");
        let mut ids = alloc.device_ids("gpu");
        ids.sort_unstable();
        assert_eq!(ids, [0, 1, 2], "every GPU is claimed");
        assert_eq!(alloc.generic_count("bandwidth"), 100);
    }

    #[test]
    fn build_exclusive_allocation_omits_absent_gpus_and_zero_counts() {
        let inv = ResourceSet {
            fabric_binding_version: 0,
            block_file_revision: None,
            cpus: 8,
            memory_mb: 1024,
            devices: Vec::new(),
            generic: HashMap::from([("licenses".to_string(), 0u64)]),
            generation: 0,
            topology: None,
        };
        let alloc = build_exclusive_allocation(&inv, 1024);

        assert!(!alloc.devices.contains_key("gpu"));
        assert!(!alloc.devices.contains_key("licenses"));
        assert!(!alloc.has_devices());
    }

    /// On real KFD nodes positional `device_id` (0,1) and `stable_id` (128,129)
    /// diverge. Exclusive allocation must emit stable_ids so the agent, whose
    /// accounting keys on stable_id, resolves the right devices.
    #[test]
    fn build_exclusive_allocation_emits_stable_ids_not_positional() {
        let inv = ResourceSet {
            fabric_binding_version: 0,
            block_file_revision: None,
            cpus: 16,
            memory_mb: 1024,
            devices: vec![
                Device {
                    numa_node: None,
                    affinity_cpus: Vec::new(),
                    gres_name: "gpu".into(),
                    device_type: "mi300x".into(),
                    stable_id: 128,
                    index: 0,
                    topology: None,
                },
                Device {
                    numa_node: None,
                    affinity_cpus: Vec::new(),
                    gres_name: "gpu".into(),
                    device_type: "mi300x".into(),
                    stable_id: 129,
                    index: 1,
                    topology: None,
                },
            ],
            generic: HashMap::new(),
            generation: 0,
            topology: None,
        };
        let alloc = build_exclusive_allocation(&inv, 1024);

        let mut ids = alloc.device_ids("gpu");
        ids.sort_unstable();
        assert_eq!(ids, [128, 129], "exclusive claims GPUs by stable_id");
    }

    /// Aggregation folds nodes with `add`, which merges on device_id, so two
    /// nodes both reporting device 0 collapse to one entry with count 2.
    #[test]
    fn aggregate_allocations_sums_scalars_and_merges_matching_device_ids() {
        let mut node_a = ResourceAllocations::with_scalar(16, 2048);
        node_a
            .devices
            .insert("gpu".into(), vec![AllocatedDevice::injectable(0)]);
        let mut node_b = ResourceAllocations::with_scalar(16, 2048);
        node_b
            .devices
            .insert("gpu".into(), vec![AllocatedDevice::injectable(0)]);

        let total = aggregate_allocations([node_a, node_b]);

        assert_eq!(total.cpus, 32);
        assert_eq!(total.memory_mb, 4096);
        assert_eq!(total.device_ids("gpu"), [0]);
        assert_eq!(total.total_device_count("gpu"), 2);
    }

    #[test]
    fn aggregate_allocations_of_nothing_is_empty() {
        assert!(aggregate_allocations(std::iter::empty()).is_empty());
    }

    #[test]
    fn total_gpus_counts_devices_not_types() {
        let inv = inventory();
        assert_eq!(inv.total_gpus(), 3);
        assert_eq!(inv.gpu_counts().get("mi300x"), Some(&2));
        assert_eq!(ResourceSet::default().total_gpus(), 0);
    }

    /// Legacy inventories default every stable_id to 0; backfill must derive a
    /// distinct stable_id from device_id while leaving a real nonzero one intact.
    #[test]
    fn backfill_stable_ids_fills_legacy_zeros_only() {
        let mut inv = ResourceSet {
            fabric_binding_version: 0,
            block_file_revision: None,
            cpus: 0,
            memory_mb: 0,
            devices: vec![
                Device {
                    numa_node: None,
                    affinity_cpus: Vec::new(),
                    gres_name: "gpu".into(),
                    device_type: "mi300x".into(),
                    stable_id: 0,
                    index: 0,
                    topology: None,
                },
                Device {
                    numa_node: None,
                    affinity_cpus: Vec::new(),
                    gres_name: "gpu".into(),
                    device_type: "mi300x".into(),
                    stable_id: 0,
                    index: 1,
                    topology: None,
                },
                Device {
                    numa_node: None,
                    affinity_cpus: Vec::new(),
                    gres_name: "gpu".into(),
                    device_type: "mi300x".into(),
                    stable_id: 129,
                    index: 5,
                    topology: None,
                },
            ],
            generic: HashMap::new(),
            generation: 0,
            topology: None,
        };
        inv.backfill_stable_ids();
        let ids: Vec<u64> = inv.gpus().map(|g| g.stable_id).collect();
        assert_eq!(ids, vec![0, 1, 129], "legacy zeros distinct, real id kept");
    }

    /// A typed GPU held by stable_id (not positional device_id) must be removed
    /// from availability; otherwise a backfilled inventory double-books it.
    #[test]
    fn feasibility_subtracts_held_gpu_by_stable_id() {
        let inv = ResourceSet {
            fabric_binding_version: 0,
            block_file_revision: None,
            cpus: 8,
            memory_mb: 1024,
            devices: vec![Device {
                numa_node: None,
                affinity_cpus: Vec::new(),
                gres_name: "gpu".into(),
                device_type: "mi300x".into(),
                stable_id: 128,
                index: 0,
                topology: None,
            }],
            generic: HashMap::new(),
            generation: 0,
            topology: None,
        };
        let allocated = ResourceAllocations::from_device_ids("gpu", &[128]);
        assert_eq!(allocated.allocated_count("gpu", Some("mi300x"), &inv), 1);

        let mut req = ResourceRequest::with_scalar(1, 1);
        req.add_devices("gpu", Some("mi300x"), 1);
        assert!(
            !inv.can_satisfy_with_allocated(&allocated, &req),
            "the held stable_id 128 mi300x must not be re-offered"
        );
    }
}

#[cfg(test)]
mod request_tests {
    use super::*;

    fn mixed_inventory() -> ResourceSet {
        let gpu = |device_id: u32, gpu_type: &str| Device {
            numa_node: None,
            affinity_cpus: Vec::new(),
            gres_name: "gpu".into(),
            device_type: gpu_type.into(),
            stable_id: u64::from(device_id) + 1,
            index: device_id,
            topology: None,
        };
        ResourceSet {
            cpus: 16,
            memory_mb: 1024,
            devices: vec![gpu(0, "mi300x"), gpu(1, "mi300x"), gpu(2, "mi250x")],
            generic: [("bandwidth:lustre".to_string(), 100)].into(),
            ..Default::default()
        }
    }

    #[test]
    fn device_counts_sum_every_type_of_a_gres() {
        let mut req = ResourceRequest::default();
        req.add_devices("gpu", Some("mi300x"), 2);
        req.add_devices("gpu", None, 1);
        req.add_devices("gpu", Some("mi300x"), 1);
        req.add_devices("nic", None, 4);
        assert_eq!(req.device_count("gpu"), 4);
        assert_eq!(req.device_count("nic"), 4);
        assert_eq!(req.device_count("license"), 0);
    }

    #[test]
    fn setting_a_gres_replaces_every_earlier_request_for_it() {
        let mut req = ResourceRequest::default();
        req.add_devices("gpu", Some("mi300x"), 2);
        req.add_devices("nic", None, 1);
        req.set_devices("gpu", None, 3);
        assert_eq!(
            req.devices,
            [
                (DeviceKey::new("gpu", None), 3),
                (DeviceKey::new("nic", None), 1)
            ]
            .into()
        );
        req.set_devices("gpu", None, 0);
        assert_eq!(req.device_count("gpu"), 0);
        assert!(!req.devices.keys().any(|k| k.gres_name == "gpu"));
    }

    #[test]
    fn an_untyped_device_request_takes_any_type() {
        let inv = mixed_inventory();
        let mut three = ResourceRequest::default();
        three.add_devices("gpu", None, 3);
        assert!(inv.can_satisfy(&three));
        let mut four = ResourceRequest::default();
        four.add_devices("gpu", None, 4);
        assert!(!inv.can_satisfy(&four));
    }

    #[test]
    fn a_typed_device_request_takes_only_its_type() {
        let inv = mixed_inventory();
        let mut two = ResourceRequest::default();
        two.add_devices("gpu", Some("mi250x"), 2);
        assert!(!inv.can_satisfy(&two));

        let mut one = ResourceRequest::default();
        one.add_devices("gpu", Some("mi250x"), 1);
        let alloc = build_node_allocation(&inv, &ResourceAllocations::default(), &one);
        assert_eq!(alloc.device_ids("gpu"), vec![3]);
    }

    #[test]
    fn a_whole_node_request_asks_for_everything_the_node_has() {
        let inv = mixed_inventory();
        let req = inv.as_request();
        assert_eq!((req.cpus, req.memory_mb), (16, 1024));
        assert_eq!(
            req.devices,
            [
                (DeviceKey::new("bandwidth", Some("lustre")), 100),
                (DeviceKey::new("gpu", Some("mi250x")), 1),
                (DeviceKey::new("gpu", Some("mi300x")), 2),
            ]
            .into()
        );
        assert!(inv.can_satisfy(&req));
        let one_gpu_held = ResourceAllocations::from_device_ids("gpu", &[1]);
        assert!(!inv.can_satisfy_with_allocated(&one_gpu_held, &req));
    }

    #[test]
    fn a_countable_gres_request_draws_on_the_generic_pool() {
        let inv = mixed_inventory();
        let mut req = ResourceRequest::default();
        req.add_devices("bandwidth", Some("lustre"), 60);
        assert!(inv.can_satisfy(&req));

        let alloc = build_node_allocation(&inv, &ResourceAllocations::default(), &req);
        assert_eq!(alloc.generic_count("bandwidth:lustre"), 60);
        assert!(!inv.can_satisfy_with_allocated(&alloc, &req));
    }
}

#[cfg(test)]
mod device_inventory_tests {
    use super::*;
    use crate::device::Device;

    /// The inventory types as the last GPU-only build defined them.
    mod prev {
        use super::super::{DeviceTopology, GpuLinkType};
        use serde::{Deserialize, Serialize};
        use std::collections::HashMap;

        #[derive(Debug, Serialize, Deserialize)]
        pub struct GpuResource {
            pub device_id: u32,
            pub gpu_type: String,
            pub memory_mb: u64,
            pub peer_gpus: Vec<u32>,
            pub link_type: GpuLinkType,
            #[serde(default)]
            pub stable_id: u64,
            #[serde(default)]
            pub topology: Option<DeviceTopology>,
            #[serde(default)]
            pub numa_node: Option<u32>,
            #[serde(default)]
            pub affinity_cpus: Vec<u32>,
        }

        #[derive(Debug, Serialize, Deserialize)]
        pub struct ResourceSet {
            pub cpus: u32,
            pub memory_mb: u64,
            pub gpus: Vec<GpuResource>,
            pub generic: HashMap<String, u64>,
            #[serde(default)]
            pub generation: u64,
            #[serde(default)]
            pub fabric_binding_version: u32,
            #[serde(default)]
            pub block_file_revision: Option<String>,
            #[serde(default)]
            pub topology: Option<crate::cpu_topology::CpuTopology>,
        }
    }

    fn nic(stable_id: u64, index: u32) -> Device {
        Device::new("nic", "pollara400", stable_id, index)
    }

    fn gpu_and_nic_node() -> ResourceSet {
        ResourceSet {
            cpus: 16,
            memory_mb: 1024,
            devices: vec![
                Device::gpu(0x110000, "mi300x", 0),
                Device::gpu(0x120000, "mi300x", 1),
                nic(0x050000, 0),
                nic(0x850000, 1),
            ],
            ..Default::default()
        }
    }

    fn node_with_shadowing_pools() -> ResourceSet {
        let mut node = gpu_and_nic_node();
        node.generic.extend([
            ("nic".to_string(), 4),
            ("nic:pollara400".to_string(), 2),
            ("gpu".to_string(), 8),
            ("bandwidth:lustre".to_string(), 100),
        ]);
        node
    }

    #[test]
    fn an_exclusive_allocation_skips_pools_named_like_a_device_kind() {
        let alloc = build_exclusive_allocation(&node_with_shadowing_pools(), 0);
        assert!(!alloc.devices.contains_key("nic:pollara400"));
        assert!(alloc.devices["nic"].iter().all(|d| d.count == 1));
        assert!(alloc.devices["gpu"].iter().all(|d| d.count == 1));
        assert_eq!(
            alloc.devices["bandwidth:lustre"],
            vec![AllocatedDevice {
                device_id: 0,
                count: 100
            }]
        );
    }

    #[test]
    fn an_exclusive_allocation_holds_exactly_what_the_whole_node_request_asks_for() {
        let node = node_with_shadowing_pools();
        let request = node.as_request();
        assert!(node.can_satisfy(&request));
        let alloc = build_exclusive_allocation(&node, 0);
        let mut held: Vec<String> = alloc.devices.keys().cloned().collect();
        held.sort();
        let mut asked: Vec<String> = request
            .devices
            .keys()
            .map(|key| {
                if node.allocates_by_id(&key.gres_name) {
                    key.gres_name.clone()
                } else {
                    key.generic_key()
                }
            })
            .collect();
        asked.sort();
        asked.dedup();
        assert_eq!(held, asked);
    }

    fn nic_request(untyped: u32, typed: u32) -> ResourceRequest {
        let mut request = ResourceRequest::with_scalar(1, 0);
        request.add_devices("nic", None, untyped);
        request.add_devices("nic", Some("pollara400"), typed);
        request
    }

    #[test]
    fn typed_and_untyped_requests_for_one_gres_share_its_devices() {
        let node = gpu_and_nic_node();
        assert!(node.can_satisfy(&nic_request(1, 1)));
        assert!(
            !node.can_satisfy(&nic_request(2, 1)),
            "three NICs cannot come from two"
        );
    }

    #[test]
    fn a_typed_request_gets_its_type_before_an_untyped_one_takes_it() {
        let node = ResourceSet {
            cpus: 4,
            devices: vec![nic(0x050000, 0), Device::new("nic", "cx7", 0x060000, 1)],
            ..Default::default()
        };
        let request = nic_request(1, 1);
        assert!(node.can_satisfy(&request));
        let alloc = build_node_allocation(&node, &ResourceAllocations::default(), &request);
        let mut ids = alloc.device_ids("nic");
        ids.sort();
        assert_eq!(ids, vec![0x050000, 0x060000]);
    }

    #[test]
    fn an_exclusive_job_fits_a_node_with_a_countable_pool_named_like_a_device_kind() {
        let node = ResourceSet {
            cpus: 4,
            memory_mb: 1024,
            devices: vec![nic(0x050000, 0)],
            generic: HashMap::from([("gpu".to_string(), 8), ("nic".to_string(), 4)]),
            ..Default::default()
        };
        assert!(node.can_satisfy(&node.as_request()));
    }

    #[test]
    fn a_legacy_gpu_inventory_reads_into_devices() {
        let json = r#"{"cpus": 8, "memory_mb": 1024, "generic": {},
            "gpus": [{"device_id": 0, "gpu_type": "mi300x", "memory_mb": 0, "peer_gpus": [],
                      "link_type": "XGMI", "stable_id": 1114112}]}"#;
        let set: ResourceSet = serde_json::from_str(json).expect("legacy inventory");
        assert_eq!(set.devices, vec![Device::gpu(0x110000, "mi300x", 0)]);
    }

    #[test]
    fn a_legacy_gpu_entry_keeps_its_numa_node_and_affinity_cpus() {
        let json = r#"{"cpus": 8, "memory_mb": 1024, "generic": {},
            "gpus": [{"device_id": 0, "gpu_type": "mi355x", "memory_mb": 0, "peer_gpus": [],
                      "link_type": "PCIe", "stable_id": 5242880, "topology": null,
                      "numa_node": 1, "affinity_cpus": [8, 9]}]}"#;
        let set: ResourceSet = serde_json::from_str(json).expect("post-1275 inventory");
        assert_eq!(set.devices[0].numa_node, Some(1));
        assert_eq!(set.devices[0].affinity_cpus, vec![8, 9]);
        let back = serde_json::to_value(&set).unwrap();
        assert_eq!(back["gpus"][0]["numa_node"], 1);
        assert_eq!(back["gpus"][0]["affinity_cpus"], serde_json::json!([8, 9]));
    }

    #[test]
    fn devices_win_over_legacy_gpus_when_both_are_present() {
        let json = r#"{"cpus": 8, "memory_mb": 1024, "generic": {},
            "devices": [{"gres_name": "nic", "device_type": "pollara400", "stable_id": 7, "index": 0}],
            "gpus": [{"device_id": 0, "gpu_type": "mi300x", "memory_mb": 0, "peer_gpus": [],
                      "link_type": "XGMI", "stable_id": 1114112}]}"#;
        let set: ResourceSet = serde_json::from_str(json).unwrap();
        assert_eq!(set.devices, vec![nic(7, 0)]);
    }

    #[test]
    fn a_new_inventory_still_reads_with_the_previous_build() {
        let json = serde_json::to_string(&gpu_and_nic_node()).unwrap();
        let old: prev::ResourceSet =
            serde_json::from_str(&json).expect("a rolled-back controller must replay it");
        let ids: Vec<u64> = old.gpus.iter().map(|g| g.stable_id).collect();
        assert_eq!(
            ids,
            vec![0x110000, 0x120000],
            "only GPUs, which it understands"
        );
        assert_eq!(old.gpus[1].device_id, 1);
    }

    #[test]
    fn an_inventory_round_trips_with_every_device_kind() {
        let set = gpu_and_nic_node();
        let back: ResourceSet =
            serde_json::from_str(&serde_json::to_string(&set).unwrap()).unwrap();
        assert_eq!(back, set);
    }

    #[test]
    fn an_untyped_nic_request_takes_typed_nics_by_id() {
        let node = gpu_and_nic_node();
        let mut req = ResourceRequest::with_scalar(1, 0);
        req.add_devices("nic", None, 2);
        assert!(node.can_satisfy(&req));

        let alloc = build_node_allocation(&node, &ResourceAllocations::default(), &req);
        assert_eq!(alloc.device_ids("nic"), vec![0x050000, 0x850000]);
        assert!(!node.can_satisfy_with_allocated(&alloc, &req));
    }

    #[test]
    fn a_typed_nic_request_takes_only_its_type() {
        let node = gpu_and_nic_node();
        let mut req = ResourceRequest::default();
        req.add_devices("nic", Some("pollara400-vf"), 1);
        assert!(!node.can_satisfy(&req));
    }

    #[test]
    fn a_node_holding_a_nic_it_no_longer_has_offers_none() {
        let node = gpu_and_nic_node();
        let orphaned = ResourceAllocations::from_device_ids("nic", &[0x990000]);
        assert!(node.available_device_ids(&orphaned, "nic", None).is_empty());
        assert_eq!(
            node.available_device_ids(&orphaned, "gpu", None).len(),
            2,
            "a lost NIC says nothing about the GPUs"
        );
    }

    #[test]
    fn an_exclusive_allocation_takes_every_device_of_every_kind() {
        let alloc = build_exclusive_allocation(&gpu_and_nic_node(), 512);
        assert_eq!(alloc.device_ids("gpu"), vec![0x110000, 0x120000]);
        assert_eq!(alloc.device_ids("nic"), vec![0x050000, 0x850000]);
    }
}

#[cfg(test)]
mod convergence_fields_tests {
    use super::*;

    #[test]
    fn resource_set_deserializes_without_generation() {
        let json = r#"{"cpus":8,"memory_mb":1024,"gpus":[],"generic":{}}"#;
        let r: ResourceSet = serde_json::from_str(json).unwrap();
        assert_eq!(r.generation, 0);
    }

    #[test]
    fn reserved_cpus_count_only_under_their_own_layout() {
        let alloc = ResourceAllocations {
            reserved_cpus: 8,
            layout_fingerprint: 42,
            ..Default::default()
        };
        assert_eq!(alloc.reserved_under(42), 8);
        assert_eq!(
            alloc.reserved_under(43),
            0,
            "an older layout is all general"
        );
    }
}
