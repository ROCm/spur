// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;

use anyhow::{bail, Result};
use tracing::{info, warn};

use crate::cdi::cache::CdiCache;
use crate::gres::cache::CountableGresPool;
use crate::gres::cache::{ExpandedGresDevice, GresCache};
use crate::inject::{
    build_injection_plans, ContainerInjectionPlan, HostInjectionPlan, InjectActions,
};
use crate::registry::entry::{entry_matches_gres_name, from_cdi, DeviceCapability, DeviceEntry};
use crate::types::DeviceSelection;

pub struct DeviceRegistry {
    devices: Vec<DeviceEntry>,
}

impl DeviceRegistry {
    pub fn new() -> Self {
        Self {
            devices: Vec::new(),
        }
    }

    pub fn populate(&mut self, cdi_cache: &CdiCache, gres_cache: &GresCache) {
        self.devices.clear();
        let mut candidates = Vec::new();

        let mut cdi_entries: Vec<DeviceEntry> = cdi_cache
            .iter()
            .map(|(_, cached)| from_cdi(cached))
            .collect();
        cdi_entries.sort_by(cdi_sort_key);

        let mut configured = Vec::new();
        for expanded in &gres_cache.injectable {
            let entry = expanded_to_device_entry(expanded);
            if !merge_into_discovered(&entry, &mut cdi_entries) {
                configured.push(entry);
            }
        }
        candidates.extend(cdi_entries);
        candidates.extend(configured);

        for pool in &gres_cache.countable {
            candidates.push(countable_pool_entry(pool));
        }

        // Dedup catches the remaining exact device path matches. CDI is filled first, so it wins.
        self.devices = dedup_by_device_paths(candidates);
        assign_device_ids(&mut self.devices);
    }

    pub fn resolve_by_ids(&self, name: &str, device_ids: &[u64]) -> Result<Vec<&DeviceEntry>> {
        let mut resolved = Vec::new();
        for &id in device_ids {
            let found = self.devices.iter().find(|e| {
                e.is_injectable() && entry_matches_gres_name(e, name) && e.stable_id == id
            });
            match found {
                Some(entry) => resolved.push(entry),
                None => bail!(
                    "allocated {} device ID {} not found in device registry",
                    name,
                    id
                ),
            }
        }
        Ok(resolved)
    }

    /// Build one host/container injection plan covering every allocated kind.
    pub fn build_job_injection_plans(
        &self,
        selection: &DeviceSelection,
        job_uid: u32,
        job_gid: u32,
    ) -> Result<(HostInjectionPlan, ContainerInjectionPlan)> {
        let mut devices = Vec::new();
        for (gres_name, ids) in selection {
            devices.extend(self.resolve_by_ids(gres_name, ids)?);
        }
        Ok(build_injection_plans(&devices, job_uid, job_gid))
    }

    pub fn has_injectable(&self, gres_name: &str) -> bool {
        self.devices
            .iter()
            .any(|e| e.is_injectable() && e.gres_name == gres_name)
    }

    /// The registry to inject from: `pinned` ids keep `previous`'s device even
    /// where the new layout reuses the id; unreported `gres_name` ones drop.
    pub fn with_pinned(
        mut self,
        previous: &DeviceRegistry,
        gres_name: &str,
        reported: &std::collections::HashSet<u64>,
        pinned: &std::collections::HashSet<u64>,
    ) -> DeviceRegistry {
        // Exact name, as the reported inventory is built from it.
        let is_gres = |e: &DeviceEntry| e.is_injectable() && e.gres_name == gres_name;
        self.devices.retain(|e| {
            !is_gres(e) || (reported.contains(&e.stable_id) && !pinned.contains(&e.stable_id))
        });
        let kept: Vec<DeviceEntry> = previous
            .devices
            .iter()
            .filter(|e| is_gres(e) && pinned.contains(&e.stable_id))
            .cloned()
            .collect();
        self.devices.extend(kept);
        self
    }

    pub fn list(&self) -> &[DeviceEntry] {
        &self.devices
    }

    pub fn injectable_count(&self) -> usize {
        self.devices.iter().filter(|d| d.is_injectable()).count()
    }

    pub fn countable_count(&self) -> usize {
        self.devices
            .iter()
            .filter(|d| d.is_countable_pool())
            .count()
    }
}

impl Default for DeviceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn expanded_to_device_entry(expanded: &ExpandedGresDevice) -> DeviceEntry {
    DeviceEntry {
        gres_name: expanded.gres_name.clone(),
        resource_type: expanded.resource_type.clone(),
        kind: "spur.internal/gres".into(),
        device_id: 0,
        capability: DeviceCapability::Injectable,
        memory_mb: 0,
        links: expanded.links.clone(),
        link_type: None,
        pci_bdf: None,
        vendor: expanded.vendor.clone(),
        inject_actions: expanded.inject_actions.clone(),
        capacity: 1,
        device_edits: expanded.device_edits.clone(),
        shared_edits: expanded.shared_edits.clone(),
        topology: None,
        stable_id: 0,
        numa_node: None,
        cores: expanded.cores.clone(),
    }
}

fn countable_pool_entry(pool: &CountableGresPool) -> DeviceEntry {
    DeviceEntry {
        gres_name: pool.gres_name.clone(),
        resource_type: pool.resource_type.clone(),
        kind: "spur.internal/gres".into(),
        device_id: 0,
        capability: DeviceCapability::CountablePool,
        memory_mb: 0,
        links: None,
        link_type: None,
        pci_bdf: None,
        vendor: None,
        inject_actions: InjectActions::default(),
        capacity: pool.capacity,
        device_edits: Default::default(),
        shared_edits: Default::default(),
        topology: None,
        stable_id: 0,
        numa_node: None,
        cores: None,
    }
}

