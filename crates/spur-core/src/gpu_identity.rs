// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Which silicon a GPU `stable_id` names, and how two inventories' ids relate.

use std::collections::{BTreeSet, HashMap, HashSet};

use spur_devices::cdi::STABLE_ID_PARTITION_MASK;

const FUNCTION_SHIFT: u32 = 8;
const FUNCTION_MASK: u64 = 0x7 << FUNCTION_SHIFT;

pub fn anchor(id: u64) -> u64 {
    id & !STABLE_ID_PARTITION_MASK
}

pub fn rank(id: u64) -> u64 {
    id & STABLE_ID_PARTITION_MASK
}

fn function(id: u64) -> u64 {
    (id & FUNCTION_MASK) >> FUNCTION_SHIFT
}

fn device_anchor(id: u64) -> u64 {
    id & !(STABLE_ID_PARTITION_MASK | FUNCTION_MASK)
}

/// Anchor 0 is BDF 0000:00:00.0, the host bridge, never a GPU: ids there are
/// positional backfills, so a change in their count is not a repartition.
fn is_bdf_anchor(anchor: u64) -> bool {
    anchor != 0
}

/// Older builds put a partition's rank in the PCI function bits. Such an id is
/// read as rank `f` of function 0 only when nothing sits on its own anchor and
/// function 0 of the same device is present, so a real multi-function device
/// keeps its id.
fn legacy_normalized(old: u64, anchors: &HashSet<u64>) -> Option<u64> {
    let f = function(old);
    let base = device_anchor(old);
    (f != 0
        && rank(old) == 0
        && is_bdf_anchor(base)
        && !anchors.contains(&anchor(old))
        && anchors.contains(&base))
    .then_some(base | f)
}

/// Anchor of the GPU `id` sits on in an inventory with these `anchors`, so a
/// legacy id resolves to the GPU its partition lives on now.
pub fn current_anchor(id: u64, anchors: &HashSet<u64>) -> u64 {
    anchor(legacy_normalized(id, anchors).unwrap_or(id))
}

/// Anchors of the GPUs `held` ids sit on under `current`'s encoding, so a
/// legacy id fences the GPU its partition lives on now.
pub fn held_anchors(held: &HashSet<u64>, current: impl Iterator<Item = u64>) -> HashSet<u64> {
    let anchors: HashSet<u64> = current.map(anchor).collect();
    held.iter()
        .map(|&id| current_anchor(id, &anchors))
        .collect()
}

