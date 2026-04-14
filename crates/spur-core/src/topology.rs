use std::collections::HashMap;

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

/// The full fabric topology, built from config.
///
/// Supports two modes:
/// - **tree**: Hierarchical switch topology (fat-tree, dragonfly).
///   Nodes are grouped under leaf switches, which connect to aggregation switches.
/// - **block**: Flat grouping where nodes are divided into fixed-size blocks (racks).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopologyTree {
    /// All switches by name.
    pub switches: HashMap<String, Switch>,
    /// node_name → leaf switch name (fast lookup).
    pub node_switch: HashMap<String, String>,
}

impl TopologyTree {
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

        let groups = self.group_by_switch(candidates);
        let mut sorted_groups: Vec<(String, Vec<&'a str>)> = groups.into_iter().collect();
        sorted_groups.sort_by(|a, b| b.1.len().cmp(&a.1.len()));

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
        groups_with_dist.sort_by_key(|(dist, _, _)| *dist);

        for (_, _, nodes) in groups_with_dist {
            let remaining = count - result.len();
            if remaining == 0 {
                break;
            }
            let take = remaining.min(nodes.len());
            result.extend_from_slice(&nodes[..take]);
        }

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
    /// Plugin type: "tree", "block", or "none".
    #[serde(default = "default_topo_plugin")]
    pub plugin: String,
    /// Switch definitions (for tree mode).
    #[serde(default)]
    pub switches: Vec<SwitchConfig>,
    /// Block size (for block mode): how many nodes per block/rack.
    pub block_size: Option<usize>,
}

fn default_topo_plugin() -> String {
    "none".into()
}

impl Default for TopologyConfig {
    fn default() -> Self {
        Self {
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
