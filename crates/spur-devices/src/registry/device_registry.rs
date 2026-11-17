// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeSet, HashMap};

use anyhow::{bail, Result};
use tracing::debug;

use crate::cdi::cache::CdiCache;
use crate::gres::cache::CountableGresPool;
use crate::gres::cache::{ExpandedGresDevice, GresCache};
use crate::inject::{
    build_injection_plans, ContainerInjectionPlan, HostInjectionPlan, InjectActions,
};
use crate::registry::entry::{entry_matches_gres_name, from_cdi, DeviceCapability, DeviceEntry};
use crate::selection::DeviceSelection;

/// A countable pool dropped because its name is handed out by device.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ShadowedPool {
    pub gres_name: String,
    pub resource_type: Option<String>,
}

/// How a configured entry or a candidate device was resolved. Kept as data
/// because the registry is rebuilt every refresh; the caller logs changes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum RegistryNote {
    /// Merged into the discovered devices it names.
    Merged { paths: Vec<String> },
    /// Merged, but these nodes are not on the devices it names.
    DroppedNodes {
        paths: Vec<String>,
        dropped: Vec<String>,
    },
    /// Names only nodes every device of its kind shares, so names no device.
    SharedOnly { paths: Vec<String> },
    /// Skipped as a duplicate of a device already kept.
    Duplicate {
        kind: String,
        existing_kind: String,
        paths: Vec<String>,
        rule: &'static str,
    },
}

pub struct DeviceRegistry {
    devices: Vec<DeviceEntry>,
    shadowed: Vec<ShadowedPool>,
    notes: Vec<RegistryNote>,
    skipped_discovery: BTreeSet<String>,
}

impl DeviceRegistry {
    pub fn new() -> Self {
        Self {
            devices: Vec::new(),
            shadowed: Vec::new(),
            notes: Vec::new(),
            skipped_discovery: BTreeSet::new(),
        }
    }

    pub fn populate(&mut self, cdi_cache: &CdiCache, gres_cache: &GresCache) {
        self.populate_with_history(cdi_cache, gres_cache, &BTreeSet::new());
    }

    /// `by_id_history` names kinds handed out by device before: their pools
    /// stay dropped even with no device of that name left, so jobs pend
    /// instead of running without the devices they asked for.
    pub fn populate_with_history(
        &mut self,
        cdi_cache: &CdiCache,
        gres_cache: &GresCache,
        by_id_history: &BTreeSet<String>,
    ) {
        self.devices.clear();
        self.notes.clear();
        self.skipped_discovery = cdi_cache.skipped_discovery().clone();
        let mut candidates = Vec::new();

        let mut cdi_entries: Vec<(String, DeviceEntry)> = cdi_cache
            .iter()
            .map(|(name, cached)| (name.clone(), from_cdi(cached)))
            .collect();
        sort_cdi_entries(&mut cdi_entries);
        // An aggregate shares every node with its parts, so it would make each
        // node name several devices and widen what a GRES entry merges into.
        let cdi_devices: Vec<DeviceEntry> = cdi_entries.iter().map(|(_, e)| e.clone()).collect();
        cdi_entries.retain(|(_, entry)| !is_aggregate(entry, &cdi_devices));

        let mut configured = Vec::new();
        for expanded in &gres_cache.injectable {
            let entry = expanded_to_device_entry(expanded);
            match merge_into_discovered(&entry, &mut cdi_entries) {
                Merge::Merged(notes) => self.notes.extend(notes),
                Merge::SharedOnly(note) => self.notes.push(note),
                Merge::Unmatched => configured.push(entry),
            }
        }
        candidates.extend(cdi_entries.into_iter().map(|(_, entry)| entry));
        candidates.extend(configured);

        for pool in &gres_cache.countable {
            candidates.push(countable_pool_entry(pool));
        }

        // CDI is filled first, so it wins over GRES naming the same device.
        let kept = dedup_by_device_paths(candidates, &mut self.notes);
        let (devices, shadowed) = drop_shadowed_pools(kept, by_id_history);
        self.devices = devices;
        self.shadowed = shadowed;
        self.notes.sort();
        self.notes.dedup();
        assign_device_ids(&mut self.devices);
    }

    pub fn shadowed_pools(&self) -> &[ShadowedPool] {
        &self.shadowed
    }

    pub fn notes(&self) -> &[RegistryNote] {
        &self.notes
    }

    pub fn by_id_names(&self) -> BTreeSet<String> {
        self.devices
            .iter()
            .filter(|e| e.is_injectable())
            .map(|e| e.gres_name.clone())
            .collect()
    }

