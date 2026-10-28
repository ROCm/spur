// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Explicit fabric membership derived from a single controller inventory snapshot.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
pub use spur_devices::topology::{FabricObservation, ObservationStatus};

use crate::node::Node;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FabricBlockKey {
    pub namespace: String,
    pub physical_domain: String,
    pub fabric_domain: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FabricDeviceKey {
    pub node: String,
    pub stable_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FabricMember {
    pub device: FabricDeviceKey,
    pub endpoint: String,
    pub inventory_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FabricBlock {
    pub key: FabricBlockKey,
    pub members: Vec<FabricMember>,
    pub active_endpoints: Vec<String>,
    pub physical_capacity: Option<u32>,
    pub fabric_capacity: Option<u32>,
    /// Empty means complete, consistent, and backed by live registered nodes;
    /// it does not establish authoritative fabric health or launch safety.
    pub issues: Vec<String>,
    #[serde(default)]
    pub file_revision: Option<String>,
    #[serde(default)]
    pub observed_key: Option<FabricBlockKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnresolvedFabricDevice {
    pub device: FabricDeviceKey,
    pub issue: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockFileNotice {
    pub block_name: String,
    pub warning: bool,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FabricView {
    pub namespace: String,
    pub blocks: Vec<FabricBlock>,
    pub unresolved: Vec<UnresolvedFabricDevice>,
    #[serde(default)]
    pub notices: Vec<BlockFileNotice>,
}

pub fn has_native_fabric(resources: &crate::resource::ResourceSet) -> bool {
    resources.gpus.iter().any(|gpu| {
        gpu.topology
            .as_ref()
            .and_then(|t| t.fabric.as_ref())
            // Generic discovery failure markers also protect declaration-only GPUs.
            .is_some_and(|observation| observation.source != "device-discovery")
    })
}

fn nonempty(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|value| !value.trim().is_empty())
}

fn endpoint_set(values: &[String]) -> BTreeSet<&str> {
    values.iter().map(String::as_str).collect()
}

struct Report<'a> {
    node: &'a Node,
    stable_id: u64,
    pci_bdf: Option<&'a str>,
    observation: &'a FabricObservation,
}

impl FabricView {
    pub fn from_nodes(
        namespace: &str,
        nodes: &[Node],
        now: DateTime<Utc>,
        heartbeat_timeout_secs: u64,
    ) -> Self {
        let mut reports: BTreeMap<FabricBlockKey, Vec<Report<'_>>> = BTreeMap::new();
        let mut unresolved = Vec::new();
        let mut device_counts = BTreeMap::new();
        for node in nodes {
            for gpu in &node.total_resources.gpus {
                *device_counts
                    .entry(FabricDeviceKey {
                        node: node.name.clone(),
                        stable_id: gpu.stable_id,
                    })
                    .or_insert(0usize) += 1;
                let observation = gpu.topology.as_ref().and_then(|t| t.fabric.as_ref());
                let domains = observation.and_then(|o| {
                    Some((
                        nonempty(&o.physical_domain_id)?,
                        nonempty(&o.fabric_domain_id)?,
                    ))
                });
                let (Some(observation), Some((physical, fabric))) = (observation, domains) else {
                    unresolved.push(UnresolvedFabricDevice {
                        device: FabricDeviceKey {
                            node: node.name.clone(),
                            stable_id: gpu.stable_id,
                        },
                        issue: if observation.is_some() {
                            "missing_domain"
                        } else {
                            "missing_observation"
                        }
                        .into(),
                    });
                    continue;
                };
                let key = FabricBlockKey {
                    namespace: namespace.into(),
                    physical_domain: physical.into(),
                    fabric_domain: fabric.into(),
                };
                reports.entry(key).or_default().push(Report {
                    node,
                    stable_id: gpu.stable_id,
                    pci_bdf: gpu.topology.as_ref().and_then(|t| t.pci_bdf.as_deref()),
                    observation,
                });
            }
        }

        let mut blocks: Vec<_> = reports
            .into_iter()
            .map(|(key, reports)| assemble_block(key, &reports, now, heartbeat_timeout_secs))
            .collect();
        // Endpoint identity is physical-domain scoped, even when reports disagree
        // about which fabric partition currently owns it.
        let mut owners: BTreeMap<(String, String), BTreeSet<usize>> = BTreeMap::new();
        let mut capacities: BTreeMap<String, BTreeSet<Option<u32>>> = BTreeMap::new();
        for (index, block) in blocks.iter().enumerate() {
            capacities
                .entry(block.key.physical_domain.clone())
                .or_default()
                .insert(block.physical_capacity);
            for endpoint in &block.active_endpoints {
                owners
                    .entry((block.key.physical_domain.clone(), endpoint.clone()))
                    .or_default()
                    .insert(index);
            }
        }
        for indices in owners.values().filter(|indices| indices.len() > 1) {
            for &index in indices {
                blocks[index]
                    .issues
                    .push("endpoint_in_multiple_blocks".into());
            }
        }
        for block in &mut blocks {
            if block
                .members
                .iter()
                .any(|member| device_counts.get(&member.device).copied().unwrap_or(0) != 1)
            {
                block.issues.push("invalid_device_identity".into());
            }
            if capacities
                .get(&block.key.physical_domain)
                .is_some_and(|values| values.len() > 1)
            {
                block.issues.push("physical_capacity_conflict".into());
            }
            block.issues.sort();
            block.issues.dedup();
        }
        unresolved.sort_by(|a, b| a.device.cmp(&b.device).then(a.issue.cmp(&b.issue)));
        Self {
            namespace: namespace.into(),
            blocks,
            unresolved,
            notices: Vec::new(),
        }
    }
}

fn assemble_block(
    key: FabricBlockKey,
    reports: &[Report<'_>],
    now: DateTime<Utc>,
    heartbeat_timeout_secs: u64,
) -> FabricBlock {
    let mut issues = BTreeSet::new();
    let mut members = Vec::new();
    let mut identities = BTreeSet::new();
    let mut physical_capacities = BTreeSet::new();
    let mut fabric_capacities = BTreeSet::new();
    let mut active_views = BTreeSet::new();
    let mut transports = BTreeSet::new();
    let mut endpoints: BTreeMap<&str, BTreeSet<(&str, Option<&str>)>> = BTreeMap::new();
    let mut local_views: BTreeMap<&str, BTreeSet<BTreeSet<&str>>> = BTreeMap::new();
    if key.namespace.trim().is_empty() {
        issues.insert("missing_namespace");
    }
    for report in reports {
        let node = report.node;
        let observation = report.observation;
        let device = FabricDeviceKey {
            node: node.name.clone(),
            stable_id: report.stable_id,
        };
        if !identities.insert(device.clone()) || report.stable_id == 0 {
            issues.insert("invalid_device_identity");
        }
        if node.total_resources.generation == 0 {
            issues.insert("missing_inventory_generation");
        }
        if !node.state.is_up() {
            issues.insert("node_unavailable");
        }
        let fresh = node.last_heartbeat.is_some_and(|last| {
            let age = now.signed_duration_since(last);
            age >= chrono::Duration::zero()
                && age.num_seconds() < heartbeat_timeout_secs.min(i64::MAX as u64) as i64
        });
        if !fresh {
            issues.insert("stale_node");
        }
        if observation.source.trim().is_empty() {
            issues.insert("missing_source");
        }
        if observation.status != ObservationStatus::Observed || !observation.issues.is_empty() {
            issues.insert("observation_not_consistent");
        }
        physical_capacities.insert(observation.physical_capacity);
        fabric_capacities.insert(observation.fabric_capacity);
        let active = endpoint_set(&observation.active_endpoints);
        let local = endpoint_set(&observation.local_endpoints);
        if active.is_empty() || active.contains("") || local.is_empty() || !local.is_subset(&active)
        {
            issues.insert("invalid_endpoint_membership");
        }
        if observation
            .fabric_capacity
            .is_none_or(|capacity| capacity == 0 || active.len() > capacity as usize)
            || observation.physical_capacity.is_none_or(|capacity| {
                capacity == 0
                    || observation
                        .fabric_capacity
                        .is_some_and(|fabric| fabric > capacity)
            })
        {
            issues.insert("invalid_capacity");
        }
        transports.insert(observation.transport.as_deref());
        if let Some(endpoint) = nonempty(&observation.endpoint_id) {
            if !local.contains(endpoint) {
                issues.insert("endpoint_not_local");
            }
            endpoints
                .entry(endpoint)
                .or_default()
                .insert((&node.name, report.pci_bdf));
            members.push(FabricMember {
                device,
                endpoint: endpoint.into(),
                inventory_generation: node.total_resources.generation,
            });
        } else {
            issues.insert("missing_endpoint");
        }
        active_views.insert(active);
        local_views.entry(&node.name).or_default().insert(local);
    }
    if physical_capacities.len() != 1 {
        issues.insert("physical_capacity_conflict");
    }
    if fabric_capacities.len() != 1 {
        issues.insert("fabric_capacity_conflict");
    }
    if active_views.len() != 1 {
        issues.insert("active_membership_conflict");
    }
    if transports.len() != 1 {
        issues.insert("transport_conflict");
    }
    if local_views.values().any(|views| views.len() != 1) {
        issues.insert("local_membership_conflict");
    }
    if endpoints.values().any(|owners| owners.len() != 1) {
        issues.insert("duplicate_physical_endpoint");
    }
    // Multiple logical partitions require a shared, known PCI identity.
    for (endpoint, owners) in &endpoints {
        if owners.iter().any(|(_, pci)| pci.is_none())
            && members.iter().filter(|m| m.endpoint == *endpoint).count() > 1
        {
            issues.insert("ambiguous_physical_endpoint");
        }
    }
    let active: BTreeSet<&str> = active_views.into_iter().flatten().collect();
    if active != endpoints.keys().copied().collect() {
        issues.insert("unregistered_endpoints");
    }
    for (node, views) in local_views {
        let registered: BTreeSet<_> = members
            .iter()
            .filter(|member| member.device.node == node)
            .map(|member| member.endpoint.as_str())
            .collect();
        if views.iter().any(|view| *view != registered) {
            issues.insert("local_endpoints_unregistered");
        }
    }
    members.sort_by(|a, b| a.device.cmp(&b.device).then(a.endpoint.cmp(&b.endpoint)));
    FabricBlock {
        key,
        members,
        active_endpoints: active.into_iter().map(str::to_string).collect(),
        physical_capacity: if physical_capacities.len() == 1 {
            physical_capacities.into_iter().next().flatten()
        } else {
            None
        },
        fabric_capacity: if fabric_capacities.len() == 1 {
            fabric_capacities.into_iter().next().flatten()
        } else {
            None
        },
        issues: issues.into_iter().map(str::to_string).collect(),
        file_revision: None,
        observed_key: None,
    }
}

/// A candidate membership document; runtime availability always comes from agents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FabricManifest {
    pub version: u32,
    pub namespace: String,
    pub blocks: Vec<FabricManifestBlock>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FabricManifestBlock {
    pub key: FabricBlockKey,
    pub devices: Vec<FabricDeviceKey>,
}

impl FabricManifest {
    pub fn from_view(view: &FabricView) -> Self {
        Self {
            version: 1,
            namespace: view.namespace.clone(),
            blocks: view
                .blocks
                .iter()
                .map(|block| FabricManifestBlock {
                    key: block.key.clone(),
                    devices: block
                        .members
                        .iter()
                        .map(|member| member.device.clone())
                        .collect(),
                })
                .collect(),
        }
    }

    pub fn validate_shape(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err("unsupported fabric manifest version".into());
        }
        if self.namespace.trim().is_empty() || self.blocks.is_empty() {
            return Err("fabric manifest requires a namespace and at least one block".into());
        }
        let mut keys = BTreeSet::new();
        let mut devices = BTreeSet::new();
        for declared in &self.blocks {
            if declared.key.namespace != self.namespace || !keys.insert(&declared.key) {
                return Err("duplicate block or mismatched block namespace".into());
            }
            if declared.key.physical_domain.trim().is_empty()
                || declared.key.fabric_domain.trim().is_empty()
                || declared.devices.is_empty()
            {
                return Err("fabric block requires physical/fabric domains and devices".into());
            }
            for device in &declared.devices {
                if device.node.trim().is_empty() || device.stable_id == 0 || !devices.insert(device)
                {
                    return Err("invalid or duplicate device identity".into());
                }
            }
        }
        Ok(())
    }

    /// Resolve declarations against live inventory; file-provided health,
    /// capacity, and unregistered resources cannot satisfy membership.
    pub fn resolve<'a>(&self, view: &'a FabricView) -> Result<Vec<&'a FabricBlock>, String> {
        self.validate_shape()?;
        if self.namespace != view.namespace {
            return Err("fabric manifest namespace does not match the controller".into());
        }
        let mut resolved = Vec::new();
        for declared in &self.blocks {
            let block = view
                .blocks
                .iter()
                .find(|block| block.key == declared.key)
                .ok_or_else(|| format!("unknown fabric block: {:?}", declared.key))?;
            if !block.issues.is_empty() {
                return Err(format!(
                    "fabric block {:?}: {}",
                    block.key,
                    block.issues.join(",")
                ));
            }
            let expected: BTreeSet<_> = block.members.iter().map(|member| &member.device).collect();
            if declared.devices.iter().collect::<BTreeSet<_>>() != expected {
                return Err(format!(
                    "fabric block {:?}: membership differs from live inventory",
                    block.key
                ));
            }
            resolved.push(block);
        }
        Ok(resolved)
    }
}

impl From<&FabricView> for spur_proto::proto::GetFabricTopologyResponse {
    fn from(view: &FabricView) -> Self {
        use spur_proto::proto;
        let device = |key: &FabricDeviceKey| proto::FabricDeviceKey {
            node: key.node.clone(),
            stable_id: key.stable_id,
        };
        Self {
            single_block_partitions: Vec::new(),
            notices: view
                .notices
                .iter()
                .map(|notice| proto::BlockFileNotice {
                    block_name: notice.block_name.clone(),
                    warning: notice.warning,
                    message: notice.message.clone(),
                })
                .collect(),
            namespace: view.namespace.clone(),
            blocks: view
                .blocks
                .iter()
                .map(|block| proto::FabricBlock {
                    key: Some(proto::FabricBlockKey {
                        namespace: block.key.namespace.clone(),
                        physical_domain: block.key.physical_domain.clone(),
                        fabric_domain: block.key.fabric_domain.clone(),
                    }),
                    members: block
                        .members
                        .iter()
                        .map(|member| proto::FabricMember {
                            device: Some(device(&member.device)),
                            endpoint: member.endpoint.clone(),
                            inventory_generation: member.inventory_generation,
                        })
                        .collect(),
                    active_endpoints: block.active_endpoints.clone(),
                    physical_capacity: block.physical_capacity,
                    fabric_capacity: block.fabric_capacity,
                    issues: block.issues.clone(),
                    file_revision: block.file_revision.clone(),
                    observed_key: block.observed_key.as_ref().map(|k| proto::FabricBlockKey {
                        namespace: k.namespace.clone(),
                        physical_domain: k.physical_domain.clone(),
                        fabric_domain: k.fabric_domain.clone(),
                    }),
                })
                .collect(),
            unresolved: view
                .unresolved
                .iter()
                .map(|item| proto::UnresolvedFabricDevice {
                    device: Some(device(&item.device)),
                    issue: item.issue.clone(),
                })
                .collect(),
        }
    }
}

impl TryFrom<spur_proto::proto::GetFabricTopologyResponse> for FabricView {
    type Error = String;

    fn try_from(view: spur_proto::proto::GetFabricTopologyResponse) -> Result<Self, Self::Error> {
        let device =
            |key: Option<spur_proto::proto::FabricDeviceKey>| -> Result<FabricDeviceKey, String> {
                let key = key.ok_or("missing fabric device identity")?;
                Ok(FabricDeviceKey {
                    node: key.node,
                    stable_id: key.stable_id,
                })
            };
        let blocks = view
            .blocks
            .into_iter()
            .map(|block| {
                let key = block.key.ok_or("missing fabric block identity")?;
                let members = block
                    .members
                    .into_iter()
                    .map(|member| {
                        Ok(FabricMember {
                            device: device(member.device)?,
                            endpoint: member.endpoint,
                            inventory_generation: member.inventory_generation,
                        })
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                Ok(FabricBlock {
                    key: FabricBlockKey {
                        namespace: key.namespace,
                        physical_domain: key.physical_domain,
                        fabric_domain: key.fabric_domain,
                    },
                    members,
                    active_endpoints: block.active_endpoints,
                    physical_capacity: block.physical_capacity,
                    fabric_capacity: block.fabric_capacity,
                    issues: block.issues,
                    file_revision: block.file_revision,
                    observed_key: block.observed_key.map(|k| FabricBlockKey {
                        namespace: k.namespace,
                        physical_domain: k.physical_domain,
                        fabric_domain: k.fabric_domain,
                    }),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let unresolved = view
            .unresolved
            .into_iter()
            .map(|item| {
                Ok(UnresolvedFabricDevice {
                    device: device(item.device)?,
                    issue: item.issue,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(Self {
            namespace: view.namespace,
            blocks,
            unresolved,
            notices: view
                .notices
                .into_iter()
                .map(|notice| BlockFileNotice {
                    block_name: notice.block_name,
                    warning: notice.warning,
                    message: notice.message,
                })
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::NodeState;
    use crate::resource::{DeviceTopology, GpuLinkType, GpuResource, ResourceSet};

    #[test]
    fn legacy_topology_view_defaults_to_no_notices() {
        let view: FabricView =
            serde_json::from_str(r#"{"namespace":"test","blocks":[],"unresolved":[]}"#).unwrap();
        assert!(view.notices.is_empty());
        let wire = spur_proto::proto::GetFabricTopologyResponse {
            namespace: "test".into(),
            ..Default::default()
        };
        assert_eq!(FabricView::try_from(wire).unwrap(), view);
    }

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).unwrap()
    }

    fn node(name: &str, physical: &str, endpoint: &str) -> Node {
        let mut node = Node::new(
            name.into(),
            ResourceSet {
                cpus: 8,
                memory_mb: 1000,
                generation: 7,
                gpus: vec![GpuResource {
                    device_id: 0,
                    stable_id: 42,
                    gpu_type: "test".into(),
                    memory_mb: 100,
                    peer_gpus: vec![],
                    link_type: GpuLinkType::PCIe,
                    topology: Some(DeviceTopology {
                        pci_bdf: Some("0001:04:00.0".into()),
                        fabric: Some(FabricObservation {
                            source: "test".into(),
                            status: ObservationStatus::Observed,
                            physical_domain_id: Some(physical.into()),
                            fabric_domain_id: Some("7".into()),
                            endpoint_id: Some(endpoint.into()),
                            physical_capacity: Some(8),
                            fabric_capacity: Some(2),
                            active_endpoints: vec!["1".into(), "2".into()],
                            local_endpoints: vec![endpoint.into()],
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                }],
                ..Default::default()
            },
        );
        node.state = NodeState::Idle;
        node.last_heartbeat = Some(now());
        node
    }

    fn observation(node: &mut Node) -> &mut FabricObservation {
        node.total_resources.gpus[0]
            .topology
            .as_mut()
            .unwrap()
            .fabric
            .as_mut()
            .unwrap()
    }

    fn nodes() -> Vec<Node> {
        vec![node("n1", "rack-a", "1"), node("n2", "rack-a", "2")]
    }
    fn view(nodes: &[Node]) -> FabricView {
        FabricView::from_nodes("test", nodes, now(), 90)
    }

    #[test]
    fn joins_explicit_members_and_scopes_reused_partition_ids() {
        let mut nodes = nodes();
        nodes.extend([node("n3", "rack-b", "1"), node("n4", "rack-b", "2")]);
        let view = view(&nodes);
        assert_eq!(view.blocks.len(), 2);
        assert!(view
            .blocks
            .iter()
            .all(|b| b.issues.is_empty() && b.members.len() == 2));
        assert_eq!(view.blocks[0].members[1].device.node, "n2");
        assert_eq!(view.blocks[0].members[1].device.stable_id, 42);
    }

    #[test]
    fn deterministic_and_wire_round_trip() {
        let mut nodes = nodes();
        let expected = view(&nodes);
        nodes.reverse();
        observation(&mut nodes[0]).active_endpoints.reverse();
        assert_eq!(view(&nodes), expected);
        let wire: spur_proto::proto::GetFabricTopologyResponse = (&expected).into();
        assert_eq!(FabricView::try_from(wire).unwrap(), expected);
    }

    #[test]
    fn missing_nodes_never_inflate_capacity() {
        let view = view(&nodes()[..1]);
        assert_eq!(view.blocks[0].members.len(), 1);
        assert_eq!(view.blocks[0].physical_capacity, Some(8));
        assert!(view.blocks[0]
            .issues
            .contains(&"unregistered_endpoints".into()));
    }

    #[test]
    fn multiple_logical_partitions_share_one_physical_endpoint() {
        let mut nodes = nodes();
        let mut gpu = nodes[0].total_resources.gpus[0].clone();
        gpu.stable_id = 43;
        nodes[0].total_resources.gpus.push(gpu);
        let result = view(&nodes);
        assert!(
            result.blocks[0].issues.is_empty(),
            "{:?}",
            result.blocks[0].issues
        );
        assert_eq!(result.blocks[0].members.len(), 3);
        assert_eq!(result.blocks[0].active_endpoints.len(), 2);
        nodes[0].total_resources.gpus[1]
            .topology
            .as_mut()
            .unwrap()
            .pci_bdf = Some("0002:04:00.0".into());
        assert!(view(&nodes).blocks[0]
            .issues
            .contains(&"duplicate_physical_endpoint".into()));
    }

    #[test]
    fn detects_cross_node_membership_and_capacity_disagreement() {
        let mut nodes = nodes();
        observation(&mut nodes[1]).active_endpoints.push("3".into());
        observation(&mut nodes[1]).fabric_capacity = Some(3);
        let result = view(&nodes);
        assert!(result.blocks[0]
            .issues
            .contains(&"active_membership_conflict".into()));
        assert!(result.blocks[0]
            .issues
            .contains(&"fabric_capacity_conflict".into()));
    }

    #[test]
    fn rejects_stale_down_missing_and_legacy_inventory() {
        let mut nodes = nodes();
        nodes[0].last_heartbeat = Some(now() - chrono::Duration::seconds(90));
        nodes[1].total_resources.generation = 0;
        nodes[1].state = NodeState::Down;
        let result = view(&nodes);
        for code in [
            "stale_node",
            "node_unavailable",
            "missing_inventory_generation",
        ] {
            assert!(result.blocks[0].issues.contains(&code.into()));
        }
        observation(&mut nodes[0]).fabric_domain_id = None;
        let result = view(&nodes);
        assert_eq!(result.unresolved[0].issue, "missing_domain");
        assert!(result.blocks[0]
            .issues
            .contains(&"unregistered_endpoints".into()));
    }

    #[test]
    fn endpoint_cannot_be_claimed_by_two_partitions() {
        let mut nodes = nodes();
        observation(&mut nodes[1]).fabric_domain_id = Some("8".into());
        let result = view(&nodes);
        assert_eq!(result.blocks.len(), 2);
        assert!(result
            .blocks
            .iter()
            .all(|b| b.issues.contains(&"endpoint_in_multiple_blocks".into())));
    }

    #[test]
    fn duplicate_logical_id_across_blocks_is_invalid() {
        let mut nodes = nodes();
        let mut gpu = nodes[0].total_resources.gpus[0].clone();
        gpu.topology
            .as_mut()
            .unwrap()
            .fabric
            .as_mut()
            .unwrap()
            .physical_domain_id = Some("rack-b".into());
        nodes[0].total_resources.gpus.push(gpu);
        assert!(view(&nodes)
            .blocks
            .iter()
            .all(|b| b.issues.contains(&"invalid_device_identity".into())));
    }

    #[test]
    fn manifest_resolves_only_current_complete_membership() {
        let mut nodes = nodes();
        let before = view(&nodes);
        let manifest = FabricManifest::from_view(&before);
        assert_eq!(manifest.resolve(&before).unwrap().len(), 1);
        let encoded = serde_json::to_string(&manifest).unwrap();
        assert_eq!(
            serde_json::from_str::<FabricManifest>(&encoded).unwrap(),
            manifest
        );
        nodes[1].total_resources.gpus[0].stable_id = 43;
        assert!(manifest
            .resolve(&view(&nodes))
            .unwrap_err()
            .contains("membership differs"));
        nodes[1].last_heartbeat = None;
        assert!(manifest
            .resolve(&view(&nodes))
            .unwrap_err()
            .contains("stale_node"));
    }

    #[test]
    fn manifest_rejects_namespace_version_duplicates_and_invented_devices() {
        let view = view(&nodes());
        let manifest = FabricManifest::from_view(&view);
        let mut invalid = manifest.clone();
        invalid.version = 2;
        assert!(invalid.resolve(&view).is_err());
        let mut invalid = manifest.clone();
        invalid.namespace = "other".into();
        assert!(invalid.resolve(&view).is_err());
        let mut invalid = manifest.clone();
        invalid.blocks.push(invalid.blocks[0].clone());
        assert!(invalid.resolve(&view).is_err());
        let mut invalid = manifest.clone();
        let duplicate = invalid.blocks[0].devices[0].clone();
        invalid.blocks[0].devices.push(duplicate);
        assert!(invalid.resolve(&view).is_err());
        let mut invalid = manifest;
        invalid.blocks[0].devices[0].node = "invented".into();
        assert!(invalid.resolve(&view).is_err());
    }
}