fn assign_device_ids(devices: &mut [DeviceEntry]) {
    let mut next: HashMap<String, u32> = HashMap::new();
    for entry in devices.iter_mut() {
        if !entry.is_injectable() {
            continue;
        }
        let counter = next.entry(entry.gres_name.clone()).or_insert(0);
        entry.device_id = *counter;
        *counter += 1;
        // Paths without a render-minor annotation (annotation-less CDI, GRES,
        // countable/expanded) leave stable_id at 0; back it with the positional
        // id so every injectable device has a distinct, nonzero-collision id.
        if entry.stable_id == 0 {
            entry.stable_id = entry.device_id as u64;
        }
    }
}

fn cdi_sort_key(a: &DeviceEntry, b: &DeviceEntry) -> std::cmp::Ordering {
    match (&a.pci_bdf, &b.pci_bdf) {
        // Partitions of one GPU share a BDF; tie-break so a refresh keeps the order.
        (Some(a_bdf), Some(b_bdf)) => a_bdf
            .cmp(b_bdf)
            .then(a.stable_id.cmp(&b.stable_id))
            .then_with(|| primary_device_path(a).cmp(primary_device_path(b))),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => primary_device_path(a).cmp(primary_device_path(b)),
    }
}

fn primary_device_path(entry: &DeviceEntry) -> &str {
    entry.node_paths().next().unwrap_or("")
}

/// Merges a configured entry into the discovered device whose render node it names
/// (exact-path dedup would miss it, since a GPU also carries a `cardN` node).
fn merge_into_discovered(configured: &DeviceEntry, discovered: &mut [DeviceEntry]) -> bool {
    let paths: Vec<&str> = configured
        .device_edits
        .device_nodes
        .iter()
        .map(|dn| dn.effective_host_path())
        .collect();
    let mut matched: Vec<&str> = Vec::new();
    for device in discovered
        .iter_mut()
        .filter(|device| entry_matches_gres_name(device, &configured.gres_name))
    {
        let Some(render_node) = paths.iter().copied().find(|path| {
            path.starts_with("/dev/dri/renderD")
                && device
                    .device_edits
                    .device_nodes
                    .iter()
                    .any(|dn| dn.effective_host_path() == *path)
        }) else {
            continue;
        };
        matched.push(render_node);
        if configured.resource_type.is_some() {
            device.resource_type = configured.resource_type.clone();
        }
        if configured.cores.is_some() {
            device.cores = configured.cores.clone();
        }
        if configured.links.is_some() {
            device.links = configured.links.clone();
        }
        device.inject_actions.merge(&configured.inject_actions);
    }
    if matched.is_empty() {
        return false;
    }
    let unmatched: Vec<&str> = paths
        .iter()
        .copied()
        .filter(|p| p.starts_with("/dev/dri/renderD") && !matched.contains(p))
        .collect();
    if unmatched.is_empty() {
        info!(paths = ?paths, "configured GPU merged into the discovered one");
    } else {
        warn!(
            matched = ?matched,
            unmatched = ?unmatched,
            "configured GPU entry merged into discovered GPUs; render nodes no GPU was \
             discovered for are dropped with it"
        );
    }
    true
}

