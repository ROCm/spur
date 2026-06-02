// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;

use anyhow::{bail, Result};
use tracing::warn;

use crate::cdi::cache::CdiCache;
use crate::gres::cache::CountableGresPool;
use crate::gres::cache::{ExpandedGresDevice, GresCache};
use crate::inject::{
    apply_spur_job_device_env, device_ids_for_gres, merge_edits_from_devices,
    ContainerInjectionPlan, ContainerInjector, HostInjectionPlan, HostInjector, InjectActions,
};
use crate::registry::entry::{entry_matches_gres_name, from_cdi, DeviceCapability, DeviceEntry};

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
        candidates.extend(cdi_entries);

        for expanded in &gres_cache.injectable {
            candidates.push(expanded_to_device_entry(expanded));
        }

        for pool in &gres_cache.countable {
            candidates.push(countable_pool_entry(pool));
        }

        // Dedup only catches exact device path matches. CDI is filled first, so it wins.
        // If autodetect is on and GRES also lists the same GPUs, that's a config mistake we don't fix.
        self.devices = dedup_by_device_paths(candidates);
        assign_device_ids(&mut self.devices);

        for (i, entry) in self.devices.iter_mut().enumerate() {
            entry.index = i as u32;
        }
    }

    pub fn resolve_by_ids(&self, name: &str, device_ids: &[u32]) -> Result<Vec<&DeviceEntry>> {
        let mut resolved = Vec::new();
        for &id in device_ids {
            let found = self.devices.iter().find(|e| {
                e.is_injectable() && entry_matches_gres_name(e, name) && e.device_id == id
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

    pub fn gres_inject_actions(&self, gres_name: &str) -> InjectActions {
        let mut actions = InjectActions::default();
        for entry in self
            .devices
            .iter()
            .filter(|e| e.is_injectable() && entry_matches_gres_name(e, gres_name))
        {
            actions.merge(&entry.inject_actions);
        }
        actions
    }

    /// Build host/container injection plans for a job from allocated device IDs.
    pub fn build_job_injection_plans(
        &self,
        gres_name: &str,
        allocated_ids: &[u32],
        job_uid: u32,
        job_gid: u32,
    ) -> Result<(HostInjectionPlan, ContainerInjectionPlan)> {
        let devices = self.resolve_by_ids(gres_name, allocated_ids)?;
        let edits = merge_edits_from_devices(&devices);

        let gpu_ids = device_ids_for_gres(&devices, gres_name);
        let mut host = HostInjector::plan(&edits, &gpu_ids);
        let mut container = ContainerInjector::plan(&edits, &gpu_ids, job_uid, job_gid);
        apply_spur_job_device_env(&mut host.env, &devices);
        apply_spur_job_device_env(&mut container.env, &devices);
        Ok((host, container))
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
        index: 0,
        gres_name: expanded.gres_name.clone(),
        resource_type: expanded.resource_type.clone(),
        kind: "spur.internal/gres".into(),
        device_id: 0,
        capability: DeviceCapability::Injectable,
        memory_mb: 0,
        numa_node: None,
        cores: expanded.cores.clone(),
        links: expanded.links.clone(),
        link_type: None,
        pci_bdf: None,
        vendor: expanded.vendor.clone(),
        inject_actions: expanded.inject_actions.clone(),
        capacity: 1,
        allocated: 0,
        device_edits: expanded.device_edits.clone(),
        shared_edits: expanded.shared_edits.clone(),
        device_paths: expanded.device_paths.clone(),
    }
}

fn countable_pool_entry(pool: &CountableGresPool) -> DeviceEntry {
    DeviceEntry {
        index: 0,
        gres_name: pool.gres_name.clone(),
        resource_type: pool.resource_type.clone(),
        kind: "spur.internal/gres".into(),
        device_id: 0,
        capability: DeviceCapability::CountablePool,
        memory_mb: 0,
        numa_node: None,
        cores: None,
        links: None,
        link_type: None,
        pci_bdf: None,
        vendor: None,
        inject_actions: InjectActions::default(),
        capacity: pool.capacity,
        allocated: 0,
        device_edits: Default::default(),
        shared_edits: Default::default(),
        device_paths: Vec::new(),
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
    }
}

fn cdi_sort_key(a: &DeviceEntry, b: &DeviceEntry) -> std::cmp::Ordering {
    match (&a.pci_bdf, &b.pci_bdf) {
        (Some(a_bdf), Some(b_bdf)) => a_bdf.cmp(b_bdf),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => primary_device_path(a).cmp(primary_device_path(b)),
    }
}

fn primary_device_path(entry: &DeviceEntry) -> &str {
    entry
        .device_edits
        .device_nodes
        .first()
        .map(|dn| dn.effective_host_path())
        .unwrap_or("")
}

/// Skips a later entry when its device-node paths exactly match one already kept.
fn dedup_by_device_paths(candidates: Vec<DeviceEntry>) -> Vec<DeviceEntry> {
    let mut seen: Vec<(Vec<String>, String)> = Vec::new();
    let mut result = Vec::new();

    for entry in candidates {
        let mut per_device_paths: Vec<String> = entry
            .device_edits
            .device_nodes
            .iter()
            .map(|dn| dn.effective_host_path().to_owned())
            .collect();
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
    use crate::gres::entry::{expand_hostlist, GresEntry};
    use crate::inject::{build_injection_plans, merge_edits_from_devices};
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
            .build_job_injection_plans("gpu", &[], 1000, 1000)
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
            .build_job_injection_plans("gpu", &[], 1000, 1000)
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
            .build_job_injection_plans("gpu", &[0, 1], 1000, 1000)
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
    fn test_expand_hostlist() {
        assert_eq!(
            expand_hostlist("/dev/dri/renderD[128-130]"),
            vec![
                "/dev/dri/renderD128",
                "/dev/dri/renderD129",
                "/dev/dri/renderD130"
            ]
        );

        assert_eq!(
            expand_hostlist("/dev/block[0-2]"),
            vec!["/dev/block0", "/dev/block1", "/dev/block2"]
        );

        assert_eq!(expand_hostlist("/dev/kfd"), vec!["/dev/kfd"]);
    }

    #[test]
    fn test_inject_actions_merge() {
        let mut amd = InjectActions {
            gpu_visibility_vars: HashSet::from(["ROCR_VISIBLE_DEVICES".into()]),
        };
        let intel = InjectActions {
            gpu_visibility_vars: HashSet::from(["ZE_AFFINITY_MASK".into()]),
        };
        amd.merge(&intel);
        assert!(amd.gpu_visibility_vars.contains("ROCR_VISIBLE_DEVICES"));
        assert!(amd.gpu_visibility_vars.contains("ZE_AFFINITY_MASK"));
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
}
