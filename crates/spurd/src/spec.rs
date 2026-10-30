// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Node-level core and memory specialization enforcement (SPUR-386).
//!
//! The controller computes a [`CoreLayout`] and delivers it to the agent; this
//! module turns a held layout into the writes the `spur/` cgroup root needs so
//! that no job — every job lives under that root — can land on a spec CPU or
//! grow into memory reserved for node services (e.g. a WEKA client).
//!
//! The policy is pure and testable without a cgroupfs: [`root_cpuset`] derives
//! the root's CPU confinement, [`reconcile_root_cpuset`] honours cgroup v2's
//! rule that a child whose cpuset stops intersecting its parent's *widens*
//! rather than empties (so a CPU is kept in the root until the last job holding
//! it exits), and [`mem_decision`] sequences `memory.high` before `memory.max`
//! so tightening never races a job into the OOM killer. The impure applier that
//! writes these to `/sys/fs/cgroup/spur` lives alongside the agent.

use std::collections::BTreeSet;

use spur_core::core_layout::CoreLayout;

/// The CPU set the `spur/` root is confined to: every non-spec CPU, i.e. the
/// layout's general pool unioned with every GPU-reserved pool. Sorted.
///
/// Confinement is only meaningful once spec CPUs exist; callers gate the root
/// write on `!layout.spec_cpus.is_empty()` and otherwise leave the root to
/// inherit (a reserved-only SPUR-369 layout enforces its pools per job, not at
/// the root).
pub fn root_cpuset(layout: &CoreLayout) -> Vec<u32> {
    let mut cpus: BTreeSet<u32> = layout.general_cpus.iter().copied().collect();
    for gpu in &layout.gpu_reserved {
        cpus.extend(gpu.cpus.iter().copied());
    }
    cpus.into_iter().collect()
}

/// Reconcile a shrinking root cpuset against the cpusets of live job cgroups.
///
/// cgroup v2 treats a child whose `cpuset.cpus` no longer intersects its
/// parent's as if the child's cpuset were empty, and *widens* the child to
/// inherit the parent — the opposite of isolation. So a CPU that `desired`
/// would drop is kept in the root while any live job's cpuset would otherwise
/// be left with an empty intersection; it is released only once that job exits
/// (design review finding 3).
///
/// Returns the set to actually write, sorted. A CPU in neither `desired` nor
/// any blocking job's cpuset is removed immediately.
pub fn reconcile_root_cpuset(desired: &[u32], live_job_cpusets: &[Vec<u32>]) -> Vec<u32> {
    let desired_set: BTreeSet<u32> = desired.iter().copied().collect();
    let mut keep = desired_set.clone();
    for job in live_job_cpusets {
        let job_set: BTreeSet<u32> = job.iter().copied().collect();
        if job_set.is_empty() {
            continue;
        }
        if job_set.is_disjoint(&desired_set) {
            // Dropping to `desired` would empty this job's intersection with the
            // root and widen it; defer by keeping the CPUs it still holds.
            keep.extend(job_set);
        }
    }
    keep.into_iter().collect()
}

/// Whether the node's reserved memory is being enforced yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemReservation {
    /// No spec memory target (unset or reset).
    Inactive,
    /// `memory.high` is throttling toward the target but usage has not settled
    /// under it, so `memory.max` is not yet tightened and the reservation is
    /// not advertised as live.
    Pending,
    /// Usage settled under the target and `memory.max` is tightened to it.
    Active,
}

/// What to write to the root's memory controls this tick, and the resulting
/// reservation state. `*_mb == None` means write the literal `max` (reset).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemDecision {
    pub memory_high_mb: Option<u64>,
    pub memory_max_mb: Option<u64>,
    pub state: MemReservation,
}

/// Sequence the root's memory tightening (design review finding 5).
///
/// `target_mb` is total node memory minus the reserved amount, present only
/// when spec cores are set. `current_mb` is the live `memory.current` of the
/// root (MB). `max_already_tight` records whether `memory.max` has already been
/// lowered to the target on a prior tick.
///
/// `memory.high` drops to the target immediately — it only throttles and
/// reclaims, never OOM-kills — while `memory.max` is lowered to the target only
/// once usage is confirmed at or below it, so tightening cannot force an
/// unreclaimable cgroup into the OOM killer. Until then the reservation is
/// [`MemReservation::Pending`].
pub fn mem_decision(
    target_mb: Option<u64>,
    current_mb: u64,
    max_already_tight: bool,
) -> MemDecision {
    let Some(target) = target_mb else {
        return MemDecision {
            memory_high_mb: None,
            memory_max_mb: None,
            state: MemReservation::Inactive,
        };
    };
    if max_already_tight || current_mb <= target {
        MemDecision {
            memory_high_mb: Some(target),
            memory_max_mb: Some(target),
            state: MemReservation::Active,
        }
    } else {
        MemDecision {
            memory_high_mb: Some(target),
            memory_max_mb: None,
            state: MemReservation::Pending,
        }
    }
}