    pub fn resolve_by_ids(&self, name: &str, device_ids: &[u64]) -> Result<Vec<&DeviceEntry>> {
        let mut resolved = Vec::new();
        for &id in device_ids {
            let found = self.devices.iter().find(|e| is_injectable_id(e, name, id));
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
        for (gres_name, ids) in selection.iter() {
            devices.extend(self.resolve_by_ids(gres_name, ids)?);
        }
        Ok(build_injection_plans(&devices, job_uid, job_gid))
    }

    /// Whether `key` (`name` or `name:type`) names one of this node's countable pools.
    pub fn has_countable_pool(&self, key: &str) -> bool {
        self.devices
            .iter()
            .filter(|e| e.is_countable_pool())
            .any(
                |e| match e.resource_type.as_deref().filter(|t| !t.is_empty()) {
                    Some(t) => key == format!("{}:{t}", e.gres_name),
                    None => key == e.gres_name,
                },
            )
    }

    pub fn has_injectable(&self, gres_name: &str) -> bool {
        self.devices
            .iter()
            .any(|e| e.is_injectable() && e.gres_name == gres_name)
    }

    /// Whether `stable_id` resolves for injection, as `resolve_by_ids` would.
    pub fn has_injectable_id(&self, gres_name: &str, stable_id: u64) -> bool {
        self.devices
            .iter()
            .any(|e| is_injectable_id(e, gres_name, stable_id))
    }

    /// The registry to inject from: `pinned` ids keep `previous`'s device even
    /// where the new layout reuses the id, and ids of a reported kind that the
    /// report left out drop. Kinds the report does not name are left as found.
    pub fn with_pinned(
        mut self,
        previous: &DeviceRegistry,
        reported: &DeviceSelection,
        pinned: &DeviceSelection,
    ) -> DeviceRegistry {
        let names_kind = |selection: &DeviceSelection, gres: &str| {
            selection.iter().any(|(kind, _)| kind == gres)
        };
        // Exact name, as the reported inventory is built from it.
        let is_pinned =
            |e: &DeviceEntry| e.is_injectable() && pinned.of(&e.gres_name).contains(&e.stable_id);
        self.devices.retain(|e| {
            if !e.is_injectable() || !names_kind(reported, &e.gres_name) {
                return true;
            }
            reported.of(&e.gres_name).contains(&e.stable_id) && !is_pinned(e)
        });
        let kept: Vec<DeviceEntry> = previous
            .devices
            .iter()
            .filter(|e| is_pinned(e))
            .cloned()
            .collect();
        self.devices.extend(kept);
        let (devices, shadowed) =
            drop_shadowed_pools(std::mem::take(&mut self.devices), &BTreeSet::new());
        self.devices = devices;
        self.shadowed.extend(shadowed);
        self.shadowed.sort();
        self.shadowed.dedup();
        self
    }

    /// Gres names an on-disk CDI spec supplies, so live discovery skipped them.
    pub fn skipped_discovery(&self) -> &BTreeSet<String> {
        &self.skipped_discovery
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
        kind: GRES_KIND.into(),
        device_id: 0,
        capability: DeviceCapability::Injectable,
        pci_bdf: None,
        vendor: expanded.vendor.clone(),
        inject_actions: expanded.inject_actions.clone(),
        capacity: 1,
        device_edits: expanded.device_edits.clone(),
        shared_edits: expanded.shared_edits.clone(),
        topology: None,
        stable_id: 0,
        rdma_device: None,
        numa_node: None,
        cores: expanded.cores.clone(),
    }
}

fn countable_pool_entry(pool: &CountableGresPool) -> DeviceEntry {
    DeviceEntry {
        gres_name: pool.gres_name.clone(),
        resource_type: pool.resource_type.clone(),
        kind: GRES_KIND.into(),
        device_id: 0,
        capability: DeviceCapability::CountablePool,
        pci_bdf: None,
        vendor: None,
        inject_actions: InjectActions::default(),
        capacity: pool.capacity,
        device_edits: Default::default(),
        shared_edits: Default::default(),
        topology: None,
        stable_id: 0,
        rdma_device: None,
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

/// The CDI cache iterates in hash order, so the qualified name settles ties.
fn sort_cdi_entries(entries: &mut [(String, DeviceEntry)]) {
    entries.sort_by(|(a_name, a), (b_name, b)| cdi_sort_key(a, b).then_with(|| a_name.cmp(b_name)));
}

enum Merge {
    Merged(Vec<RegistryNote>),
    SharedOnly(RegistryNote),
    Unmatched,
}

/// Merges a configured entry into the discovered devices of its name it names
/// (exact-path dedup would miss it, since a GPU also carries a `cardN` node).
/// Its nodes outside those devices are dropped; an entry naming only shared
/// nodes (`/dev/kfd`) names no device; any other entry stays for dedup.
fn merge_into_discovered(
    configured: &DeviceEntry,
    discovered: &mut [(String, DeviceEntry)],
) -> Merge {
    let paths: Vec<String> = configured.own_node_paths().map(str::to_owned).collect();
    if paths.is_empty() && configured.node_paths().next().is_some() {
        return Merge::SharedOnly(RegistryNote::SharedOnly {
            paths: configured.node_paths().map(str::to_owned).collect(),
        });
    }
    let MergeTargets { targets, dropped } = merge_targets(configured, discovered);
    if targets.is_empty() {
        return Merge::Unmatched;
    }
    let mut notes = vec![RegistryNote::Merged {
        paths: paths.clone(),
    }];
    if !dropped.is_empty() {
        notes.push(RegistryNote::DroppedNodes {
            paths,
            dropped: dropped.into_iter().map(str::to_owned).collect(),
        });
    }
    for &index in &targets {
        let device = &mut discovered[index].1;
        if configured.resource_type.is_some() {
            device.resource_type = configured.resource_type.clone();
        }
        if configured.cores.is_some() {
            device.cores = configured.cores.clone();
        }
        device.inject_actions.merge(&configured.inject_actions);
    }
    Merge::Merged(notes)
}

/// The discovered devices a configured entry names, by index, and its own nodes
/// none of them has.
struct MergeTargets<'a> {
    targets: Vec<usize>,
    dropped: Vec<&'a str>,
}

/// A node only one device has names that device; when the entry names no such
/// node, a shared one (a partitioned GPU's `cardN`) names every device behind it.
fn merge_targets<'a>(
    configured: &'a DeviceEntry,
    discovered: &[(String, DeviceEntry)],
) -> MergeTargets<'a> {
    let own: Vec<&str> = configured.own_node_paths().collect();
    let owners_of = |path: &str| -> Vec<usize> {
        discovered
            .iter()
            .enumerate()
            .filter(|(_, (_, device))| {
                entry_matches_gres_name(device, &configured.gres_name)
                    && device.node_paths().any(|p| p == path)
            })
            .map(|(index, _)| index)
            .collect()
    };
    let owners: Vec<Vec<usize>> = own.iter().map(|path| owners_of(path)).collect();
    let unique: Vec<usize> = owners
        .iter()
        .filter(|o| o.len() == 1)
        .map(|o| o[0])
        .collect();
    let mut targets = if unique.is_empty() {
        owners.iter().flatten().copied().collect()
    } else {
        unique
    };
    targets.sort_unstable();
    targets.dedup();
    let dropped = own
        .iter()
        .zip(&owners)
        .filter(|(_, o)| !o.iter().any(|index| targets.contains(index)))
        .map(|(path, _)| *path)
        .collect();
    MergeTargets { targets, dropped }
}