/// Skips a later entry when its device-node paths exactly match one already kept.
fn dedup_by_device_paths(candidates: Vec<DeviceEntry>) -> Vec<DeviceEntry> {
    let mut seen: Vec<(Vec<String>, String)> = Vec::new();
    let mut result = Vec::new();

    for entry in candidates {
        let mut per_device_paths: Vec<String> = entry.node_paths().map(str::to_owned).collect();
        per_device_paths.sort();

        if per_device_paths.is_empty() {
            result.push(entry);
            continue;
        }

        if let Some((_, existing_kind)) = seen.iter().find(|(p, _)| *p == per_device_paths) {
            warn!(
                duplicate_kind = %entry.kind,
                existing_kind = %existing_kind,
                paths = ?per_device_paths,
                "duplicate device (same device paths), skipping"
            );
            continue;
        }

        seen.push((per_device_paths, entry.kind.clone()));
        result.push(entry);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cdi::annotations;
    use crate::cdi::spec::{CdiDevice, CdiSpec, ContainerEdits, DeviceNode};
    use crate::gres::cache::GresCache;
    use crate::gres::entry::GresEntry;
    use crate::inject::merge_edits_from_devices;
    use crate::registry::entry::kind_matches_gres_name;
    use std::collections::HashSet;

    fn populate_registry(
        reg: &mut DeviceRegistry,
        cdi_cache: &CdiCache,
        gres_entries: &[GresEntry],
    ) {
        let gres_cache = GresCache::from_entries(gres_entries);
        reg.populate(cdi_cache, &gres_cache);
    }

    fn partition_entry(stable_id: u64, render_node: &str) -> DeviceEntry {
        let pool = crate::gres::cache::CountableGresPool {
            gres_name: "gpu".into(),
            resource_type: None,
            capacity: 1,
        };
        let mut entry = countable_pool_entry(&pool);
        entry.pci_bdf = Some("0000:11:00.0".into());
        entry.stable_id = stable_id;
        entry.device_edits.device_nodes.push(DeviceNode {
            path: render_node.into(),
            host_path: None,
            r#type: None,
            major: None,
            minor: None,
            file_mode: None,
            permissions: None,
            uid: None,
            gid: None,
        });
        entry
    }

    fn node(path: &str) -> DeviceNode {
        DeviceNode {
            path: path.into(),
            host_path: None,
            r#type: None,
            major: None,
            minor: None,
            file_mode: None,
            permissions: None,
            uid: None,
            gid: None,
        }
    }

    fn spec(kind: &str, devices: &[(&str, &str)], shared: ContainerEdits) -> CdiSpec {
        CdiSpec {
            cdi_version: "0.6.0".into(),
            kind: kind.into(),
            annotations: Default::default(),
            devices: devices
                .iter()
                .map(|(name, path)| CdiDevice {
                    name: (*name).into(),
                    annotations: Default::default(),
                    container_edits: Some(ContainerEdits {
                        device_nodes: vec![node(path)],
                        ..Default::default()
                    }),
                })
                .collect(),
            container_edits: (!shared.is_empty()).then_some(shared),
        }
    }

    fn shared_nodes(paths: &[&str]) -> ContainerEdits {
        ContainerEdits {
            device_nodes: paths.iter().map(|p| node(p)).collect(),
            ..Default::default()
        }
    }

    fn registry_of(specs: &[CdiSpec]) -> DeviceRegistry {
        let mut cache = CdiCache::new();
        cache.add_specs(specs);
        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &[]);
        reg
    }

    fn selection(entries: &[(&str, &[u64])]) -> DeviceSelection {
        entries
            .iter()
            .map(|(gres, ids)| ((*gres).to_string(), ids.to_vec()))
            .collect()
    }

    #[test]
    fn one_plan_covers_every_allocated_kind() {
        let reg = registry_of(&[
            spec(
                "amd.com/gpu",
                &[("0", "/dev/dri/renderD128"), ("1", "/dev/dri/renderD129")],
                shared_nodes(&["/dev/kfd"]),
            ),
            spec(
                "example.com/nic",
                &[("0", "/dev/infiniband/uverbs0")],
                shared_nodes(&["/dev/infiniband/rdma_cm"]),
            ),
        ]);

        let (host, container) = reg
            .build_job_injection_plans(&selection(&[("gpu", &[0, 1]), ("nic", &[0])]), 1000, 1000)
            .unwrap();

        assert_eq!(host.env.get("ROCR_VISIBLE_DEVICES").unwrap(), "0,1");
        for path in [
            "/dev/dri/renderD128",
            "/dev/dri/renderD129",
            "/dev/kfd",
            "/dev/infiniband/uverbs0",
            "/dev/infiniband/rdma_cm",
        ] {
            assert!(host.device_paths.contains(&path.to_string()), "{path}");
        }
        assert_eq!(container.device_nodes.len(), 5);
    }

    #[test]
    fn a_non_gpu_amd_kind_sets_no_gpu_visibility() {
        let reg = registry_of(&[spec(
            "amd.com/nic",
            &[("0", "/dev/infiniband/uverbs0")],
            ContainerEdits::default(),
        )]);

        let (host, container) = reg
            .build_job_injection_plans(&selection(&[("nic", &[0])]), 1000, 1000)
            .unwrap();

        assert!(
            !host.env.contains_key("ROCR_VISIBLE_DEVICES"),
            "an empty ROCR list would hide the job's GPUs"
        );
        assert!(!container.env.contains_key("ROCR_VISIBLE_DEVICES"));
    }

    #[test]
    fn two_specs_of_one_kind_both_contribute_their_shared_edits() {
        let reg = registry_of(&[
            spec(
                "amd.com/gpu",
                &[("0", "/dev/dri/renderD128")],
                shared_nodes(&["/dev/kfd"]),
            ),
            spec(
                "amd.com/gpu",
                &[("extra", "/dev/dri/renderD200")],
                shared_nodes(&["/dev/vendor-ctl"]),
            ),
        ]);

        let (host, _) = reg
            .build_job_injection_plans(&selection(&[("gpu", &[0, 1])]), 1000, 1000)
            .unwrap();

        assert!(host.device_paths.contains(&"/dev/kfd".to_string()));
        assert!(host.device_paths.contains(&"/dev/vendor-ctl".to_string()));
    }

    #[test]
    fn injectable_kinds_are_reported_by_gres_name() {
        let reg = registry_of(&[spec(
            "amd.com/nic",
            &[("0", "/dev/infiniband/uverbs0")],
            ContainerEdits::default(),
        )]);
        assert!(reg.has_injectable("nic"));
        assert!(!reg.has_injectable("gpu"));
    }

    #[test]
    fn a_pinned_id_the_new_layout_reuses_keeps_its_old_device() {
        let gpu = |stable_id: u64, node: &str| {
            let mut entry = partition_entry(stable_id, node);
            entry.capability = DeviceCapability::Injectable;
            entry
        };
        let previous = DeviceRegistry {
            devices: vec![
                gpu(0x110000, "/dev/dri/renderD128"),
                gpu(0x110001, "/dev/dri/renderD129"),
            ],
        };
        let candidate = DeviceRegistry {
            devices: vec![
                gpu(0x110000, "/dev/dri/renderD200"),
                gpu(0x110001, "/dev/dri/renderD201"),
            ],
        };
        let pinned: HashSet<u64> = [0x110000].into();

        let merged = candidate.with_pinned(&previous, "gpu", &pinned, &pinned);

        let entries: Vec<(u64, &str)> = merged
            .list()
            .iter()
            .map(|e| (e.stable_id, primary_device_path(e)))
            .collect();
        assert_eq!(entries, vec![(0x110000, "/dev/dri/renderD128")]);
    }

    #[test]
    fn pinned_ids_come_from_the_previous_registry_and_only_reported_ids_stay() {
        let gpu = |stable_id: u64, node: &str| {
            let mut entry = partition_entry(stable_id, node);
            entry.capability = DeviceCapability::Injectable;
            entry
        };
        let mut nic = gpu(0x110003, "/dev/infiniband/uverbs0");
        nic.gres_name = "nic".into();
        let previous = DeviceRegistry {
            devices: vec![
                gpu(0x110000, "/dev/dri/renderD128"),
                gpu(0x120300, "/dev/dri/renderD131"),
                gpu(0x110005, "/dev/dri/renderD133"),
            ],
        };
        let candidate = DeviceRegistry {
            devices: vec![
                gpu(0x110000, "/dev/dri/renderD200"),
                gpu(0x110001, "/dev/dri/renderD201"),
                gpu(0x120000, "/dev/dri/renderD202"),
                nic,
            ],
        };
        let reported: HashSet<u64> = [0x110000, 0x110001, 0x120300].into();
        let pinned: HashSet<u64> = [0x120300].into();

        let merged = candidate.with_pinned(&previous, "gpu", &reported, &pinned);

        let entries: Vec<(u64, &str)> = merged
            .list()
            .iter()
            .map(|e| (e.stable_id, primary_device_path(e)))
            .collect();
        assert_eq!(
            entries,
            vec![
                (0x110000, "/dev/dri/renderD200"),
                (0x110001, "/dev/dri/renderD201"),
                (0x110003, "/dev/infiniband/uverbs0"),
                (0x120300, "/dev/dri/renderD131"),
            ]
        );
    }

    #[test]
    fn cdi_sort_key_orders_partitions_of_one_bdf() {
        let mut entries = [
            partition_entry(0x110002, "/dev/dri/renderD130"),
            partition_entry(0, "/dev/dri/renderD129"),
            partition_entry(0x110001, "/dev/dri/renderD131"),
            partition_entry(0, "/dev/dri/renderD128"),
        ];
        entries.sort_by(cdi_sort_key);
        let order: Vec<(u64, &str)> = entries
            .iter()
            .map(|e| (e.stable_id, primary_device_path(e)))
            .collect();
        assert_eq!(
            order,
            vec![
                (0, "/dev/dri/renderD128"),
                (0, "/dev/dri/renderD129"),
                (0x110001, "/dev/dri/renderD131"),
                (0x110002, "/dev/dri/renderD130"),
            ]
        );
    }

    fn make_amd_cache() -> CdiCache {
        let spec = CdiSpec {
            cdi_version: "0.6.0".into(),
            kind: "amd.com/gpu".into(),
            annotations: Default::default(),
            devices: vec![
                CdiDevice {
                    name: "0".into(),
                    annotations: [
                        (annotations::GPU_TYPE.into(), "mi300x".into()),
                        (annotations::MEMORY_MB.into(), "196608".into()),
                        (annotations::AUTO_DETECTED.into(), "true".into()),
                    ]
                    .into(),
                    container_edits: Some(ContainerEdits {
                        device_nodes: vec![DeviceNode {
                            path: "/dev/dri/renderD128".into(),
                            host_path: None,
                            r#type: None,
                            major: None,
                            minor: None,
                            file_mode: None,
                            permissions: None,
                            uid: None,
                            gid: None,
                        }],
                        ..Default::default()
                    }),
                },
                CdiDevice {
                    name: "1".into(),
                    annotations: [
                        (annotations::GPU_TYPE.into(), "mi300x".into()),
                        (annotations::MEMORY_MB.into(), "196608".into()),
                        (annotations::AUTO_DETECTED.into(), "true".into()),
                    ]
                    .into(),
                    container_edits: Some(ContainerEdits {
                        device_nodes: vec![DeviceNode {
                            path: "/dev/dri/renderD129".into(),
                            host_path: None,
                            r#type: None,
                            major: None,
                            minor: None,
                            file_mode: None,
                            permissions: None,
                            uid: None,
                            gid: None,
                        }],
                        ..Default::default()
                    }),
                },
            ],
            container_edits: Some(ContainerEdits {
                device_nodes: vec![DeviceNode {
                    path: "/dev/kfd".into(),
                    host_path: None,
                    r#type: None,
                    major: None,
                    minor: None,
                    file_mode: None,
                    permissions: None,
                    uid: None,
                    gid: None,
                }],
                ..Default::default()
            }),
        };

        let mut cache = CdiCache::new();
        cache.add_specs(&[spec]);
        cache
    }

    #[test]
    fn test_populate_from_cdi_cache() {
        let cache = make_amd_cache();
        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &[]);

        assert_eq!(reg.injectable_count(), 2);
        assert_eq!(reg.countable_count(), 0);
    }

    #[test]
    fn test_populate_link_type_from_annotation() {
        let spec = CdiSpec {
            cdi_version: "0.6.0".into(),
            kind: "amd.com/gpu".into(),
            annotations: Default::default(),
            devices: vec![CdiDevice {
                name: "0".into(),
                annotations: [(annotations::LINK_TYPE.into(), "xgmi".into())].into(),
                container_edits: Some(ContainerEdits {
                    device_nodes: vec![DeviceNode {
                        path: "/dev/dri/renderD128".into(),
                        host_path: None,
                        r#type: None,
                        major: None,
                        minor: None,
                        file_mode: None,
                        permissions: None,
                        uid: None,
                        gid: None,
                    }],
                    ..Default::default()
                }),
            }],
            container_edits: None,
        };

        let mut cache = CdiCache::new();
        cache.add_specs(&[spec]);
        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &[]);

        let gpu = reg.resolve_by_ids("gpu", &[0]).unwrap()[0];
        assert_eq!(gpu.vendor.as_deref(), Some("amd"));
        assert_eq!(gpu.link_type, Some(crate::types::LinkType::Xgmi));
        assert_eq!(
            crate::registry::entry::resolve_link_type(gpu),
            crate::types::LinkType::Xgmi
        );
    }

    #[test]
    fn test_populate_with_countable_gres() {
        let cache = CdiCache::new();
        let gres = vec![GresEntry {
            name: "bandwidth".into(),
            r#type: Some("lustre".into()),
            count: Some(4096),
            flags: vec!["count_only".into()],
            ..Default::default()
        }];

        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &gres);

        assert_eq!(reg.injectable_count(), 0);
        assert_eq!(reg.countable_count(), 1);
    }

    /// On real KFD nodes the allocation ids the agent hands us are stable_ids
    /// (render_minor 128..), while the driver's positional visible index is
    /// `device_id` (0..). Resolution must key on stable_id, and the resolved
    /// entry must still carry its positional device_id for visibility emission.
    fn make_amd_cache_with_render_minors() -> CdiCache {
        let mut spec = CdiSpec {
            cdi_version: "0.6.0".into(),
            kind: "amd.com/gpu".into(),
            annotations: Default::default(),
            devices: Vec::new(),
            container_edits: None,
        };
        for (name, render_path, minor) in [
            ("0", "/dev/dri/renderD128", "128"),
            ("1", "/dev/dri/renderD129", "129"),
        ] {
            spec.devices.push(CdiDevice {
                name: name.into(),
                annotations: [
                    (annotations::RENDER_MINOR.into(), minor.into()),
                    (annotations::STABLE_ID.into(), minor.into()),
                ]
                .into(),
                container_edits: Some(ContainerEdits {
                    device_nodes: vec![DeviceNode {
                        path: render_path.into(),
                        host_path: None,
                        r#type: None,
                        major: None,
                        minor: None,
                        file_mode: None,
                        permissions: None,
                        uid: None,
                        gid: None,
                    }],
                    ..Default::default()
                }),
            });
        }
        let mut cache = CdiCache::new();
        cache.add_specs(&[spec]);
        cache
    }

    #[test]
    fn resolve_by_ids_keys_on_stable_id_not_positional_device_id() {
        let cache = make_amd_cache_with_render_minors();
        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &[]);

        // Positional device_id 0 carries stable_id 128; resolve by stable_id.
        let resolved = reg.resolve_by_ids("gpu", &[128]).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].stable_id, 128);
        assert_eq!(
            resolved[0].device_id, 0,
            "resolved entry keeps its positional device_id for visibility"
        );

        // The positional index is no longer a valid allocation id.
        assert!(
            reg.resolve_by_ids("gpu", &[0]).is_err(),
            "resolution must not match on positional device_id"
        );

        // Emitted visibility ids are the positional device_ids, not stable_ids.
        let (host_plan, _) = build_injection_plans(&resolved, 1000, 1000);
        assert_eq!(host_plan.env.get("ROCR_VISIBLE_DEVICES").unwrap(), "0");
        assert_eq!(host_plan.env.get("SPUR_JOB_GPUS").unwrap(), "0");
    }

    /// Annotation-less CDI devices arrive with stable_id 0. Without a backfill
    /// both GPUs would share id 0, so available_device_ids returns [0,0] and a
    /// 2-GPU allocation collides on the same index. assign_device_ids must give
    /// each injectable entry a distinct stable_id (here matching device_id).
    #[test]
    fn assign_backfills_distinct_stable_ids_for_annotationless_devices() {
        // make_amd_cache carries no render-minor annotation, so stable_id would
        // stay 0 on both devices without the backfill.
        let cache = make_amd_cache();
        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &[]);

        let stable_ids: Vec<u64> = reg
            .list()
            .iter()
            .filter(|e| e.is_injectable())
            .map(|e| e.stable_id)
            .collect();

        assert_eq!(stable_ids.len(), 2);
        assert_eq!(
            stable_ids,
            vec![0, 1],
            "stable_ids are distinct, not both 0"
        );
        assert_eq!(
            stable_ids.iter().collect::<HashSet<_>>().len(),
            2,
            "a 2-GPU allocation would not collide"
        );
    }

    /// The KFD path annotates render_minor (nonzero); the backfill guard
    /// (`if stable_id == 0`) must leave those real identities untouched.
    #[test]
    fn assign_preserves_nonzero_kfd_render_minor_stable_ids() {
        let cache = make_amd_cache_with_render_minors();
        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &[]);

        let mut stable_ids: Vec<u64> = reg
            .list()
            .iter()
            .filter(|e| e.is_injectable())
            .map(|e| e.stable_id)
            .collect();
        stable_ids.sort_unstable();
        assert_eq!(stable_ids, vec![128, 129], "render-minor stable_ids kept");
    }

    #[test]
    fn test_resolve_by_ids() {
        let cache = make_amd_cache();
        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &[]);

        let resolved = reg.resolve_by_ids("gpu", &[0, 1]).unwrap();
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].device_id, 0);
        assert_eq!(resolved[1].device_id, 1);
        assert_eq!(resolved[0].kind, "amd.com/gpu");
    }

    #[test]
    fn test_resolve_by_ids_specific_selection() {
        let cache = make_amd_cache();
        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &[]);

        let resolved = reg.resolve_by_ids("gpu", &[1]).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].device_id, 1);
    }

    #[test]
    fn test_resolve_by_ids_missing_id() {
        let cache = make_amd_cache();
        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &[]);

        let result = reg.resolve_by_ids("gpu", &[0, 5]);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("device ID 5 not found"));
    }

    #[test]
    fn test_build_injection_plans() {
        let cache = make_amd_cache();
        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &[]);

        let devices = reg.resolve_by_ids("gpu", &[0, 1]).unwrap();
        let (host_plan, container_plan) = build_injection_plans(&devices, 1000, 1000);

        assert_eq!(host_plan.env.get("ROCR_VISIBLE_DEVICES").unwrap(), "0,1");
        assert_eq!(host_plan.env.get("SPUR_JOB_GPUS").unwrap(), "0,1");
        assert_eq!(
            container_plan.env.get("ROCR_VISIBLE_DEVICES").unwrap(),
            "0,1"
        );
        assert_eq!(container_plan.device_nodes.len(), 3);
        assert_eq!(container_plan.device_nodes[0].uid, Some(1000));
    }

    #[test]
    fn test_build_injection_plans_missing_id() {
        let cache = make_amd_cache();
        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &[]);

        let result = reg.resolve_by_ids("gpu", &[0, 5]);
        assert!(result.is_err());
    }

    #[test]
    fn test_build_job_injection_plans_no_devices_empty_registry() {
        let reg = DeviceRegistry::new();
        let (host_plan, container_plan) = reg
            .build_job_injection_plans(&DeviceSelection::new(), 1000, 1000)
            .unwrap();
        assert!(host_plan.env.is_empty());
        assert!(container_plan.env.is_empty());
    }

    #[test]
    fn test_build_job_injection_plans_no_devices_populated_registry() {
        let cache = make_amd_cache();
        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &[]);

        let (host_plan, container_plan) = reg
            .build_job_injection_plans(&DeviceSelection::new(), 1000, 1000)
            .unwrap();
        assert!(host_plan.env.is_empty());
        assert!(container_plan.env.is_empty());
    }

    #[test]
    fn test_build_job_injection_plans_with_devices() {
        let cache = make_amd_cache();
        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &[]);

        let (host_plan, container_plan) = reg
            .build_job_injection_plans(&selection(&[("gpu", &[0, 1])]), 1000, 1000)
            .unwrap();
        assert_eq!(host_plan.env.get("ROCR_VISIBLE_DEVICES").unwrap(), "0,1");
        assert_eq!(host_plan.env.get("SPUR_JOB_GPUS").unwrap(), "0,1");
        assert_eq!(
            container_plan.env.get("ROCR_VISIBLE_DEVICES").unwrap(),
            "0,1"
        );
    }

    #[test]
    fn test_edits_for_devices() {
        let cache = make_amd_cache();
        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &[]);

        let resolved = reg.resolve_by_ids("gpu", &[0, 1]).unwrap();
        let edits = merge_edits_from_devices(&resolved);

        assert_eq!(edits.device_edits.device_nodes.len(), 2);
        assert_eq!(edits.shared_edits.device_nodes.len(), 1);
        assert_eq!(edits.shared_edits.device_nodes[0].path, "/dev/kfd");
        assert!(edits
            .inject_actions
            .gpu_visibility_vars
            .contains("ROCR_VISIBLE_DEVICES"));
        assert!(!edits
            .inject_actions
            .gpu_visibility_vars
            .contains("CUDA_VISIBLE_DEVICES"));
    }

    #[test]
    fn test_merged_edits_all_device_paths() {
        let cache = make_amd_cache();
        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &[]);

        let resolved = reg.resolve_by_ids("gpu", &[0, 1]).unwrap();
        let edits = merge_edits_from_devices(&resolved);

        let paths = edits.all_device_paths();
        assert!(paths.contains(&"/dev/dri/renderD128"));
        assert!(paths.contains(&"/dev/dri/renderD129"));
        assert!(paths.contains(&"/dev/kfd"));
    }

    #[test]
    fn test_kind_matches_gres_name() {
        assert!(kind_matches_gres_name("amd.com/gpu", "gpu"));
        assert!(kind_matches_gres_name("intel.com/gpu", "gpu"));
        assert!(!kind_matches_gres_name("amd.com/gpu", "mps"));
        assert!(!kind_matches_gres_name("invalid", "gpu"));
    }

    #[test]
    fn test_dedup_same_device_paths() {
        let spec1 = CdiSpec {
            cdi_version: "0.6.0".into(),
            kind: "amd.com/gpu".into(),
            annotations: Default::default(),
            devices: vec![CdiDevice {
                name: "0".into(),
                annotations: Default::default(),
                container_edits: Some(ContainerEdits {
                    device_nodes: vec![
                        DeviceNode {
                            path: "/dev/dri/renderD128".into(),
                            host_path: None,
                            r#type: None,
                            major: None,
                            minor: None,
                            file_mode: None,
                            permissions: None,
                            uid: None,
                            gid: None,
                        },
                        DeviceNode {
                            path: "/dev/dri/card0".into(),
                            host_path: None,
                            r#type: None,
                            major: None,
                            minor: None,
                            file_mode: None,
                            permissions: None,
                            uid: None,
                            gid: None,
                        },
                    ],
                    ..Default::default()
                }),
            }],
            container_edits: None,
        };

        let spec2 = CdiSpec {
            cdi_version: "0.6.0".into(),
            kind: "custom.vendor/gpu".into(),
            annotations: Default::default(),
            devices: vec![CdiDevice {
                name: "gpu0".into(),
                annotations: Default::default(),
                container_edits: Some(ContainerEdits {
                    device_nodes: vec![
                        DeviceNode {
                            path: "/dev/dri/card0".into(),
                            host_path: None,
                            r#type: None,
                            major: None,
                            minor: None,
                            file_mode: None,
                            permissions: None,
                            uid: None,
                            gid: None,
                        },
                        DeviceNode {
                            path: "/dev/dri/renderD128".into(),
                            host_path: None,
                            r#type: None,
                            major: None,
                            minor: None,
                            file_mode: None,
                            permissions: None,
                            uid: None,
                            gid: None,
                        },
                    ],
                    ..Default::default()
                }),
            }],
            container_edits: None,
        };

        let mut cache = CdiCache::new();
        cache.add_specs(&[spec1, spec2]);

        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &[]);

        assert_eq!(reg.injectable_count(), 1);
    }

    #[test]
    fn test_no_dedup_different_device_paths() {
        let spec = CdiSpec {
            cdi_version: "0.6.0".into(),
            kind: "amd.com/gpu".into(),
            annotations: Default::default(),
            devices: vec![
                CdiDevice {
                    name: "0".into(),
                    annotations: Default::default(),
                    container_edits: Some(ContainerEdits {
                        device_nodes: vec![DeviceNode {
                            path: "/dev/dri/renderD128".into(),
                            host_path: None,
                            r#type: None,
                            major: None,
                            minor: None,
                            file_mode: None,
                            permissions: None,
                            uid: None,
                            gid: None,
                        }],
                        ..Default::default()
                    }),
                },
                CdiDevice {
                    name: "1".into(),
                    annotations: Default::default(),
                    container_edits: Some(ContainerEdits {
                        device_nodes: vec![DeviceNode {
                            path: "/dev/dri/renderD129".into(),
                            host_path: None,
                            r#type: None,
                            major: None,
                            minor: None,
                            file_mode: None,
                            permissions: None,
                            uid: None,
                            gid: None,
                        }],
                        ..Default::default()
                    }),
                },
            ],
            container_edits: None,
        };

        let mut cache = CdiCache::new();
        cache.add_specs(&[spec]);

        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &[]);

        assert_eq!(reg.injectable_count(), 2);
    }

    #[test]
    fn test_cdi_plus_countable_gres() {
        let cache = make_amd_cache();
        let gres = vec![GresEntry {
            name: "bandwidth".into(),
            r#type: Some("lustre".into()),
            count: Some(4096),
            flags: vec!["count_only".into()],
            ..Default::default()
        }];

        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &gres);

        assert_eq!(reg.injectable_count(), 2);
        assert_eq!(reg.countable_count(), 1);

        let resolved = reg.resolve_by_ids("gpu", &[0, 1]).unwrap();
        assert_eq!(resolved.len(), 2);
    }

    #[test]
    fn test_gres_file_overlaps_cdi_device() {
        let cache = make_amd_cache();

        let gres = vec![GresEntry {
            name: "gpu".into(),
            r#type: Some("mi300x".into()),
            file: Some("/dev/dri/renderD[128-129]".into()),
            count: Some(2),
            flags: vec![],
            ..Default::default()
        }];

        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &gres);

        assert_eq!(reg.injectable_count(), 2);
        assert_eq!(reg.countable_count(), 0);
    }

    #[test]
    fn test_gres_file_no_overlap_with_cdi() {
        let cache = make_amd_cache();

        let gres = vec![GresEntry {
            name: "gpu".into(),
            r#type: Some("mi300x".into()),
            file: Some("/dev/dri/renderD[200-201]".into()),
            count: Some(2),
            flags: vec!["amd_gpu_env".into()],
            ..Default::default()
        }];

        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &gres);

        assert_eq!(reg.injectable_count(), 4);
        assert!(reg.resolve_by_ids("gpu", &[0]).is_ok());
        assert!(reg.resolve_by_ids("gpu", &[2]).is_ok());

        let devices = reg.resolve_by_ids("gpu", &[2]).unwrap();
        let (host_plan, _) = build_injection_plans(&devices, 1000, 1000);
        assert_eq!(host_plan.env.get("ROCR_VISIBLE_DEVICES").unwrap(), "2");
    }

    #[test]
    fn test_dedup_cdi_wins_over_gres() {
        let cache = make_amd_cache();

        let gres = vec![GresEntry {
            name: "gpu".into(),
            file: Some("/dev/dri/renderD128".into()),
            flags: vec!["intel_gpu_env".into()],
            ..Default::default()
        }];

        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &gres);

        assert_eq!(reg.injectable_count(), 2);
        let gpu0 = reg.resolve_by_ids("gpu", &[0]).unwrap()[0];
        assert_eq!(gpu0.kind, "amd.com/gpu");
        assert!(gpu0
            .inject_actions
            .gpu_visibility_vars
            .contains("ROCR_VISIBLE_DEVICES"));
    }

    #[test]
    fn test_device_ids_deterministic() {
        let cache = make_amd_cache();
        let mut reg1 = DeviceRegistry::new();
        let mut reg2 = DeviceRegistry::new();
        populate_registry(&mut reg1, &cache, &[]);
        populate_registry(&mut reg2, &cache, &[]);

        let ids1: Vec<u32> = reg1
            .list()
            .iter()
            .filter(|e| e.is_injectable())
            .map(|e| e.device_id)
            .collect();
        let ids2: Vec<u32> = reg2
            .list()
            .iter()
            .filter(|e| e.is_injectable())
            .map(|e| e.device_id)
            .collect();
        assert_eq!(ids1, vec![0, 1]);
        assert_eq!(ids1, ids2);
    }

    #[test]
    fn test_mixed_cdi_countable_and_overlap() {
        let cache = make_amd_cache();

        let gres = vec![
            GresEntry {
                name: "bandwidth".into(),
                r#type: Some("lustre".into()),
                count: Some(4096),
                flags: vec!["count_only".into()],
                ..Default::default()
            },
            GresEntry {
                name: "gpu".into(),
                r#type: Some("mi300x".into()),
                file: Some("/dev/dri/renderD128".into()),
                count: Some(1),
                flags: vec![],
                ..Default::default()
            },
            GresEntry {
                name: "license".into(),
                r#type: Some("fluent".into()),
                count: Some(20),
                flags: vec!["count_only".into()],
                ..Default::default()
            },
        ];

        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &gres);

        assert_eq!(reg.injectable_count(), 2);
        assert_eq!(reg.countable_count(), 2);
    }

    #[test]
    fn test_multiple_files_device_injection_pipeline() {
        let cache = CdiCache::new();
        let gres = vec![GresEntry {
            name: "gpu".into(),
            r#type: Some("mi300x".into()),
            multiple_files: Some("/dev/dri/renderD128,/dev/dri/card0".into()),
            flags: vec!["amd_gpu_env".into()],
            ..Default::default()
        }];

        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &gres);

        assert_eq!(reg.injectable_count(), 1);
        let resolved = reg.resolve_by_ids("gpu", &[0]).unwrap();
        assert_eq!(resolved[0].device_edits.device_nodes.len(), 2);

        let edits = merge_edits_from_devices(&resolved);
        let paths = edits.all_device_paths();
        assert!(paths.contains(&"/dev/dri/renderD128"));
        assert!(paths.contains(&"/dev/dri/card0"));
        assert!(paths.contains(&"/dev/kfd"));

        let (host_plan, container_plan) = build_injection_plans(&resolved, 1000, 1000);
        assert_eq!(host_plan.env.get("ROCR_VISIBLE_DEVICES").unwrap(), "0");
        assert_eq!(container_plan.device_nodes.len(), 3);
    }

    /// Auto-detected GPUs with a sysfs-linked card each: `cardN` and
    /// `renderD{128+N}`, with an optional CDI cores annotation.
    fn discovered_gpus(cores_annotations: &[Option<&str>]) -> CdiCache {
        let devices = cores_annotations
            .iter()
            .enumerate()
            .map(|(i, cores)| {
                let mut annotations: HashMap<String, String> =
                    [(annotations::AUTO_DETECTED.into(), "true".into())].into();
                if let Some(cores) = cores {
                    annotations.insert(annotations::CORES.into(), (*cores).into());
                }
                CdiDevice {
                    name: i.to_string(),
                    annotations,
                    container_edits: Some(ContainerEdits {
                        device_nodes: vec![
                            node(&format!("/dev/dri/card{i}")),
                            node(&format!("/dev/dri/renderD{}", 128 + i)),
                        ],
                        ..Default::default()
                    }),
                }
            })
            .collect();
        let spec = CdiSpec {
            cdi_version: "0.6.0".into(),
            kind: "amd.com/gpu".into(),
            annotations: Default::default(),
            devices,
            container_edits: None,
        };
        let mut cache = CdiCache::new();
        cache.add_specs(&[spec]);
        cache
    }

    fn gpu_cores(reg: &DeviceRegistry) -> Vec<Option<Vec<u32>>> {
        reg.list()
            .iter()
            .filter(|e| e.is_injectable())
            .map(|e| e.cores.clone())
            .collect()
    }

    #[test]
    fn a_configured_render_node_merges_its_cores_onto_the_discovered_gpu() {
        let cache = discovered_gpus(&[None, None]);
        let gres = vec![GresEntry {
            name: "gpu".into(),
            file: Some("/dev/dri/renderD129".into()),
            cores: Some("64-71".into()),
            ..Default::default()
        }];

        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &gres);

        assert_eq!(reg.injectable_count(), 2, "no duplicate GPU");
        assert_eq!(
            gpu_cores(&reg),
            vec![None, Some((64..=71).collect::<Vec<u32>>())]
        );
    }

    #[test]
    fn configured_cores_beat_the_cdi_annotation() {
        let cache = discovered_gpus(&[Some("0-3"), Some("4-7")]);
        let gres = vec![GresEntry {
            name: "gpu".into(),
            file: Some("/dev/dri/renderD128".into()),
            cores: Some("32-35".into()),
            ..Default::default()
        }];

        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &gres);

        assert_eq!(
            gpu_cores(&reg),
            vec![Some(vec![32, 33, 34, 35]), Some(vec![4, 5, 6, 7])],
            "spur.conf wins on GPU 0; GPU 1 keeps its annotation"
        );
    }

    #[test]
    fn a_configured_entry_without_cores_keeps_the_annotation() {
        let cache = discovered_gpus(&[Some("0-3")]);
        let gres = vec![GresEntry {
            name: "gpu".into(),
            file: Some("/dev/dri/renderD128".into()),
            ..Default::default()
        }];

        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &gres);

        assert_eq!(gpu_cores(&reg), vec![Some(vec![0, 1, 2, 3])]);
    }

    #[test]
    fn an_entry_naming_several_discovered_gpus_merges_into_each() {
        let cache = discovered_gpus(&[None, None, None]);
        let gres = vec![GresEntry {
            name: "gpu".into(),
            multiple_files: Some("/dev/dri/renderD[128-129]".into()),
            cores: Some("8-9".into()),
            ..Default::default()
        }];

        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &gres);

        assert_eq!(
            gpu_cores(&reg),
            vec![Some(vec![8, 9]), Some(vec![8, 9]), None]
        );
    }

    #[test]
    fn an_entry_naming_no_render_node_stays_a_device_of_its_own() {
        let cache = discovered_gpus(&[None]);
        let gres = vec![GresEntry {
            name: "gpu".into(),
            file: Some("/dev/dri/card0".into()),
            cores: Some("0-1".into()),
            ..Default::default()
        }];

        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &gres);

        assert_eq!(reg.injectable_count(), 2, "only a render node merges");
        assert_eq!(gpu_cores(&reg), vec![None, Some(vec![0, 1])]);
    }

    #[test]
    fn a_file_range_merges_one_gpu_per_render_node() {
        let cache = discovered_gpus(&[None, None]);
        let gres = vec![GresEntry {
            name: "gpu".into(),
            file: Some("/dev/dri/renderD[128-129]".into()),
            cores: Some("0-1".into()),
            ..Default::default()
        }];

        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &gres);

        assert_eq!(reg.injectable_count(), 2);
        assert_eq!(gpu_cores(&reg), vec![Some(vec![0, 1]), Some(vec![0, 1])]);
    }

    #[test]
    fn configured_type_wins_over_the_discovered_one() {
        let cache = discovered_gpus(&[None]);
        let gres = vec![GresEntry {
            name: "gpu".into(),
            r#type: Some("mi325x".into()),
            file: Some("/dev/dri/renderD128".into()),
            ..Default::default()
        }];

        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &gres);

        let types: Vec<_> = reg.list().iter().map(|e| e.resource_type.clone()).collect();
        assert_eq!(types, vec![Some("mi325x".to_string())]);
    }

    #[test]
    fn an_entry_of_another_gres_name_does_not_merge() {
        let cache = discovered_gpus(&[None]);
        let gres = vec![GresEntry {
            name: "accel".into(),
            file: Some("/dev/dri/renderD128".into()),
            cores: Some("0-1".into()),
            ..Default::default()
        }];

        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &gres);

        assert_eq!(reg.injectable_count(), 2);
        assert_eq!(gpu_cores(&reg), vec![None, Some(vec![0, 1])]);
    }

    #[test]
    fn the_merge_matches_the_cdi_host_path() {
        let mut render = node("/dev/dri/renderD0");
        render.host_path = Some("/dev/dri/renderD129".into());
        let spec = CdiSpec {
            cdi_version: "0.6.0".into(),
            kind: "amd.com/gpu".into(),
            annotations: Default::default(),
            devices: vec![CdiDevice {
                name: "0".into(),
                annotations: Default::default(),
                container_edits: Some(ContainerEdits {
                    device_nodes: vec![node("/dev/dri/card1"), render],
                    ..Default::default()
                }),
            }],
            container_edits: None,
        };
        let mut cache = CdiCache::new();
        cache.add_specs(&[spec]);
        let gres = vec![GresEntry {
            name: "gpu".into(),
            file: Some("/dev/dri/renderD129".into()),
            cores: Some("4-5".into()),
            ..Default::default()
        }];

        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &gres);

        assert_eq!(gpu_cores(&reg), vec![Some(vec![4, 5])]);
    }
}