pub fn legacy_partition_id(old: u64, current: &HashSet<u64>) -> Option<u64> {
    let anchors: HashSet<u64> = current.iter().map(|&id| anchor(id)).collect();
    legacy_normalized(old, &anchors).filter(|id| current.contains(id))
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct LayoutDiff {
    /// Old id -> the id the same partition has under the current encoding.
    pub renamed: HashMap<u64, u64>,
    /// Anchors whose set of partitions changed: every id on them is void.
    pub repartitioned: HashSet<u64>,
}

impl LayoutDiff {
    pub fn current_id(&self, old: u64) -> u64 {
        self.renamed.get(&old).copied().unwrap_or(old)
    }

    /// Renames whose GPU kept its layout. A repartitioned GPU is different
    /// silicon under any id, so its holders keep the id they were given.
    pub fn safe_renames(&self) -> HashMap<u64, u64> {
        self.renamed
            .iter()
            .filter(|(_, to)| !self.repartitioned.contains(&anchor(**to)))
            .map(|(&from, &to)| (from, to))
            .collect()
    }

    /// The device still exists and still names the silicon it did.
    pub fn is_intact(&self, old: u64, current: &HashSet<u64>) -> bool {
        let id = self.current_id(old);
        current.contains(&id) && !self.repartitioned.contains(&anchor(id))
    }
}

fn ranks_by_anchor(ids: impl Iterator<Item = u64>) -> HashMap<u64, BTreeSet<u64>> {
    let mut grouped: HashMap<u64, BTreeSet<u64>> = HashMap::new();
    for id in ids {
        grouped.entry(anchor(id)).or_default().insert(rank(id));
    }
    grouped
}

pub fn diff_layout(old: &[u64], new: &[u64]) -> LayoutDiff {
    let anchors: HashSet<u64> = new.iter().map(|&id| anchor(id)).collect();
    let normalized: Vec<(u64, u64)> = old
        .iter()
        .map(|&id| (id, legacy_normalized(id, &anchors).unwrap_or(id)))
        .collect();
    let before = ranks_by_anchor(normalized.iter().map(|&(_, id)| id));
    let after = ranks_by_anchor(new.iter().copied());
    let repartitioned: HashSet<u64> = before
        .iter()
        .filter(|(a, ranks)| {
            is_bdf_anchor(**a) && after.get(*a).is_some_and(|fresh| fresh != *ranks)
        })
        .map(|(a, _)| *a)
        .collect();
    let renamed = normalized
        .into_iter()
        .filter(|(old, id)| old != id)
        .collect();
    LayoutDiff {
        renamed,
        repartitioned,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(bus: u64, function: u64, rank: u64) -> u64 {
        (bus << 16) | (function << 8) | rank
    }

    fn layout(bus: u64, partitions: u64) -> Vec<u64> {
        (0..partitions).map(|r| sid(bus, 0, r)).collect()
    }

    fn legacy_cpx(bus: u64, partitions: u64) -> Vec<u64> {
        (0..partitions).map(|f| sid(bus, f, 0)).collect()
    }

    #[test]
    fn unchanged_inventory_has_no_renames_or_repartitions() {
        let ids = [layout(0x11, 8), layout(0x12, 1)].concat();
        assert_eq!(diff_layout(&ids, &ids), LayoutDiff::default());
    }

    #[test]
    fn the_legacy_cpx_encoding_renames_each_partition_in_place() {
        let old = [legacy_cpx(0x11, 8), layout(0x12, 1)].concat();
        let new = [layout(0x11, 8), layout(0x12, 1)].concat();
        let diff = diff_layout(&old, &new);
        assert!(diff.repartitioned.is_empty());
        assert_eq!(diff.renamed.len(), 7);
        for f in 1..8 {
            assert_eq!(diff.renamed[&sid(0x11, f, 0)], sid(0x11, 0, f));
        }
    }

    #[test]
    fn every_partition_mode_change_is_a_repartition_of_that_gpu_only() {
        for a in [1, 2, 4, 8] {
            for b in [1, 2, 4, 8] {
                if a == b {
                    continue;
                }
                let old = [layout(0x11, a), layout(0x12, 1)].concat();
                let new = [layout(0x11, b), layout(0x12, 1)].concat();
                let diff = diff_layout(&old, &new);
                assert_eq!(
                    diff.repartitioned,
                    HashSet::from([sid(0x11, 0, 0)]),
                    "{a}->{b}"
                );
                assert!(diff.renamed.is_empty(), "{a}->{b}");
            }
        }
    }

    #[test]
    fn a_legacy_cpx_gpu_switched_while_upgrading_is_a_repartition() {
        for b in [1, 2, 4] {
            let old = legacy_cpx(0x11, 8);
            let new = layout(0x11, b);
            let diff = diff_layout(&old, &new);
            assert_eq!(
                diff.repartitioned,
                HashSet::from([sid(0x11, 0, 0)]),
                "CPX->{b}"
            );
        }
    }

    #[test]
    fn a_held_partition_is_intact_only_without_a_repartition() {
        let old = layout(0x11, 1);
        let new = layout(0x11, 8);
        let current: HashSet<u64> = new.iter().copied().collect();
        let diff = diff_layout(&old, &new);
        // SPX id equals the new partition 0 id, but names different silicon now.
        assert!(!diff.is_intact(sid(0x11, 0, 0), &current));
        let same = diff_layout(&new, &new);
        assert!(same.is_intact(sid(0x11, 0, 3), &current));
    }

    #[test]
    fn a_vanished_gpu_is_neither_renamed_nor_repartitioned() {
        let old = [layout(0x11, 1), layout(0x12, 1)].concat();
        let new = layout(0x12, 1);
        assert_eq!(diff_layout(&old, &new), LayoutDiff::default());
    }

    #[test]
    fn positional_ids_are_never_read_as_a_partitioned_gpu() {
        let old: Vec<u64> = (0..8).collect();
        let new: Vec<u64> = (0..7).collect();
        assert_eq!(diff_layout(&old, &new), LayoutDiff::default());
        assert_eq!(diff_layout(&new, &old), LayoutDiff::default());
    }

    #[test]
    fn safe_renames_leave_out_renames_onto_a_repartitioned_gpu() {
        let old = [legacy_cpx(0x11, 8), legacy_cpx(0x12, 8)].concat();
        let new = [layout(0x11, 8), layout(0x12, 2)].concat();
        let diff = diff_layout(&old, &new);
        let safe = diff.safe_renames();
        assert_eq!(safe.len(), 7);
        assert_eq!(safe[&sid(0x11, 3, 0)], sid(0x11, 0, 3));
        assert!(safe.values().all(|&id| anchor(id) != sid(0x12, 0, 0)));
    }

    #[test]
    fn the_legacy_rule_needs_the_old_anchor_gone_and_the_rank_present() {
        let current: HashSet<u64> = layout(0x11, 8).into_iter().collect();
        assert_eq!(
            legacy_partition_id(sid(0x11, 3, 0), &current),
            Some(sid(0x11, 0, 3))
        );
        assert_eq!(legacy_partition_id(sid(0x11, 0, 0), &current), None);
        let spx: HashSet<u64> = layout(0x11, 1).into_iter().collect();
        assert_eq!(legacy_partition_id(sid(0x11, 3, 0), &spx), None);
        let real_function: HashSet<u64> = [sid(0x11, 3, 0), sid(0x11, 0, 3)].into();
        assert_eq!(legacy_partition_id(sid(0x11, 3, 0), &real_function), None);
    }

    #[test]
    fn a_legacy_id_resolves_to_the_anchor_of_the_gpu_it_lives_on_now() {
        let dpx: HashSet<u64> = [sid(0x11, 0, 0), sid(0x12, 0, 0)].into();
        assert_eq!(current_anchor(sid(0x11, 3, 0), &dpx), sid(0x11, 0, 0));
        assert_eq!(current_anchor(sid(0x12, 0, 1), &dpx), sid(0x12, 0, 0));
        let gone: HashSet<u64> = [sid(0x12, 0, 0)].into();
        assert_eq!(current_anchor(sid(0x11, 3, 0), &gone), sid(0x11, 3, 0));
    }
}
