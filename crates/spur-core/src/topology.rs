// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

use crate::hostlist;

/// A switch in the fabric hierarchy.
///
/// Leaf switches have `nodes` populated (directly attached compute nodes).
/// Non-leaf (aggregation) switches have `children` populated (child switch names).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Switch {
    pub name: String,
    /// Compute nodes directly attached to this switch (leaf switches only).
    pub nodes: Vec<String>,
    /// Child switch names (aggregation switches only).
    pub children: Vec<String>,
    /// Parent switch name (None for root switches).
    pub parent: Option<String>,
    /// Depth from root (root = 0, leaf = max depth).
    pub depth: u32,
}

/// The node topology, built from configuration or observed fabric membership.
///
/// Supports two modes:
/// - **tree**: Hierarchical switch topology (fat-tree, dragonfly).
///   Nodes are grouped under leaf switches, which connect to aggregation switches.
/// - **block**: Flat grouping by explicit membership or configured block size.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopologyTree {
    /// All switches by name.
    pub switches: HashMap<String, Switch>,
    /// node_name → leaf switch name (fast lookup).
    pub node_switch: HashMap<String, String>,
}

impl TopologyTree {
    /// Project complete whole-node fabric membership into the existing leaf model.
    pub fn from_fabric(view: &crate::fabric::FabricView) -> Self {
        let mut owners: BTreeMap<&str, BTreeSet<usize>> = BTreeMap::new();
        let mut excluded: BTreeSet<&str> = view
            .unresolved
            .iter()
            .map(|item| item.device.node.as_str())
            .collect();
        for (index, block) in view.blocks.iter().enumerate() {
            for member in &block.members {
                let node = member.device.node.as_str();
                owners.entry(node).or_default().insert(index);
                if !block.issues.is_empty() {
                    excluded.insert(node);
                }
            }
        }
        let mut groups: BTreeMap<usize, Vec<String>> = BTreeMap::new();
        for (node, blocks) in owners {
            // A node with split or incomplete GPU membership has no single leaf.
            if excluded.contains(node) || blocks.len() != 1 {
                continue;
            }
            if let Some(index) = blocks.into_iter().next() {
                groups.entry(index).or_default().push(node.to_string());
            }
        }
        let mut switches = HashMap::new();
        let mut node_switch = HashMap::new();
        for (index, nodes) in groups {
            let key = &view.blocks[index].key;
            let name = format!(
                "fabric:{}:{}{}:{}{}:{}",
                key.namespace.len(),
                key.namespace,
                key.physical_domain.len(),
                key.physical_domain,
                key.fabric_domain.len(),
                key.fabric_domain,
            );
            for node in &nodes {
                node_switch.insert(node.clone(), name.clone());
            }
            switches.insert(
                name.clone(),
                Switch {
                    name,
                    nodes,
                    children: Vec::new(),
                    parent: None,
                    depth: 0,
                },
            );
        }
        Self {
            switches,
            node_switch,
        }
    }