fn is_injectable_id(entry: &DeviceEntry, gres_name: &str, stable_id: u64) -> bool {
    entry.is_injectable()
        && entry_matches_gres_name(entry, gres_name)
        && entry.stable_id == stable_id
}

fn primary_device_path(entry: &DeviceEntry) -> &str {
    entry.node_paths().next().unwrap_or("")
}

const GRES_KIND: &str = "spur.internal/gres";

/// A countable pool named like a kind handed out by id would be counted by
/// the controller and ignored by the agent, so discovered devices win. GPUs are
/// always handed out by id, so a count-only `gpu` pool could never be used.
fn drop_shadowed_pools(
    devices: Vec<DeviceEntry>,
    by_id_history: &BTreeSet<String>,
) -> (Vec<DeviceEntry>, Vec<ShadowedPool>) {
    let by_id: BTreeSet<String> = devices
        .iter()
        .filter(|e| e.is_injectable())
        .map(|e| e.gres_name.clone())
        .chain(std::iter::once("gpu".to_string()))
        .chain(by_id_history.iter().cloned())
        .collect();
    let (shadowed, kept): (Vec<DeviceEntry>, Vec<DeviceEntry>) = devices
        .into_iter()
        .partition(|entry| entry.is_countable_pool() && by_id.contains(&entry.gres_name));
    let mut shadowed: Vec<ShadowedPool> = shadowed
        .into_iter()
        .map(|entry| ShadowedPool {
            gres_name: entry.gres_name,
            resource_type: entry.resource_type,
        })
        .collect();
    shadowed.sort();
    shadowed.dedup();
    (kept, shadowed)
}

fn sorted_node_paths(entry: &DeviceEntry) -> Vec<&str> {
    let mut paths: Vec<&str> = entry.node_paths().collect();
    paths.sort_unstable();
    paths.dedup();
    paths
}

/// A CDI device standing for several others of its kind (like `all`): its nodes
/// are exactly the union of two or more siblings' nodes.
fn is_aggregate(entry: &DeviceEntry, candidates: &[DeviceEntry]) -> bool {
    if entry.kind == GRES_KIND {
        return false;
    }
    let nodes = sorted_node_paths(entry);
    let mut covered: Vec<&str> = Vec::new();
    let mut parts = 0;
    for other in candidates.iter().filter(|o| o.kind == entry.kind) {
        let other_nodes = sorted_node_paths(other);
        let strict_subset = !other_nodes.is_empty()
            && other_nodes.len() < nodes.len()
            && other_nodes.iter().all(|p| nodes.contains(p));
        if strict_subset {
            parts += 1;
            covered.extend(other_nodes);
        }
    }
    covered.sort_unstable();
    covered.dedup();
    parts >= 2 && covered == nodes
}

