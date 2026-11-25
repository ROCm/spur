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