    /// Build a topology tree from switch config entries.
    ///
    /// Each entry defines a switch with either `nodes` (leaf) or `switches` (aggregation).
    /// Parent links and depths are computed automatically.
    pub fn from_switches(switch_configs: &[SwitchConfig]) -> Self {
        let mut switches = HashMap::new();
        let mut node_switch = HashMap::new();

        // First pass: create all switches
        for sc in switch_configs {
            let nodes = if let Some(ref node_pattern) = sc.nodes {
                hostlist::expand(node_pattern).unwrap_or_default()
            } else {
                Vec::new()
            };

            let children = if let Some(ref sw_pattern) = sc.switches {
                sw_pattern
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .collect()
            } else {
                Vec::new()
            };

            // Map nodes to this switch
            for node in &nodes {
                node_switch.insert(node.clone(), sc.name.clone());
            }

            switches.insert(
                sc.name.clone(),
                Switch {
                    name: sc.name.clone(),
                    nodes,
                    children,
                    parent: None,
                    depth: 0,
                },
            );
        }

        // Second pass: set parent links
        let child_to_parent: HashMap<String, String> = switches
            .values()
            .flat_map(|sw| {
                sw.children
                    .iter()
                    .map(move |child| (child.clone(), sw.name.clone()))
            })
            .collect();

        for (child_name, parent_name) in &child_to_parent {
            if let Some(child) = switches.get_mut(child_name) {
                child.parent = Some(parent_name.clone());
            }
        }

        // Third pass: compute depths (BFS from roots)
        let roots: Vec<String> = switches
            .values()
            .filter(|sw| sw.parent.is_none())
            .map(|sw| sw.name.clone())
            .collect();

        let mut queue: std::collections::VecDeque<(String, u32)> =
            roots.into_iter().map(|name| (name, 0)).collect();

        while let Some((name, depth)) = queue.pop_front() {
            let children = if let Some(sw) = switches.get_mut(&name) {
                sw.depth = depth;
                sw.children.clone()
            } else {
                continue;
            };
            for child in children {
                queue.push_back((child, depth + 1));
            }
        }

        Self {
            switches,
            node_switch,
        }
    }

    /// Build a topology from block config (flat rack grouping).
    ///
    /// Nodes are grouped into blocks of `block_size`, creating one switch per block.
    pub fn from_blocks(all_nodes: &[String], block_size: usize) -> Self {
        let mut switches = HashMap::new();
        let mut node_switch = HashMap::new();
        let block_size = block_size.max(1);

        for (i, chunk) in all_nodes.chunks(block_size).enumerate() {
            let name = format!("block{:03}", i);
            let nodes: Vec<String> = chunk.to_vec();
            for node in &nodes {
                node_switch.insert(node.clone(), name.clone());
            }
            switches.insert(
                name.clone(),
                Switch {
                    name,
                    nodes,
                    children: Vec::new(),
                    parent: None,
                    depth: 0,
                },
            );
        }

        Self {
            switches,
            node_switch,
        }
    }

    /// Compute the hop distance between two nodes in the switch tree.
    ///
    /// 0 = same switch, 1 = sibling switches (same parent), 2 = grandparent, etc.
    /// Returns u32::MAX if either node is unknown.
    pub fn distance(&self, a: &str, b: &str) -> u32 {
        let sw_a = match self.node_switch.get(a) {
            Some(s) => s,
            None => return u32::MAX,
        };
        let sw_b = match self.node_switch.get(b) {
            Some(s) => s,
            None => return u32::MAX,
        };

        if sw_a == sw_b {
            return 0;
        }

        // Walk up from both switches to find the lowest common ancestor
        let ancestors_a = self.ancestors(sw_a);
        let ancestors_b = self.ancestors(sw_b);

        // Find LCA
        for (depth_a, anc_a) in &ancestors_a {
            for (depth_b, anc_b) in &ancestors_b {
                if anc_a == anc_b {
                    return depth_a + depth_b;
                }
            }
        }

        // No common ancestor — different trees
        u32::MAX
    }

    /// Get the ancestor chain of a switch: [(hops_from_switch, ancestor_name), ...]
    fn ancestors(&self, switch_name: &str) -> Vec<(u32, String)> {
        let mut result = vec![(0, switch_name.to_string())];
        let mut current = switch_name.to_string();
        let mut hops = 1;

        while let Some(sw) = self.switches.get(&current) {
            if let Some(ref parent) = sw.parent {
                result.push((hops, parent.clone()));
                current = parent.clone();
                hops += 1;
            } else {
                break;
            }
        }

        result
    }