/// Skips CDI aggregates, then a later entry naming a device already kept: the
/// same node set, or for a GRES entry, any node it owns that a kept one owns.
fn dedup_by_device_paths(
    candidates: Vec<DeviceEntry>,
    notes: &mut Vec<RegistryNote>,
) -> Vec<DeviceEntry> {
    let aggregates: Vec<bool> = candidates
        .iter()
        .map(|entry| is_aggregate(entry, &candidates))
        .collect();
    let mut seen: Vec<(Vec<String>, Vec<String>, String)> = Vec::new();
    let mut result = Vec::new();

    for (entry, aggregate) in candidates.into_iter().zip(aggregates) {
        if aggregate {
            debug!(kind = %entry.kind, paths = ?sorted_node_paths(&entry), "skipping a CDI device that aggregates others of its kind");
            continue;
        }
        let per_device_paths: Vec<String> = sorted_node_paths(&entry)
            .into_iter()
            .map(str::to_owned)
            .collect();
        if per_device_paths.is_empty() {
            result.push(entry);
            continue;
        }

        // A GRES entry sharing a node it owns with a CDI device describes that
        // device a second time. Otherwise devices are one only when they grant
        // exactly the same nodes: partitions share a card node, GRES entries may
        // share vendor control nodes, as in Slurm's MultipleFiles.
        let own: Vec<String> = entry.own_node_paths().map(str::to_owned).collect();
        let duplicate = seen.iter().find(|(paths, kept_own, kind)| {
            if *paths == per_device_paths {
                return true;
            }
            ((entry.kind == GRES_KIND) != (kind == GRES_KIND))
                && own.iter().any(|p| kept_own.contains(p))
        });
        if let Some((paths, _, existing_kind)) = duplicate {
            let rule = if *paths == per_device_paths {
                "exact"
            } else {
                "gres-overlap"
            };
            notes.push(RegistryNote::Duplicate {
                kind: entry.kind.clone(),
                existing_kind: existing_kind.clone(),
                paths: per_device_paths,
                rule,
            });
            continue;
        }

        seen.push((per_device_paths, own, entry.kind.clone()));
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
    use std::collections::{BTreeSet, HashSet};

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
            .fold(DeviceSelection::new(), |sel, (gres, ids)| {
                sel.with(gres, ids.to_vec())
            })
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
            ..DeviceRegistry::new()
        };
        let candidate = DeviceRegistry {
            devices: vec![
                gpu(0x110000, "/dev/dri/renderD200"),
                gpu(0x110001, "/dev/dri/renderD201"),
            ],
            ..DeviceRegistry::new()
        };
        let pinned = DeviceSelection::new().with("gpu", vec![0x110000]);

        let merged = candidate.with_pinned(&previous, &pinned, &pinned);

        let entries: Vec<(u64, &str)> = merged
            .list()
            .iter()
            .map(|e| (e.stable_id, primary_device_path(e)))
            .collect();
        assert_eq!(entries, vec![(0x110000, "/dev/dri/renderD128")]);
    }

    #[test]
    fn a_pinned_device_of_any_kind_keeps_its_previous_entry() {
        let nic = |stable_id: u64, node: &str| {
            let mut entry = partition_entry(stable_id, node);
            entry.capability = DeviceCapability::Injectable;
            entry.gres_name = "nic".into();
            entry
        };
        let previous = DeviceRegistry {
            devices: vec![
                nic(7, "/dev/infiniband/uverbs3"),
                nic(8, "/dev/infiniband/uverbs4"),
            ],
            ..DeviceRegistry::new()
        };
        let candidate = DeviceRegistry {
            devices: vec![nic(8, "/dev/infiniband/uverbs0")],
            ..DeviceRegistry::new()
        };
        let reported = DeviceSelection::new().with("nic", vec![7, 8]);
        let pinned = DeviceSelection::new().with("nic", vec![7]);

        let merged = candidate.with_pinned(&previous, &reported, &pinned);

        let entries: Vec<(u64, &str)> = merged
            .list()
            .iter()
            .map(|e| (e.stable_id, primary_device_path(e)))
            .collect();
        assert_eq!(
            entries,
            vec![
                (8, "/dev/infiniband/uverbs0"),
                (7, "/dev/infiniband/uverbs3")
            ]
        );
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
            ..DeviceRegistry::new()
        };
        let candidate = DeviceRegistry {
            devices: vec![
                gpu(0x110000, "/dev/dri/renderD200"),
                gpu(0x110001, "/dev/dri/renderD201"),
                gpu(0x120000, "/dev/dri/renderD202"),
                nic,
            ],
            ..DeviceRegistry::new()
        };
        let reported = DeviceSelection::new()
            .with("gpu", vec![0x110000, 0x110001, 0x120300])
            .with("nic", vec![0x110003]);
        let pinned = DeviceSelection::new().with("gpu", vec![0x120300]);

        let merged = candidate.with_pinned(&previous, &reported, &pinned);

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

    /// A CDI entry as the registry builds it, paired with its qualified name.
    fn cdi_entry(kind: &str, name: &str, paths: &[&str]) -> (String, DeviceEntry) {
        let cached = crate::cdi::cache::CachedDevice {
            qualified_name: format!("{kind}={name}"),
            device: CdiDevice {
                name: name.into(),
                annotations: Default::default(),
                container_edits: Some(ContainerEdits {
                    device_nodes: paths.iter().map(|p| node(p)).collect(),
                    ..Default::default()
                }),
            },
            kind: kind.into(),
            shared_edits: None,
            source_path: String::new(),
            priority: 0,
        };
        (cached.qualified_name.clone(), from_cdi(&cached))
    }

    fn sorted_then_deduped(mut entries: Vec<(String, DeviceEntry)>) -> Vec<DeviceEntry> {
        sort_cdi_entries(&mut entries);
        dedup_by_device_paths(
            entries.into_iter().map(|(_, e)| e).collect(),
            &mut Vec::new(),
        )
    }

    fn rotations<T: Clone>(items: &[T]) -> Vec<Vec<T>> {
        (0..items.len())
            .flat_map(|i| {
                let mut forward = items.to_vec();
                forward.rotate_left(i);
                let mut backward = forward.clone();
                backward.reverse();
                [forward, backward]
            })
            .collect()
    }

    #[test]
    fn a_per_gpu_spec_with_an_all_device_yields_each_gpu_once_in_any_order() {
        let mut entries: Vec<(String, DeviceEntry)> = (0..4)
            .map(|i| {
                cdi_entry(
                    "example.com/gpu",
                    &i.to_string(),
                    &[
                        &format!("/dev/dri/renderD{}", 128 + i),
                        &format!("/dev/dri/card{i}"),
                    ],
                )
            })
            .collect();
        let every: Vec<String> = entries
            .iter()
            .flat_map(|(_, e)| e.node_paths().map(str::to_owned).collect::<Vec<_>>())
            .collect();
        let every: Vec<&str> = every.iter().map(String::as_str).collect();
        entries.push(cdi_entry("example.com/gpu", "all", &every));

        for order in rotations(&entries) {
            let kept = sorted_then_deduped(order);
            assert_eq!(
                kept.len(),
                4,
                "the aggregate must not stand in for the GPUs"
            );
            assert!(kept.iter().all(|e| e.node_paths().count() == 2));
        }
    }

    #[test]
    fn a_single_gpu_spec_with_all_keeps_the_per_gpu_device() {
        let paths = ["/dev/dri/renderD128", "/dev/dri/card0"];
        for order in rotations(&[
            cdi_entry("example.com/gpu", "all", &paths),
            cdi_entry("example.com/gpu", "0", &paths),
        ]) {
            let mut sorted = order;
            sort_cdi_entries(&mut sorted);
            assert_eq!(sorted[0].0, "example.com/gpu=0");
            let kept = dedup_by_device_paths(
                sorted.into_iter().map(|(_, e)| e).collect(),
                &mut Vec::new(),
            );
            assert_eq!(kept.len(), 1);
        }
    }

    #[test]
    fn cdi_devices_of_two_kinds_sharing_some_nodes_both_stay() {
        let kept = sorted_then_deduped(vec![
            cdi_entry(
                "amd.com/gpu",
                "0",
                &["/dev/dri/renderD128", "/dev/dri/card0"],
            ),
            cdi_entry("other.vendor/gpu", "0", &["/dev/dri/renderD128"]),
        ]);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn an_exact_duplicate_across_kinds_keeps_the_same_kind_in_either_order() {
        let paths = ["/dev/dri/renderD128"];
        for order in rotations(&[
            cdi_entry("other.vendor/gpu", "0", &paths),
            cdi_entry("amd.com/gpu", "0", &paths),
        ]) {
            let kept = sorted_then_deduped(order);
            assert_eq!(kept.len(), 1);
            assert_eq!(kept[0].kind, "amd.com/gpu");
        }
    }

    fn count_only(name: &str, device_type: Option<&str>, count: u64) -> GresEntry {
        GresEntry {
            name: name.into(),
            r#type: device_type.map(str::to_string),
            count: Some(count),
            flags: vec!["count_only".into()],
            ..Default::default()
        }
    }

    fn nic_cache() -> CdiCache {
        let mut cache = CdiCache::new();
        cache.add_specs(&[spec(
            "amd.com/nic",
            &[
                ("0", "/dev/infiniband/uverbs0"),
                ("1", "/dev/infiniband/uverbs1"),
            ],
            ContainerEdits::default(),
        )]);
        cache
    }

    #[test]
    fn a_countable_pool_named_like_discovered_devices_is_dropped() {
        let mut reg = DeviceRegistry::new();
        populate_registry(
            &mut reg,
            &nic_cache(),
            &[
                count_only("nic", None, 4),
                count_only("nic", Some("pollara400"), 2),
            ],
        );
        assert_eq!((reg.injectable_count(), reg.countable_count()), (2, 0));
        assert_eq!(
            reg.shadowed_pools(),
            [shadowed("nic", None), shadowed("nic", Some("pollara400"))]
        );
    }

    #[test]
    fn a_count_only_gpu_pool_is_always_dropped() {
        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &CdiCache::new(), &[count_only("gpu", None, 8)]);
        assert_eq!(reg.countable_count(), 0);
        assert_eq!(reg.shadowed_pools(), [shadowed("gpu", None)]);
    }

    #[test]
    fn a_countable_pool_of_another_name_stays() {
        let mut reg = DeviceRegistry::new();
        populate_registry(
            &mut reg,
            &nic_cache(),
            &[count_only("bandwidth", Some("lustre"), 64)],
        );
        assert_eq!(reg.countable_count(), 1);
        assert!(reg.shadowed_pools().is_empty());
    }

    fn shadowed(gres_name: &str, resource_type: Option<&str>) -> ShadowedPool {
        ShadowedPool {
            gres_name: gres_name.into(),
            resource_type: resource_type.map(str::to_string),
        }
    }

    #[test]
    fn a_pool_of_a_kind_handed_out_before_stays_dropped_after_its_devices_vanish() {
        let pools = GresCache::from_entries(&[count_only("nic", None, 4)]);
        let mut reg = DeviceRegistry::new();
        reg.populate_with_history(
            &CdiCache::new(),
            &pools,
            &BTreeSet::from(["nic".to_string()]),
        );
        assert!(!reg.has_countable_pool("nic"));
        assert_eq!(reg.shadowed_pools(), [shadowed("nic", None)]);

        let mut fresh = DeviceRegistry::new();
        fresh.populate(&CdiCache::new(), &pools);
        assert!(fresh.has_countable_pool("nic"));
        assert!(fresh.shadowed_pools().is_empty());
    }

    #[test]
    fn a_pool_beside_pinned_devices_of_its_name_is_dropped() {
        let mut previous = DeviceRegistry::new();
        populate_registry(
            &mut previous,
            &CdiCache::new(),
            &[GresEntry {
                name: "nic".into(),
                file: Some("/dev/infiniband/uverbs[0-1]".into()),
                ..Default::default()
            }],
        );
        let mut candidate = DeviceRegistry::new();
        populate_registry(
            &mut candidate,
            &CdiCache::new(),
            &[count_only("nic", None, 4)],
        );

        let merged = candidate.with_pinned(
            &previous,
            &DeviceSelection::new().with("nic", vec![]),
            &DeviceSelection::new().with("nic", vec![1]),
        );

        assert!(merged.has_injectable("nic"));
        assert!(!merged.has_countable_pool("nic"));
        assert_eq!(merged.shadowed_pools(), [shadowed("nic", None)]);
    }

    #[test]
    fn by_id_names_lists_every_injectable_kind() {
        let mut reg = DeviceRegistry::new();
        populate_registry(
            &mut reg,
            &CdiCache::new(),
            &[
                GresEntry {
                    name: "gpu".into(),
                    file: Some("/dev/dri/renderD128".into()),
                    ..Default::default()
                },
                GresEntry {
                    name: "nic".into(),
                    file: Some("/dev/infiniband/uverbs0".into()),
                    ..Default::default()
                },
                count_only("bandwidth", None, 64),
            ],
        );
        assert_eq!(
            reg.by_id_names(),
            BTreeSet::from(["gpu".to_string(), "nic".to_string()])
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
    fn a_gres_gpu_naming_part_of_a_cdi_gpu_is_the_same_gpu() {
        let mut cache = CdiCache::new();
        cache.add_specs(&[CdiSpec {
            cdi_version: "0.6.0".into(),
            kind: "amd.com/gpu".into(),
            annotations: Default::default(),
            devices: ["128", "129"]
                .iter()
                .enumerate()
                .map(|(i, minor)| CdiDevice {
                    name: i.to_string(),
                    annotations: Default::default(),
                    container_edits: Some(ContainerEdits {
                        device_nodes: vec![
                            node(&format!("/dev/dri/renderD{minor}")),
                            node(&format!("/dev/dri/card{}", i + 1)),
                        ],
                        ..Default::default()
                    }),
                })
                .collect(),
            container_edits: None,
        }]);
        let gres = vec![GresEntry {
            name: "gpu".into(),
            file: Some("/dev/dri/renderD[128-129]".into()),
            ..Default::default()
        }];

        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &gres);
        let kinds: Vec<&str> = reg.list().iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, ["amd.com/gpu", "amd.com/gpu"]);
    }

    #[test]
    fn partitions_sharing_a_card_node_stay_distinct() {
        let mut cache = CdiCache::new();
        cache.add_specs(&[CdiSpec {
            cdi_version: "0.6.0".into(),
            kind: "amd.com/gpu".into(),
            annotations: Default::default(),
            devices: ["128", "129"]
                .iter()
                .enumerate()
                .map(|(i, minor)| CdiDevice {
                    name: i.to_string(),
                    annotations: Default::default(),
                    container_edits: Some(ContainerEdits {
                        device_nodes: vec![
                            node(&format!("/dev/dri/renderD{minor}")),
                            node("/dev/dri/card1"),
                        ],
                        ..Default::default()
                    }),
                })
                .collect(),
            container_edits: None,
        }]);
        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &[]);
        assert_eq!(reg.injectable_count(), 2);
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
    fn an_entry_naming_only_a_card_node_merges_into_its_gpu() {
        let cache = discovered_gpus(&[None]);
        let gres = vec![GresEntry {
            name: "gpu".into(),
            file: Some("/dev/dri/card0".into()),
            cores: Some("0-1".into()),
            ..Default::default()
        }];

        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &gres);

        assert_eq!(reg.injectable_count(), 1);
        assert_eq!(gpu_cores(&reg), vec![Some(vec![0, 1])]);
    }

    #[test]
    fn a_gres_entry_partly_inside_a_discovered_gpu_merges_and_drops_the_rest() {
        let cache = discovered_gpus(&[None]);
        let gres = vec![GresEntry {
            name: "gpu".into(),
            multiple_files: Some("/dev/dri/renderD128,/dev/dri/renderD200".into()),
            cores: Some("0-1".into()),
            ..Default::default()
        }];

        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &gres);

        assert_eq!(reg.injectable_count(), 1);
        assert_eq!(gpu_cores(&reg), vec![Some(vec![0, 1])]);
    }

    #[test]
    fn a_multi_file_entry_listing_kfd_merges_into_each_gpu_it_names() {
        let cache = discovered_gpus(&[None, None]);
        let gres = vec![GresEntry {
            name: "gpu".into(),
            r#type: Some("mi325x".into()),
            multiple_files: Some("/dev/dri/renderD128,/dev/dri/renderD129,/dev/kfd".into()),
            cores: Some("8-9".into()),
            ..Default::default()
        }];

        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &gres);

        assert_eq!(reg.injectable_count(), 2);
        let gpus: Vec<(Option<&str>, Option<Vec<u32>>)> = reg
            .list()
            .iter()
            .map(|e| (e.resource_type.as_deref(), e.cores.clone()))
            .collect();
        assert_eq!(gpus, vec![(Some("mi325x"), Some(vec![8, 9])); 2]);
    }

    #[test]
    fn a_single_gpu_entry_listing_kfd_merges_its_type_and_cores() {
        let cache = discovered_gpus(&[None]);
        let gres = vec![GresEntry {
            name: "gpu".into(),
            r#type: Some("mi325x".into()),
            multiple_files: Some("/dev/dri/renderD128,/dev/kfd".into()),
            cores: Some("0-1".into()),
            ..Default::default()
        }];

        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &gres);

        assert_eq!(reg.injectable_count(), 1);
        assert_eq!(reg.list()[0].resource_type.as_deref(), Some("mi325x"));
        assert_eq!(gpu_cores(&reg), vec![Some(vec![0, 1])]);
    }

    /// Two partitions of one GPU: own render nodes, one shared card node.
    fn partitioned_gpu() -> CdiCache {
        let mut cache = CdiCache::new();
        cache.add_specs(&[CdiSpec {
            cdi_version: "0.6.0".into(),
            kind: "amd.com/gpu".into(),
            annotations: Default::default(),
            devices: ["128", "129"]
                .iter()
                .enumerate()
                .map(|(i, minor)| CdiDevice {
                    name: i.to_string(),
                    annotations: Default::default(),
                    container_edits: Some(ContainerEdits {
                        device_nodes: vec![
                            node(&format!("/dev/dri/renderD{minor}")),
                            node("/dev/dri/card1"),
                        ],
                        ..Default::default()
                    }),
                })
                .collect(),
            container_edits: None,
        }]);
        cache
    }

    #[test]
    fn an_aggregate_cdi_device_does_not_widen_a_merge() {
        let mut cache = partitioned_gpu();
        cache.add_specs(&[CdiSpec {
            cdi_version: "0.6.0".into(),
            kind: "amd.com/gpu".into(),
            annotations: Default::default(),
            devices: vec![CdiDevice {
                name: "all".into(),
                annotations: Default::default(),
                container_edits: Some(ContainerEdits {
                    device_nodes: vec![
                        node("/dev/dri/renderD128"),
                        node("/dev/dri/renderD129"),
                        node("/dev/dri/card1"),
                    ],
                    ..Default::default()
                }),
            }],
            container_edits: None,
        }]);
        let gres = vec![GresEntry {
            name: "gpu".into(),
            multiple_files: Some("/dev/dri/renderD128,/dev/dri/card1".into()),
            cores: Some("0-1".into()),
            ..Default::default()
        }];

        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &gres);

        assert_eq!(reg.injectable_count(), 2);
        assert_eq!(gpu_cores(&reg), vec![Some(vec![0, 1]), None]);
    }

    #[test]
    fn a_render_node_names_one_partition_even_beside_the_shared_card() {
        let gres = vec![GresEntry {
            name: "gpu".into(),
            multiple_files: Some("/dev/dri/renderD128,/dev/dri/card1".into()),
            cores: Some("0-1".into()),
            ..Default::default()
        }];

        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &partitioned_gpu(), &gres);

        assert_eq!(gpu_cores(&reg), vec![Some(vec![0, 1]), None]);
    }

    #[test]
    fn a_card_node_shared_by_partitions_merges_into_each_partition() {
        let gres = vec![GresEntry {
            name: "gpu".into(),
            file: Some("/dev/dri/card1".into()),
            cores: Some("0-1".into()),
            ..Default::default()
        }];

        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &partitioned_gpu(), &gres);

        assert_eq!(reg.injectable_count(), 2);
        assert_eq!(gpu_cores(&reg), vec![Some(vec![0, 1]), Some(vec![0, 1])]);
    }

    #[test]
    fn a_gres_entry_of_another_name_partly_overlapping_a_gpu_is_dropped() {
        let cache = discovered_gpus(&[None]);
        let gres = vec![GresEntry {
            name: "accel".into(),
            multiple_files: Some("/dev/dri/renderD128,/dev/dri/renderD200".into()),
            ..Default::default()
        }];

        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &gres);

        assert_eq!(reg.injectable_count(), 1);
    }

    #[test]
    fn gres_entries_sharing_only_kfd_stay_distinct() {
        let gres = vec![
            GresEntry {
                name: "gpu".into(),
                multiple_files: Some("/dev/dri/renderD128,/dev/kfd".into()),
                ..Default::default()
            },
            GresEntry {
                name: "gpu".into(),
                multiple_files: Some("/dev/dri/renderD129,/dev/kfd".into()),
                ..Default::default()
            },
        ];

        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &CdiCache::new(), &gres);

        assert_eq!(reg.injectable_count(), 2);
    }

    fn gres_gpus(files: &[&str]) -> Vec<GresEntry> {
        files
            .iter()
            .map(|f| GresEntry {
                name: "gpu".into(),
                multiple_files: Some((*f).into()),
                ..Default::default()
            })
            .collect()
    }

    #[test]
    fn gres_partitions_sharing_a_card_node_stay_distinct() {
        let gres = gres_gpus(&[
            "/dev/dri/renderD128,/dev/dri/card1",
            "/dev/dri/renderD129,/dev/dri/card1",
        ]);
        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &CdiCache::new(), &gres);
        assert_eq!(reg.injectable_count(), 2);
    }

    #[test]
    fn identical_gres_entries_are_one_device() {
        let gres = gres_gpus(&[
            "/dev/dri/renderD128,/dev/dri/card1",
            "/dev/dri/renderD128,/dev/dri/card1",
        ]);
        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &CdiCache::new(), &gres);
        assert_eq!(reg.injectable_count(), 1);
    }

    #[test]
    fn gres_entries_sharing_a_control_node_stay_distinct() {
        let gres = gres_gpus(&["/dev/nvidia0,/dev/nvidiactl", "/dev/nvidia1,/dev/nvidiactl"]);
        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &CdiCache::new(), &gres);
        assert_eq!(reg.injectable_count(), 2);
    }

    #[test]
    fn a_gres_entry_naming_a_cdi_nics_node_merges_into_it() {
        let mut cache = CdiCache::new();
        cache.add_specs(&[spec(
            "amd.com/nic",
            &[("0", "/dev/infiniband/uverbs0")],
            ContainerEdits::default(),
        )]);
        let gres = vec![GresEntry {
            name: "nic".into(),
            r#type: Some("pollara400".into()),
            file: Some("/dev/infiniband/uverbs0".into()),
            cores: Some("4-5".into()),
            ..Default::default()
        }];

        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &gres);

        assert_eq!(reg.injectable_count(), 1);
        assert_eq!(reg.list()[0].resource_type.as_deref(), Some("pollara400"));
        assert_eq!(reg.list()[0].cores, Some(vec![4, 5]));
    }

    #[test]
    fn a_gres_entry_naming_only_unknown_nodes_stays_a_device() {
        let cache = discovered_gpus(&[None]);
        let gres = vec![GresEntry {
            name: "gpu".into(),
            file: Some("/dev/dri/renderD200".into()),
            ..Default::default()
        }];

        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &cache, &gres);

        assert_eq!(reg.injectable_count(), 2);
    }

    #[test]
    fn a_gres_entry_naming_only_shared_nodes_is_dropped() {
        let kfd_only = vec![GresEntry {
            name: "gpu".into(),
            file: Some("/dev/kfd".into()),
            ..Default::default()
        }];
        for cache in [discovered_gpus(&[None, None]), CdiCache::new()] {
            let before = cache.len();
            let mut reg = DeviceRegistry::new();
            populate_registry(&mut reg, &cache, &kfd_only);
            assert_eq!(reg.injectable_count(), before, "/dev/kfd names no GPU");
            assert_eq!(
                reg.notes(),
                [RegistryNote::SharedOnly {
                    paths: vec!["/dev/kfd".into()]
                }]
            );
        }
    }

    #[test]
    fn merges_and_dropped_nodes_are_recorded_not_logged() {
        let gres = vec![GresEntry {
            name: "gpu".into(),
            multiple_files: Some("/dev/dri/renderD128,/dev/dri/renderD200".into()),
            ..Default::default()
        }];
        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &discovered_gpus(&[None]), &gres);
        let paths = vec![
            "/dev/dri/renderD128".to_string(),
            "/dev/dri/renderD200".into(),
        ];
        assert_eq!(
            reg.notes(),
            [
                RegistryNote::Merged {
                    paths: paths.clone()
                },
                RegistryNote::DroppedNodes {
                    paths,
                    dropped: vec!["/dev/dri/renderD200".into()]
                },
            ]
        );
        populate_registry(&mut reg, &discovered_gpus(&[None]), &[]);
        assert!(reg.notes().is_empty(), "a rebuild starts from no notes");
    }

    fn gres_device(file: &str) -> DeviceEntry {
        let cache = GresCache::from_entries(&[GresEntry {
            name: "gpu".into(),
            multiple_files: Some(file.into()),
            ..Default::default()
        }]);
        expanded_to_device_entry(&cache.injectable[0])
    }

    fn partitions() -> Vec<(String, DeviceEntry)> {
        vec![
            cdi_entry(
                "amd.com/gpu",
                "0",
                &["/dev/dri/renderD128", "/dev/dri/card1"],
            ),
            cdi_entry(
                "amd.com/gpu",
                "1",
                &["/dev/dri/renderD129", "/dev/dri/card1"],
            ),
        ]
    }

    #[test]
    fn merge_targets_prefers_a_node_unique_to_one_device() {
        let configured = gres_device("/dev/dri/renderD128,/dev/dri/card1");
        let targets = merge_targets(&configured, &partitions());
        assert_eq!(targets.targets, vec![0]);
        assert!(targets.dropped.is_empty());
    }

    #[test]
    fn merge_targets_takes_every_owner_of_a_shared_node() {
        let configured = gres_device("/dev/dri/card1");
        assert_eq!(
            merge_targets(&configured, &partitions()).targets,
            vec![0, 1]
        );
    }

    #[test]
    fn merge_targets_lists_nodes_no_target_has() {
        let configured = gres_device("/dev/dri/renderD128,/dev/dri/renderD200");
        let targets = merge_targets(&configured, &partitions());
        assert_eq!(targets.targets, vec![0]);
        assert_eq!(targets.dropped, vec!["/dev/dri/renderD200"]);
    }

    #[test]
    fn merge_targets_ignores_shared_nodes() {
        let configured = gres_device("/dev/dri/renderD128,/dev/kfd");
        let targets = merge_targets(&configured, &partitions());
        assert_eq!(targets.targets, vec![0]);
        assert!(targets.dropped.is_empty());
    }

    #[test]
    fn merge_targets_is_empty_when_no_node_is_discovered() {
        let configured = gres_device("/dev/dri/renderD200");
        assert!(merge_targets(&configured, &partitions()).targets.is_empty());
    }

    #[test]
    fn a_nic_gres_entry_inside_a_discovered_nic_merges_type_and_cores() {
        let gres = vec![GresEntry {
            name: "nic".into(),
            r#type: Some("pollara400".into()),
            file: Some("/dev/infiniband/uverbs1".into()),
            cores: Some("4-5".into()),
            ..Default::default()
        }];

        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &nic_cache(), &gres);

        assert_eq!(reg.injectable_count(), 2, "no duplicate NIC");
        let nics: Vec<(Option<&str>, Option<Vec<u32>>)> = reg
            .list()
            .iter()
            .map(|e| (e.resource_type.as_deref(), e.cores.clone()))
            .collect();
        assert_eq!(
            nics,
            vec![(None, None), (Some("pollara400"), Some(vec![4, 5]))]
        );
    }

    #[test]
    fn a_gres_entry_of_another_name_does_not_merge_into_a_nic() {
        let gres = vec![GresEntry {
            name: "gpu".into(),
            file: Some("/dev/infiniband/uverbs1".into()),
            cores: Some("4-5".into()),
            ..Default::default()
        }];

        let mut reg = DeviceRegistry::new();
        populate_registry(&mut reg, &nic_cache(), &gres);

        assert!(reg.list().iter().all(|e| e.cores.is_none()));
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

        // Not merged, so the entry naming part of the GPU dedups away.
        assert_eq!(reg.injectable_count(), 1);
        assert_eq!(gpu_cores(&reg), vec![None]);
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