/// The CPU set `spurd` and each `spurstepd` are pinned to when `daemons_off_spec`
/// is set: the same non-spec set the root is confined to. `None` means do not
/// constrain the daemons (no spec CPUs, or the setting is off), i.e. widen back
/// to every online CPU.
pub fn daemon_cpuset(layout: &CoreLayout) -> Option<Vec<u32>> {
    if !layout.daemons_off_spec || layout.spec_cpus.is_empty() {
        return None;
    }
    Some(root_cpuset(layout))
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_core::core_layout::GpuReservedCpus;

    fn layout(spec: &[u32], general: &[u32], gpu: &[(u64, &[u32])], mem_mb: u64) -> CoreLayout {
        CoreLayout {
            spec_cpus: spec.to_vec(),
            mem_spec_mb: mem_mb,
            gpu_reserved: gpu
                .iter()
                .map(|(id, cpus)| GpuReservedCpus {
                    stable_id: *id,
                    cpus: cpus.to_vec(),
                })
                .collect(),
            general_cpus: general.to_vec(),
            daemons_off_spec: false,
        }
    }

    #[test]
    fn root_cpuset_is_general_plus_every_reserved_pool_sorted() {
        let l = layout(&[127, 255], &[0, 2, 4], &[(10, &[1, 3]), (11, &[5])], 0);
        assert_eq!(root_cpuset(&l), vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn root_cpuset_dedups_overlapping_pools() {
        let l = layout(&[], &[0, 1], &[(10, &[1, 2])], 0);
        assert_eq!(root_cpuset(&l), vec![0, 1, 2]);
    }

    #[test]
    fn reconcile_removes_cpus_no_live_job_needs() {
        // Desired drops CPU 7; the only job lives entirely on 0-3, so 7 goes.
        let out = reconcile_root_cpuset(&[0, 1, 2, 3], &[vec![0, 1]]);
        assert_eq!(out, vec![0, 1, 2, 3]);
    }

    #[test]
    fn reconcile_defers_a_cpu_a_live_job_still_sits_on() {
        // Spec just took CPU 4; a running job is pinned to {4,5}. Dropping to
        // {0,1} would empty the job's intersection with the root and widen it,
        // so 4 and 5 are kept until the job exits.
        let out = reconcile_root_cpuset(&[0, 1], &[vec![4, 5]]);
        assert_eq!(out, vec![0, 1, 4, 5]);
    }

    #[test]
    fn reconcile_keeps_root_when_job_still_partly_intersects() {
        // Job on {1,4}: it still has CPU 1 in the desired root, so no deferral —
        // CPU 4 is dropped even though the job holds it.
        let out = reconcile_root_cpuset(&[0, 1], &[vec![1, 4]]);
        assert_eq!(out, vec![0, 1]);
    }

    #[test]
    fn reconcile_ignores_jobs_with_no_recorded_cpuset() {
        let out = reconcile_root_cpuset(&[0, 1], &[vec![]]);
        assert_eq!(out, vec![0, 1]);
    }

    #[test]
    fn mem_inactive_when_no_target() {
        let d = mem_decision(None, 999, false);
        assert_eq!(
            d,
            MemDecision {
                memory_high_mb: None,
                memory_max_mb: None,
                state: MemReservation::Inactive,
            }
        );
    }

    #[test]
    fn mem_pending_throttles_high_but_not_max_while_over_target() {
        let d = mem_decision(Some(1000), 1500, false);
        assert_eq!(d.memory_high_mb, Some(1000));
        assert_eq!(d.memory_max_mb, None);
        assert_eq!(d.state, MemReservation::Pending);
    }

    #[test]
    fn mem_activates_once_usage_settles_under_target() {
        let d = mem_decision(Some(1000), 1000, false);
        assert_eq!(d.memory_high_mb, Some(1000));
        assert_eq!(d.memory_max_mb, Some(1000));
        assert_eq!(d.state, MemReservation::Active);
    }

    #[test]
    fn mem_stays_active_even_if_usage_later_rises() {
        // Once max is tight, a later spike must not loosen it back to pending.
        let d = mem_decision(Some(1000), 1200, true);
        assert_eq!(d.memory_max_mb, Some(1000));
        assert_eq!(d.state, MemReservation::Active);
    }

    #[test]
    fn daemon_cpuset_none_unless_off_spec_and_spec_present() {
        let mut l = layout(&[127], &[0, 1], &[], 0);
        assert_eq!(daemon_cpuset(&l), None); // off_spec defaults false
        l.daemons_off_spec = true;
        assert_eq!(daemon_cpuset(&l), Some(vec![0, 1]));
        l.spec_cpus.clear();
        assert_eq!(daemon_cpuset(&l), None); // no spec cores -> widen
    }
}