    /// Group node names by their leaf switch.
    pub fn group_by_switch<'a>(&self, nodes: &[&'a str]) -> HashMap<String, Vec<&'a str>> {
        let mut groups: HashMap<String, Vec<&'a str>> = HashMap::new();
        for &node in nodes {
            let switch = self
                .node_switch
                .get(node)
                .cloned()
                .unwrap_or_else(|| "unknown".to_string());
            groups.entry(switch).or_default().push(node);
        }
        groups
    }

    /// Select `count` nodes from `candidates` that are topologically closest.
    ///
    /// Strategy:
    /// 1. Group candidates by leaf switch
    /// 2. Sort groups by size (largest first)
    /// 3. If the largest group has enough nodes, use it exclusively (block mode)
    /// 4. Otherwise, greedily add closest neighboring groups until we have enough
    ///
    /// Returns node names in locality order (same-switch nodes adjacent).
    pub fn select_local_nodes<'a>(&self, candidates: &[&'a str], count: usize) -> Vec<&'a str> {
        if candidates.len() <= count {
            return candidates.to_vec();
        }

        let (known, unknown): (Vec<_>, Vec<_>) = candidates
            .iter()
            .copied()
            .partition(|node| self.node_switch.contains_key(*node));
        if known.is_empty() {
            return candidates.iter().copied().take(count).collect();
        }
        let groups = self.group_by_switch(&known);
        let mut sorted_groups: Vec<(String, Vec<&'a str>)> = groups.into_iter().collect();
        sorted_groups.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));

        // If the largest group has enough, use it
        if sorted_groups[0].1.len() >= count {
            return sorted_groups[0].1[..count].to_vec();
        }

        // Greedy: take from largest group first, then closest neighbors
        let seed_switch = sorted_groups[0].0.clone();
        let mut result: Vec<&'a str> = Vec::new();

        // Sort remaining groups by distance to seed switch
        let mut groups_with_dist: Vec<(u32, String, Vec<&'a str>)> = sorted_groups
            .into_iter()
            .map(|(sw_name, nodes)| {
                let dist = self.switch_distance(&seed_switch, &sw_name);
                (dist, sw_name, nodes)
            })
            .collect();
        groups_with_dist.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

        for (_, _, nodes) in groups_with_dist {
            let remaining = count - result.len();
            if remaining == 0 {
                break;
            }
            let take = remaining.min(nodes.len());
            result.extend_from_slice(&nodes[..take]);
        }
        result.extend(unknown.into_iter().take(count - result.len()));

        result
    }

    /// Distance between two switches (not nodes).
    fn switch_distance(&self, a: &str, b: &str) -> u32 {
        if a == b {
            return 0;
        }

        let ancestors_a = self.ancestors(a);
        let ancestors_b = self.ancestors(b);

        for (depth_a, anc_a) in &ancestors_a {
            for (depth_b, anc_b) in &ancestors_b {
                if anc_a == anc_b {
                    return depth_a + depth_b;
                }
            }
        }

        u32::MAX
    }
}

/// Configuration for a single switch (from TOML config).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwitchConfig {
    pub name: String,
    /// Hostlist pattern of compute nodes (leaf switch).
    pub nodes: Option<String>,
    /// Comma-separated child switch names (aggregation switch).
    pub switches: Option<String>,
}

/// Top-level topology configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopologyConfig {
    #[serde(default)]
    pub external: Option<ExternalFabricConfig>,
    /// Plugin type: "tree", "block", or "none".
    #[serde(default = "default_topo_plugin")]
    pub plugin: String,
    /// Switch definitions (for tree mode).
    #[serde(default)]
    pub switches: Vec<SwitchConfig>,
    /// Block size (for block mode): how many nodes per block/rack.
    pub block_size: Option<usize>,
}

