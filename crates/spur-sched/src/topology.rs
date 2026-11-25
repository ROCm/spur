// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Topology-aware ordering of placement candidates.

use chrono::{DateTime, Utc};
use spur_core::topology::TopologyTree;

/// Move the topologically closest `needed_nodes` candidates to the front.
///
/// `candidates` are in placement order (earliest start first). Entries the
/// topology does not pick keep their relative order behind the picked ones.
/// The input is returned unchanged unless a multi-node job requests "tree" or
/// "block" locality and a topology is configured.
pub fn order_by_locality<'a, T>(
    mut candidates: Vec<(T, DateTime<Utc>)>,
    needed_nodes: usize,
    request: Option<&str>,
    topology: Option<&TopologyTree>,
    name_of: impl Fn(&T) -> &'a str,
) -> Vec<(T, DateTime<Utc>)> {
    if needed_nodes <= 1 || !wants_locality(request) {
        return candidates;
    }
    let Some(topology) = topology else {
        return candidates;
    };
    let Some(&(_, earliest_window)) = candidates.get(needed_nodes - 1) else {
        return candidates;
    };

    // Locality must not delay a placement that can start sooner.
    let window_names: Vec<&str> = candidates
        .iter()
        .take_while(|(_, start)| *start <= earliest_window)
        .map(|(item, _)| name_of(item))
        .collect();
    let local_order = topology.select_local_nodes(&window_names, needed_nodes);

    let mut reordered = Vec::with_capacity(candidates.len());
    for name in local_order {
        if let Some(pos) = candidates
            .iter()
            .position(|(item, _)| name_of(item) == name)
        {
            reordered.push(candidates.remove(pos));
        }
    }
    reordered.extend(candidates);
    reordered
}

fn wants_locality(request: Option<&str>) -> bool {
    matches!(request, Some("tree" | "block"))
}

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use spur_core::topology::SwitchConfig;

    use super::*;

    fn at(minutes: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).expect("fixture time")
            + Duration::minutes(minutes)
    }

    /// rack01 = node001-002, rack02 = node003-005, both under one fabric switch.
    fn tree() -> TopologyTree {
        TopologyTree::from_switches(&[
            SwitchConfig {
                name: "rack01".into(),
                nodes: Some("node[001-002]".into()),
                switches: None,
            },
            SwitchConfig {
                name: "rack02".into(),
                nodes: Some("node[003-005]".into()),
                switches: None,
            },
            SwitchConfig {
                name: "fabric".into(),
                nodes: None,
                switches: Some("rack01,rack02".into()),
            },
        ])
    }

    fn blocks(count: usize, size: usize) -> TopologyTree {
        let names: Vec<String> = (1..=count).map(|i| format!("node{i:03}")).collect();
        TopologyTree::from_blocks(&names, size)
    }

    fn order(
        candidates: &[(&'static str, i64)],
        needed_nodes: usize,
        request: Option<&str>,
        topology: Option<&TopologyTree>,
    ) -> Vec<(&'static str, i64)> {
        let candidates = candidates
            .iter()
            .map(|&(name, minutes)| (name, at(minutes)))
            .collect();
        order_by_locality(candidates, needed_nodes, request, topology, |name| *name)
            .into_iter()
            .map(|(name, start)| (name, (start - at(0)).num_minutes()))
            .collect()
    }

    fn names(candidates: &[(&'static str, i64)]) -> Vec<&'static str> {
        candidates.iter().map(|(name, _)| *name).collect()
    }

    #[test]
    fn single_node_job_is_unchanged() {
        let input = [
            ("node003", 0),
            ("node001", 0),
            ("node004", 0),
            ("node002", 0),
        ];
        let topology = blocks(4, 2);
        assert_eq!(
            names(&order(&input, 1, Some("block"), Some(&topology))),
            ["node003", "node001", "node004", "node002"]
        );
    }

    #[test]
    fn no_topology_is_unchanged() {
        let input = [("node001", 0), ("node003", 0), ("node002", 0)];
        assert_eq!(
            names(&order(&input, 2, Some("tree"), None)),
            ["node001", "node003", "node002"]
        );
    }

    #[test]
    fn non_locality_requests_are_unchanged() {
        let input = [("node001", 0), ("node003", 0), ("node002", 0)];
        let topology = tree();
        for request in [None, Some("none"), Some("TREE"), Some("dragonfly")] {
            assert_eq!(
                names(&order(&input, 2, request, Some(&topology))),
                ["node001", "node003", "node002"],
                "{request:?}"
            );
        }
    }

    #[test]
    fn tree_prefers_one_switch_and_keeps_leftovers_in_order() {
        let input = [
            ("node001", 0),
            ("node003", 0),
            ("node002", 0),
            ("node004", 0),
            ("node005", 0),
        ];
        let topology = tree();
        assert_eq!(
            names(&order(&input, 2, Some("tree"), Some(&topology))),
            ["node003", "node004", "node001", "node002", "node005"]
        );
    }

    #[test]
    fn block_prefers_one_block_with_name_ordered_ties() {
        let input = [
            ("node001", 0),
            ("node003", 0),
            ("node004", 0),
            ("node002", 0),
        ];
        let topology = blocks(4, 2);
        assert_eq!(
            names(&order(&input, 2, Some("block"), Some(&topology))),
            ["node001", "node002", "node003", "node004"]
        );
    }

    #[test]
    fn spanning_switches_keeps_group_and_leftover_order() {
        let input = [
            ("node005", 0),
            ("node001", 0),
            ("node003", 0),
            ("node002", 0),
            ("node004", 0),
        ];
        let topology = tree();
        assert_eq!(
            names(&order(&input, 4, Some("tree"), Some(&topology))),
            ["node005", "node003", "node004", "node001", "node002"]
        );
    }

    #[test]
    fn later_start_is_never_promoted_past_the_earliest_window() {
        let topology = blocks(3, 2);
        assert_eq!(
            order(
                &[("node001", 0), ("node003", 0), ("node002", 10)],
                2,
                Some("block"),
                Some(&topology),
            ),
            [("node001", 0), ("node003", 0), ("node002", 10)]
        );
        assert_eq!(
            order(
                &[("node001", 0), ("node003", 0), ("node002", 0)],
                2,
                Some("block"),
                Some(&topology),
            ),
            [("node001", 0), ("node002", 0), ("node003", 0)]
        );
    }

    #[test]
    fn unknown_nodes_do_not_form_a_locality_group() {
        let topology = tree();
        assert_eq!(
            names(&order(
                &[("u0", 0), ("u1", 0), ("node001", 0), ("node002", 0)],
                2,
                Some("tree"),
                Some(&topology),
            )),
            ["node001", "node002", "u0", "u1"]
        );
        assert_eq!(
            names(&order(
                &[("u0", 0), ("node001", 0), ("u1", 0)],
                2,
                Some("tree"),
                Some(&topology),
            )),
            ["node001", "u0", "u1"]
        );
    }

    #[test]
    fn zero_needed_nodes_is_unchanged() {
        let input = [("node003", 0), ("node001", 0)];
        let topology = tree();
        assert_eq!(
            names(&order(&input, 0, Some("tree"), Some(&topology))),
            ["node003", "node001"]
        );
    }

    #[test]
    fn too_few_candidates_is_unchanged() {
        let input = [("node003", 0), ("node001", 0)];
        let topology = tree();
        assert_eq!(
            names(&order(&input, 3, Some("tree"), Some(&topology))),
            ["node003", "node001"]
        );
    }
}
