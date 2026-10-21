// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use crate::fabric::{
    FabricBlock, FabricBlockKey, FabricDeviceKey, FabricMember, FabricView, UnresolvedFabricDevice,
};
use crate::node::Node;

pub const MAX_BLOCK_FILE_BYTES: usize = 1024 * 1024;
const MAX_NODES: usize = 65_536;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefinedBlock {
    pub name: String,
    pub nodes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockFile {
    pub revision: String,
    pub blocks: Vec<DefinedBlock>,
}

impl BlockFile {
    pub fn read(path: &Path) -> Result<Self, String> {
        Self::read_with_text(path).map(|(parsed, _)| parsed)
    }

    pub fn read_with_text(path: &Path) -> Result<(Self, String), String> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NONBLOCK)
            .open(path)
            .map_err(|e| format!("cannot open block file: {e}"))?;
        if !file.metadata().map_err(|e| e.to_string())?.is_file() {
            return Err("block file must be a regular file".into());
        }
        let mut text = String::new();
        file.take(MAX_BLOCK_FILE_BYTES as u64 + 1)
            .read_to_string(&mut text)
            .map_err(|e| format!("cannot read block file: {e}"))?;
        let parsed = Self::parse(&text)?;
        Ok((parsed, text))
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        if text.len() > MAX_BLOCK_FILE_BYTES {
            return Err("block file exceeds 1 MiB".into());
        }
        let mut blocks = BTreeMap::new();
        let mut owners = BTreeSet::new();
        let mut continued = String::new();
        for (number, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if let Some(prefix) = line.strip_suffix('\\') {
                continued.push_str(prefix);
                continued.push(' ');
                continue;
            }
            continued.push_str(line);
            let line = std::mem::take(&mut continued);
            if line.trim().is_empty() {
                continue;
            }
            let fail = |why: &str| format!("block file line {}: {why}", number + 1);
            let mut name = None;
            let mut nodes = None;
            for token in line.split_whitespace() {
                let (key, value) = token
                    .split_once('=')
                    .ok_or_else(|| fail("expected key=value"))?;
                if value.is_empty() {
                    return Err(fail("empty field value"));
                }
                let target = match key.to_ascii_lowercase().as_str() {
                    "blockname" => &mut name,
                    "nodes" => &mut nodes,
                    "blocksizes" => return Err(fail(
                        "BlockSizes is unsupported; each BlockName is a hard placement boundary",
                    )),
                    _ => return Err(fail("unsupported field; expected BlockName and Nodes")),
                };
                if target.replace(value).is_some() {
                    return Err(fail("duplicate field"));
                }
            }
            let name = name.ok_or_else(|| fail("BlockName is required"))?;
            if name.len() > 128
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
            {
                return Err(fail("invalid block name"));
            }
            let expression = nodes.ok_or_else(|| fail("Nodes is required"))?;
            let mut nodes = crate::hostlist::expand_bounded(
                expression,
                MAX_NODES.saturating_sub(owners.len()),
                255,
            )
            .map_err(|e| fail(&e.to_string()))?;
            if nodes.is_empty() || nodes.len() > MAX_NODES.saturating_sub(owners.len()) {
                return Err(fail("empty or oversized node membership"));
            }
            for node in &nodes {
                if node.is_empty()
                    || node.len() > 255
                    || !node
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
                {
                    return Err(fail("invalid node name"));
                }
                if !owners.insert(node.clone()) {
                    return Err(fail("node occurs more than once or in multiple blocks"));
                }
            }
            nodes.sort();
            if blocks.insert(name.to_string(), nodes).is_some() {
                return Err(fail("duplicate block name"));
            }
        }
        if !continued.is_empty() {
            return Err("unfinished block file continuation".into());
        }
        if blocks.is_empty() {
            return Err("block file contains no blocks".into());
        }
        let mut digest = Sha256::new();
        digest.update(b"spur-block-file-v1\0");
        let blocks = blocks
            .into_iter()
            .map(|(name, nodes)| {
                digest.update(name.as_bytes());
                digest.update([0]);
                for node in &nodes {
                    digest.update(node.as_bytes());
                    digest.update([0]);
                }
                digest.update([0]);
                DefinedBlock { name, nodes }
            })
            .collect();
        Ok(Self {
            revision: digest
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            blocks,
        })
    }

    pub fn contains(&self, block: &str, node: &str) -> bool {
        self.blocks
            .iter()
            .any(|b| b.name == block && b.nodes.iter().any(|n| n == node))
    }

    pub fn to_text(&self) -> String {
        self.blocks
            .iter()
            .map(|b| {
                format!(
                    "BlockName={} Nodes={}\n",
                    b.name,
                    crate::hostlist::compress(&b.nodes)
                )
            })
            .collect()
    }

    pub fn view(
        &self,
        namespace: &str,
        nodes: &[Node],
        now: DateTime<Utc>,
        heartbeat_timeout_secs: u64,
        check_revision: bool,
    ) -> FabricView {
        let native = FabricView::from_nodes(namespace, nodes, now, heartbeat_timeout_secs);
        let native_devices: BTreeMap<_, _> = native
            .blocks
            .iter()
            .flat_map(|block| {
                block
                    .members
                    .iter()
                    .map(move |member| (&member.device, (block, member)))
            })
            .collect();
        let native_blocks: BTreeMap<_, _> = native.blocks.iter().map(|b| (&b.key, b)).collect();
        let by_name: BTreeMap<_, _> = nodes.iter().map(|n| (n.name.as_str(), n)).collect();
        let mut covered = BTreeSet::new();
        let mut blocks = Vec::new();
        for definition in &self.blocks {
            let mut issues = BTreeSet::new();
            let mut selected = Vec::new();
            for name in &definition.nodes {
                covered.insert(name.as_str());
                let Some(node) = by_name.get(name.as_str()) else {
                    issues.insert(format!("node_unregistered:{name}"));
                    continue;
                };
                if !node.state.is_up() {
                    issues.insert(format!("node_unavailable:{name}"));
                }
                if node.last_heartbeat.is_none_or(|t| {
                    (now - t).num_seconds() < 0
                        || (now - t).num_seconds() as u64 > heartbeat_timeout_secs
                }) {
                    issues.insert(format!("stale_node:{name}"));
                }
                if check_revision
                    && (node.total_resources.fabric_binding_version
                        < crate::fabric_binding::FILE_BINDING_VERSION
                        || node.total_resources.block_file_revision.as_deref()
                            != Some(&self.revision))
                {
                    issues.insert(format!("block_file_revision_mismatch:{name}"));
                }
                if node.total_resources.generation == 0 {
                    issues.insert(format!("missing_inventory_generation:{name}"));
                }
                if node.total_resources.gpus.is_empty() {
                    issues.insert(format!("node_has_no_gpus:{name}"));
                }
                for gpu in &node.total_resources.gpus {
                    selected.push((*node, gpu));
                }
            }
            let has_observations = selected.iter().any(|(_, gpu)| {
                gpu.topology
                    .as_ref()
                    .and_then(|t| t.fabric.as_ref())
                    .is_some()
            });
            let mut native_keys = BTreeSet::new();
            let mut members = Vec::new();
            let mut ids = BTreeSet::new();
            for (node, gpu) in &selected {
                let device = FabricDeviceKey {
                    node: node.name.clone(),
                    stable_id: gpu.stable_id,
                };
                if gpu.stable_id == 0 || !ids.insert(device.clone()) {
                    issues.insert("invalid_device_identity".into());
                }
                let mut endpoint = declared_endpoint(&node.name, gpu.stable_id);
                if has_observations {
                    match native_devices.get(&device) {
                        Some((block, member)) if block.issues.is_empty() => {
                            native_keys.insert(block.key.clone());
                            endpoint = member.endpoint.clone();
                        }
                        _ => {
                            issues.insert("native_membership_unavailable".into());
                        }
                    }
                }
                members.push(FabricMember {
                    device,
                    endpoint,
                    inventory_generation: node.total_resources.generation,
                });
            }
            if native_keys.len() > 1 {
                issues.insert("declared_block_crosses_native_blocks".into());
            }
            let observed_key = if native_keys.len() == 1 {
                native_keys.into_iter().next()
            } else {
                None
            };
            let native_block = observed_key
                .as_ref()
                .and_then(|key| native_blocks.get(key).copied());
            let active_endpoints = native_block
                .map(|b| b.active_endpoints.clone())
                .unwrap_or_else(|| members.iter().map(|m| m.endpoint.clone()).collect());
            members.sort_by(|a, b| a.device.cmp(&b.device));
            blocks.push(FabricBlock {
                key: FabricBlockKey {
                    namespace: namespace.into(),
                    physical_domain: native_block
                        .map(|b| b.key.physical_domain.clone())
                        .unwrap_or_else(|| "admin-defined".into()),
                    fabric_domain: definition.name.clone(),
                },
                members,
                active_endpoints,
                physical_capacity: native_block.and_then(|b| b.physical_capacity),
                fabric_capacity: native_block.and_then(|b| b.fabric_capacity),
                issues: issues.into_iter().collect(),
                file_revision: Some(self.revision.clone()),
                observed_key,
            });
        }
        blocks.sort_by(|a, b| a.key.cmp(&b.key));
        let mut unresolved: Vec<_> = nodes
            .iter()
            .filter(|n| !covered.contains(n.name.as_str()))
            .flat_map(|node| {
                node.total_resources
                    .gpus
                    .iter()
                    .map(|gpu| UnresolvedFabricDevice {
                        device: FabricDeviceKey {
                            node: node.name.clone(),
                            stable_id: gpu.stable_id,
                        },
                        issue: "node_not_in_block_file".into(),
                    })
            })
            .collect();
        unresolved.sort_by(|a, b| a.device.cmp(&b.device));
        FabricView {
            namespace: namespace.into(),
            single_block_partitions: Vec::new(),
            blocks,
            unresolved,
        }
    }
}