impl TopologyConfig {
    pub fn build(
        &self,
        nodes: &[crate::node::Node],
        view: &crate::fabric::FabricView,
    ) -> Option<TopologyTree> {
        if self.plugin == "tree" && !self.switches.is_empty() {
            return Some(TopologyTree::from_switches(&self.switches));
        }
        if !matches!(self.plugin.as_str(), "tree" | "block") {
            return None;
        }
        let has_membership_source = !view.blocks.is_empty()
            || view
                .unresolved
                .iter()
                .any(|item| item.issue != "missing_observation")
            || self
                .external
                .as_ref()
                .is_some_and(|external| external.block_file.is_some());
        if has_membership_source {
            return Some(TopologyTree::from_fabric(view));
        }
        if self.plugin == "tree" {
            return Some(TopologyTree::from_switches(&self.switches));
        }
        let mut names: Vec<_> = nodes.iter().map(|node| node.name.clone()).collect();
        names.sort();
        Some(TopologyTree::from_blocks(
            &names,
            self.block_size.unwrap_or(18),
        ))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalFabricConfig {
    #[serde(default)]
    pub block_file: Option<std::path::PathBuf>,
    #[serde(skip)]
    pub loaded_block_file: Option<crate::block_file::BlockFile>,
}

impl ExternalFabricConfig {
    pub fn load_block_file(&mut self) -> Result<(), String> {
        self.loaded_block_file = self
            .block_file
            .as_deref()
            .map(crate::block_file::BlockFile::read)
            .transpose()?;
        Ok(())
    }
}

fn default_topo_plugin() -> String {
    "none".into()
}

impl Default for TopologyConfig {
    fn default() -> Self {
        Self {
            external: None,
            plugin: "none".into(),
            switches: Vec::new(),
            block_size: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tree_config() -> Vec<SwitchConfig> {
        vec![
            SwitchConfig {
                name: "rack01".into(),
                nodes: Some("node[001-004]".into()),
                switches: None,
            },
            SwitchConfig {
                name: "rack02".into(),
                nodes: Some("node[005-008]".into()),
                switches: None,
            },
            SwitchConfig {
                name: "fabric0".into(),
                nodes: None,
                switches: Some("rack01,rack02".into()),
            },
        ]
    }

    #[test]
    fn test_tree_build() {
        let tree = TopologyTree::from_switches(&make_tree_config());
        assert_eq!(tree.switches.len(), 3);
        assert_eq!(tree.node_switch.len(), 8);
        assert_eq!(tree.node_switch.get("node001").unwrap(), "rack01");
        assert_eq!(tree.node_switch.get("node005").unwrap(), "rack02");
    }

    #[test]
    fn test_distance_same_switch() {
        let tree = TopologyTree::from_switches(&make_tree_config());
        assert_eq!(tree.distance("node001", "node002"), 0);
    }

    #[test]
    fn test_distance_sibling_switches() {
        let tree = TopologyTree::from_switches(&make_tree_config());
        // node001 (rack01) → fabric0 → rack02 → node005 = 2 hops
        assert_eq!(tree.distance("node001", "node005"), 2);
    }

    #[test]
    fn test_distance_unknown_node() {
        let tree = TopologyTree::from_switches(&make_tree_config());
        assert_eq!(tree.distance("node001", "unknown"), u32::MAX);
    }

    #[test]
    fn test_block_topology() {
        let nodes: Vec<String> = (1..=10).map(|i| format!("node{:03}", i)).collect();
        let tree = TopologyTree::from_blocks(&nodes, 4);
        assert_eq!(tree.switches.len(), 3); // 4+4+2
        assert_eq!(tree.node_switch.get("node001").unwrap(), "block000");
        assert_eq!(tree.node_switch.get("node005").unwrap(), "block001");
        assert_eq!(tree.node_switch.get("node009").unwrap(), "block002");
        assert_eq!(tree.distance("node001", "node004"), 0); // same block
        assert_eq!(tree.distance("node001", "node005"), u32::MAX); // different blocks, no parent
    }

    #[test]
    fn test_select_local_from_single_switch() {
        let tree = TopologyTree::from_switches(&make_tree_config());
        let candidates: Vec<&str> = vec![
            "node001", "node002", "node003", "node004", "node005", "node006", "node007", "node008",
        ];
        let selected = tree.select_local_nodes(&candidates, 3);
        assert_eq!(selected.len(), 3);
        // All 3 should be from the same rack
        let switch = tree.node_switch.get(selected[0]).unwrap();
        for node in &selected {
            assert_eq!(tree.node_switch.get(*node).unwrap(), switch);
        }
    }

    #[test]
    fn test_select_local_spans_switches() {
        let tree = TopologyTree::from_switches(&make_tree_config());
        let candidates: Vec<&str> = vec![
            "node001", "node002", "node003", "node004", "node005", "node006", "node007", "node008",
        ];
        // Need 6 nodes — must span both racks
        let selected = tree.select_local_nodes(&candidates, 6);
        assert_eq!(selected.len(), 6);
    }

    #[test]
    fn test_group_by_switch() {
        let tree = TopologyTree::from_switches(&make_tree_config());
        let candidates: Vec<&str> = vec!["node001", "node003", "node005"];
        let groups = tree.group_by_switch(&candidates);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups.get("rack01").unwrap().len(), 2);
        assert_eq!(groups.get("rack02").unwrap().len(), 1);
    }

    #[test]
    fn test_depths() {
        let tree = TopologyTree::from_switches(&make_tree_config());
        assert_eq!(tree.switches.get("fabric0").unwrap().depth, 0);
        assert_eq!(tree.switches.get("rack01").unwrap().depth, 1);
        assert_eq!(tree.switches.get("rack02").unwrap().depth, 1);
    }

    #[test]
    fn test_parent_links() {
        let tree = TopologyTree::from_switches(&make_tree_config());
        assert_eq!(
            tree.switches.get("rack01").unwrap().parent.as_deref(),
            Some("fabric0")
        );
        assert!(tree.switches.get("fabric0").unwrap().parent.is_none());
    }
}

#[cfg(test)]
mod external_config_tests {
    use super::*;

    #[test]
    fn legacy_topology_json_omits_external_policy() {
        let config: TopologyConfig =
            serde_json::from_str(r#"{"plugin":"none","switches":[],"block_size":null}"#).unwrap();
        assert!(config.external.is_none());
    }

    #[test]
    fn obsolete_partition_list_is_rejected() {
        assert!(toml::from_str::<TopologyConfig>(
            "[external]\nsingle_block_partitions = [\"rack\"]\n"
        )
        .is_err());
    }
}

#[cfg(test)]
mod discovery_tests {
    use super::*;
    use crate::fabric::{
        FabricBlock, FabricBlockKey, FabricDeviceKey, FabricMember, FabricView,
        UnresolvedFabricDevice,
    };

    fn view() -> FabricView {
        FabricView {
            notices: Vec::new(),
            namespace: "test".into(),
            unresolved: vec![],
            blocks: [("rack-a", "7", ["n0", "n2"]), ("rack-b", "7", ["n1", "n3"])]
                .into_iter()
                .map(|(physical, fabric, nodes)| FabricBlock {
                    key: FabricBlockKey {
                        namespace: "test".into(),
                        physical_domain: physical.into(),
                        fabric_domain: fabric.into(),
                    },
                    members: nodes
                        .into_iter()
                        .map(|node| FabricMember {
                            device: FabricDeviceKey {
                                node: node.into(),
                                stable_id: 42,
                            },
                            endpoint: node.into(),
                            inventory_generation: 1,
                        })
                        .collect(),
                    active_endpoints: vec![],
                    physical_capacity: None,
                    fabric_capacity: None,
                    issues: vec![],
                    file_revision: None,
                    observed_key: None,
                })
                .collect(),
        }
    }

    #[test]
    fn native_and_declared_membership_use_existing_block_and_tree_models() {
        for plugin in ["block", "tree"] {
            let config = TopologyConfig {
                plugin: plugin.into(),
                block_size: Some(4),
                ..Default::default()
            };
            for declared in [false, true] {
                let mut view = view();
                if declared {
                    for block in &mut view.blocks {
                        block.file_revision = Some("revision".into());
                    }
                }
                let tree = config.build(&[], &view).unwrap();
                assert_eq!(tree.switches.len(), 2);
                assert_eq!(tree.node_switch["n0"], tree.node_switch["n2"]);
                assert_ne!(tree.node_switch["n0"], tree.node_switch["n1"]);
                assert_eq!(tree.distance("n0", "n2"), 0);
                assert_eq!(tree.distance("n0", "n1"), u32::MAX);
                assert_eq!(
                    tree.select_local_nodes(&["n0", "n1", "n2", "n3"], 2),
                    ["n0", "n2"]
                );
                assert!(tree.switches.values().all(|switch| switch.parent.is_none()));
            }
        }
    }

    #[test]
    fn split_incomplete_and_invalid_nodes_have_no_inferred_leaf() {
        let mut view = view();
        let member = view.blocks[0].members[0].clone();
        view.blocks[1].members.push(member);
        let tree = TopologyTree::from_fabric(&view);
        assert!(!tree.node_switch.contains_key("n0"));
        assert_eq!(tree.node_switch.len(), 3);

        view.unresolved.push(UnresolvedFabricDevice {
            device: FabricDeviceKey {
                node: "n2".into(),
                stable_id: 43,
            },
            issue: "missing_observation".into(),
        });
        view.blocks[1].issues.push("stale_node".into());
        assert!(TopologyTree::from_fabric(&view).switches.is_empty());
        let config = TopologyConfig {
            plugin: "block".into(),
            ..Default::default()
        };
        assert!(config.build(&[], &view).unwrap().switches.is_empty());
    }

    #[test]
    fn unavailable_source_does_not_fall_back_to_sized_blocks() {
        let config = TopologyConfig {
            plugin: "block".into(),
            ..Default::default()
        };
        let nodes = [crate::node::Node::new("n0".into(), Default::default())];
        let mut view = view();
        view.blocks.clear();
        view.unresolved.push(UnresolvedFabricDevice {
            device: FabricDeviceKey {
                node: "n0".into(),
                stable_id: 42,
            },
            issue: "missing_domain".into(),
        });
        assert!(config.build(&nodes, &view).unwrap().node_switch.is_empty());
        view.unresolved.clear();
        assert_eq!(config.build(&nodes, &view).unwrap().node_switch.len(), 1);
        let file_config = TopologyConfig {
            external: Some(ExternalFabricConfig {
                block_file: Some("/etc/spur/topology.conf".into()),
                ..Default::default()
            }),
            ..config
        };
        assert!(file_config
            .build(&nodes, &view)
            .unwrap()
            .node_switch
            .is_empty());
    }

    #[test]
    fn explicit_switch_hierarchy_and_disabled_plugin_keep_their_meaning() {
        let config = TopologyConfig {
            plugin: "tree".into(),
            switches: vec![SwitchConfig {
                name: "leaf".into(),
                nodes: Some("n[0-3]".into()),
                switches: None,
            }],
            ..Default::default()
        };
        assert_eq!(config.build(&[], &view()).unwrap().switches.len(), 1);
        assert!(TopologyConfig::default().build(&[], &view()).is_none());
    }

    #[test]
    fn legacy_sized_blocks_are_independent_of_inventory_order() {
        let config = TopologyConfig {
            plugin: "block".into(),
            block_size: Some(2),
            ..Default::default()
        };
        let mut view = view();
        view.blocks.clear();
        let mut nodes: Vec<_> = ["n3", "n0", "n2", "n1"]
            .into_iter()
            .map(|name| crate::node::Node::new(name.into(), Default::default()))
            .collect();
        let first = config.build(&nodes, &view).unwrap();
        assert_eq!(first.node_switch["n0"], first.node_switch["n1"]);
        assert_ne!(first.node_switch["n0"], first.node_switch["n2"]);
        nodes.reverse();
        assert_eq!(
            first.node_switch,
            config.build(&nodes, &view).unwrap().node_switch
        );
    }

    #[test]
    fn unknown_nodes_do_not_form_a_fake_locality_group() {
        let tree = TopologyTree::from_fabric(&view());
        assert_eq!(
            tree.select_local_nodes(&["u0", "u1", "u2", "n0", "n2"], 2),
            ["n0", "n2"]
        );
        assert_eq!(
            tree.select_local_nodes(&["u0", "u1", "u2"], 2),
            ["u0", "u1"]
        );
        assert_eq!(
            tree.select_local_nodes(&["n0", "u0", "u1"], 2),
            ["n0", "u0"]
        );
    }
}