pub fn declared_endpoint(node: &str, stable_id: u64) -> String {
    format!("{node}/{stable_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::{
        AllocatedDevice, DeviceTopology, GpuLinkType, GpuResource, ResourceAllocations, ResourceSet,
    };

    fn fixture(file: &BlockFile, native: bool) -> (DateTime<Utc>, Vec<Node>) {
        let now = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let nodes = (0..4)
            .map(|i| {
                let mut node = Node::new(
                    format!("n{i}"),
                    ResourceSet {
                        cpus: 4,
                        memory_mb: 1024,
                        generation: 7,
                        fabric_binding_version: 2,
                        block_file_revision: Some(file.revision.clone()),
                        gpus: (0..2)
                            .map(|g| GpuResource {
                                device_id: g,
                                stable_id: 42 + g as u64,
                                gpu_type: "test".into(),
                                memory_mb: 100,
                                peer_gpus: vec![],
                                link_type: GpuLinkType::PCIe,
                                topology: native.then(|| DeviceTopology {
                                    pci_bdf: Some(format!("000{}:04:00.0", g + 1)),
                                    fabric: Some(crate::fabric::FabricObservation {
                                        source: "test".into(),
                                        status: crate::fabric::ObservationStatus::Observed,
                                        physical_domain_id: Some("rack".into()),
                                        fabric_domain_id: Some((i / 2).to_string()),
                                        endpoint_id: Some((i * 2 + g).to_string()),
                                        physical_capacity: Some(8),
                                        fabric_capacity: Some(4),
                                        active_endpoints: (i / 2 * 4..i / 2 * 4 + 4)
                                            .map(|e| e.to_string())
                                            .collect(),
                                        local_endpoints: (i * 2..i * 2 + 2)
                                            .map(|e| e.to_string())
                                            .collect(),
                                        ..Default::default()
                                    }),
                                    ..Default::default()
                                }),
                            })
                            .collect(),
                        ..Default::default()
                    },
                );
                node.state = crate::node::NodeState::Idle;
                node.last_heartbeat = Some(now);
                node
            })
            .collect();
        (now, nodes)
    }

    #[test]
    fn declared_blocks_bind_live_devices_with_and_without_native_observations() {
        let file = BlockFile::parse("BlockName=a Nodes=n[0-1]").unwrap();
        for native in [false, true] {
            let (now, nodes) = fixture(&file, native);
            let view = file.view("test", &nodes, now, 90, true);
            assert_eq!(view.blocks[0].members.len(), 4);
            assert_eq!(view.unresolved.len(), 4);
            assert!(
                view.blocks[0].issues.is_empty(),
                "{:?}",
                view.blocks[0].issues
            );
            assert_eq!(view.blocks[0].observed_key.is_some(), native);
            let allocation = ResourceAllocations {
                generation: 7,
                devices: [("gpu".into(), vec![AllocatedDevice::injectable(42)])].into(),
                ..Default::default()
            };
            let binding = crate::fabric_binding::FabricBinding::for_allocation(
                &view.blocks[0],
                "n0",
                &allocation,
            )
            .unwrap();
            binding
                .validate("test", "n0", &nodes[0].total_resources, &allocation)
                .unwrap();
            assert_eq!(binding.version, 2);
            assert_eq!(
                crate::fabric_binding::from_proto(crate::fabric_binding::to_proto(&binding)),
                binding
            );
            let roundtrip =
                FabricView::try_from(spur_proto::proto::GetFabricTopologyResponse::from(&view))
                    .unwrap();
            assert_eq!(view, roundtrip);
            let mut changed = nodes[0].total_resources.clone();
            changed.block_file_revision = None;
            assert!(binding
                .validate("test", "n0", &changed, &allocation)
                .is_err());
        }
    }

    #[test]
    fn native_boundaries_cannot_be_overridden_by_a_file() {
        let file = BlockFile::parse("BlockName=a Nodes=n0,n2").unwrap();
        let (now, nodes) = fixture(&file, true);
        let view = file.view("test", &nodes, now, 90, true);
        assert!(view.blocks[0]
            .issues
            .iter()
            .any(|i| i == "declared_block_crosses_native_blocks"));
        let mut missing = nodes;
        missing[2].total_resources.gpus[0].topology = None;
        assert!(file.view("test", &missing, now, 90, true).blocks[0]
            .issues
            .iter()
            .any(|i| i == "native_membership_unavailable"));
    }

    #[test]
    fn missing_nodes_stale_agents_and_different_revisions_are_unavailable() {
        let file = BlockFile::parse("BlockName=a Nodes=n[0-1],missing").unwrap();
        let (now, mut nodes) = fixture(&file, false);
        nodes[0].total_resources.block_file_revision = None;
        nodes[1].last_heartbeat = None;
        let view = file.view("test", &nodes, now, 90, true);
        for issue in [
            "node_unregistered:missing",
            "block_file_revision_mismatch:n0",
            "stale_node:n1",
        ] {
            assert!(view.blocks[0].issues.iter().any(|i| i == issue), "{issue}");
        }
    }

    #[test]
    fn file_reads_are_bounded_and_require_a_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(BlockFile::read(dir.path()).is_err());
        let path = dir.path().join("topology.conf");
        std::fs::write(&path, "BlockName=a Nodes=n[0-1]").unwrap();
        let file = BlockFile::read(&path).unwrap();
        assert_eq!(file, BlockFile::parse(&file.to_text()).unwrap());
        std::fs::write(&path, "#".repeat(MAX_BLOCK_FILE_BYTES + 1)).unwrap();
        assert!(BlockFile::read(&path).is_err());
    }

    #[test]
    fn records_are_order_independent_and_revision_is_semantic() {
        let a = BlockFile::parse("# comment\nBlockName=a Nodes=n[01-02]\nBlockName=b Nodes=n03\n")
            .unwrap();
        let b = BlockFile::parse(
            "nodes=n03 blockname=b\nNodes=n02,n01 \\\n          BlockName=a # comment\n",
        )
        .unwrap();
        assert_eq!(a, b);
        assert_ne!(
            a.revision,
            BlockFile::parse("BlockName=a Nodes=n[01-03]")
                .unwrap()
                .revision
        );
    }

    #[test]
    fn rejects_ambiguous_or_unsupported_definitions() {
        for text in [
            "",
            "BlockName=a",
            "Nodes=n1",
            "BlockName=a BlockName=b Nodes=n1",
            "BlockName=a Nodes=n1,n1",
            "BlockName=a Nodes=n1\nBlockName=b Nodes=n1",
            "BlockName=a Nodes=n1\nBlockName=a Nodes=n2",
            "BlockName=a Nodes=n[2-1]",
            "BlockSizes=2,4",
            "SwitchName=a Nodes=n1",
            "BlockName=a Nodes=n1 Extra=x",
            "BlockName=a Nodes=n1 \\",
        ] {
            assert!(BlockFile::parse(text).is_err(), "{text}");
        }
        assert!(BlockFile::parse(&"#".repeat(MAX_BLOCK_FILE_BYTES + 1)).is_err());
    }
}
