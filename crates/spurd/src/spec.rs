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
//! so tightening never races a job into the OOM killer. [`SpecEnforcer`] holds
//! the adopted layout and applies it to the real cgroup root.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

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

/// Holds the layout the controller last delivered and applies it to the
/// `spur/` cgroup root. One per agent; the registration reply, launch requests,
/// and the heartbeat loop all drive it.
pub struct SpecEnforcer {
    root: PathBuf,
    held: Option<CoreLayout>,
    /// Whether `memory.max` has already been tightened to the target, so a later
    /// usage spike cannot loosen the reservation back to pending.
    max_tight: bool,
    /// Whether root cpuset/memory have been written and so must be reset when the
    /// layout is cleared. Nodes that never set spec cores never touch the root.
    active: bool,
}

impl SpecEnforcer {
    pub fn new(root: PathBuf) -> Self {
        SpecEnforcer {
            root,
            held: None,
            max_tight: false,
            active: false,
        }
    }

    /// The fingerprint of the held layout, or 0 when none is held — exactly what
    /// the heartbeat reports so the controller knows whether to redeliver.
    pub fn held_fingerprint(&self) -> u64 {
        self.held.as_ref().map(CoreLayout::fingerprint).unwrap_or(0)
    }

    /// Adopt a delivered layout. `None` clears the held layout (the controller
    /// dropped the node's spec settings). A `Some` layout is rejected when its
    /// generation is older than the held one — a late launch must not overwrite
    /// a newer layout adopted from a registration reply (finding 6) — and is a
    /// no-op when its content matches what is already held. Returns true when the
    /// held layout changed, so the caller re-applies and re-reports.
    pub fn adopt(&mut self, incoming: Option<CoreLayout>) -> bool {
        match (incoming, &self.held) {
            (None, None) => false,
            (None, Some(_)) => {
                self.held = None;
                self.max_tight = false;
                true
            }
            (Some(new), Some(held)) if new.generation < held.generation => false,
            (Some(new), Some(held)) if new.fingerprint() == held.fingerprint() => false,
            (Some(new), _) => {
                self.max_tight = false;
                self.held = Some(new);
                true
            }
        }
    }

    /// Apply the held layout to the cgroup root. `live_job_cpusets` are the
    /// current `cpuset.cpus` of each live job cgroup, `total_mem_mb` the node's
    /// memory, `current_mem_mb` the root's live `memory.current`. Returns the
    /// memory reservation state; any write error bubbles up and makes the node
    /// unschedulable. A no-op for a node that has never held spec cores.
    pub fn apply(
        &mut self,
        live_job_cpusets: &[Vec<u32>],
        total_mem_mb: u64,
        current_mem_mb: u64,
    ) -> io::Result<MemReservation> {
        let enforce = self.held.as_ref().is_some_and(|l| !l.spec_cpus.is_empty());
        if !enforce {
            if self.active {
                self.reset_root()?;
                self.active = false;
            }
            return Ok(MemReservation::Inactive);
        }
        let layout = self.held.as_ref().expect("enforce implies held");
        let desired = root_cpuset(layout);
        let effective = reconcile_root_cpuset(&desired, live_job_cpusets);
        write_cgroup(&self.root, "cpuset.cpus", &cpu_list(&effective))?;
        let target = total_mem_mb.saturating_sub(layout.mem_spec_mb);
        let decision = mem_decision(Some(target), current_mem_mb, self.max_tight);
        if let Some(high) = decision.memory_high_mb {
            write_cgroup(&self.root, "memory.high", &(high * 1024 * 1024).to_string())?;
        }
        match decision.memory_max_mb {
            Some(max) => {
                write_cgroup(&self.root, "memory.max", &(max * 1024 * 1024).to_string())?;
                self.max_tight = true;
            }
            None => write_cgroup(&self.root, "memory.max", "max")?,
        }
        self.active = true;
        Ok(decision.state)
    }

    /// The CPU set `spurd` and each `spurstepd` should be pinned to right now, or
    /// `None` to widen back to every online CPU (no spec cores, or the setting is
    /// off). Thin wrapper over [`daemon_cpuset`] on the held layout.
    pub fn daemon_cpuset(&self) -> Option<Vec<u32>> {
        self.held.as_ref().and_then(daemon_cpuset)
    }

    /// Return the root to inherited CPUs and uncapped memory when spec settings
    /// are removed.
    fn reset_root(&self) -> io::Result<()> {
        write_cgroup(&self.root, "cpuset.cpus", "")?;
        write_cgroup(&self.root, "memory.high", "max")?;
        write_cgroup(&self.root, "memory.max", "max")?;
        Ok(())
    }
}

fn cpu_list(cpus: &[u32]) -> String {
    cpus.iter()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn write_cgroup(root: &Path, file: &str, value: &str) -> io::Result<()> {
    std::fs::write(root.join(file), value)
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
            generation: 0,
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

    fn gen_layout(spec: &[u32], general: &[u32], mem_mb: u64, generation: u64) -> CoreLayout {
        let mut l = layout(spec, general, &[], mem_mb);
        l.generation = generation;
        l
    }

    #[test]
    fn adopt_holds_and_reports_fingerprint() {
        let mut e = SpecEnforcer::new(PathBuf::from("/unused"));
        assert_eq!(e.held_fingerprint(), 0);
        let l = gen_layout(&[127], &[0, 1], 1024, 5);
        assert!(e.adopt(Some(l.clone())));
        assert_eq!(e.held_fingerprint(), l.fingerprint());
    }

    #[test]
    fn adopt_rejects_an_older_generation_but_takes_a_newer_one() {
        let mut e = SpecEnforcer::new(PathBuf::from("/unused"));
        e.adopt(Some(gen_layout(&[127], &[0, 1], 1024, 5)));
        // A late launch carrying generation 4 (older) must not overwrite.
        assert!(!e.adopt(Some(gen_layout(&[63], &[0, 2], 1024, 4))));
        assert_eq!(
            e.held_fingerprint(),
            gen_layout(&[127], &[0, 1], 1024, 5).fingerprint()
        );
        // A newer generation is adopted.
        assert!(e.adopt(Some(gen_layout(&[63], &[0, 2], 1024, 6))));
        assert_eq!(
            e.held_fingerprint(),
            gen_layout(&[63], &[0, 2], 1024, 6).fingerprint()
        );
    }

    #[test]
    fn adopt_ignores_identical_content_and_clears_on_none() {
        let mut e = SpecEnforcer::new(PathBuf::from("/unused"));
        let l = gen_layout(&[127], &[0, 1], 1024, 5);
        assert!(e.adopt(Some(l.clone())));
        assert!(!e.adopt(Some(l.clone()))); // same content, no change
        assert!(e.adopt(None)); // clear
        assert_eq!(e.held_fingerprint(), 0);
        assert!(!e.adopt(None)); // already clear
    }

    fn read(root: &Path, file: &str) -> Option<String> {
        std::fs::read_to_string(root.join(file)).ok()
    }

    #[test]
    fn apply_writes_root_cpuset_and_sequences_memory() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = SpecEnforcer::new(dir.path().to_path_buf());
        e.adopt(Some(gen_layout(&[127, 255], &[0, 1, 2], 1024, 1)));

        // Usage over target (7168): high throttles, max stays uncapped, pending.
        let state = e.apply(&[], 8192, 7500).unwrap();
        assert_eq!(state, MemReservation::Pending);
        assert_eq!(read(dir.path(), "cpuset.cpus").as_deref(), Some("0,1,2"));
        assert_eq!(
            read(dir.path(), "memory.high").as_deref(),
            Some(((8192u64 - 1024) * 1024 * 1024).to_string().as_str())
        );
        assert_eq!(read(dir.path(), "memory.max").as_deref(), Some("max"));

        // Usage settles under target: max tightens, active.
        let state = e.apply(&[], 8192, 1000).unwrap();
        assert_eq!(state, MemReservation::Active);
        assert_eq!(
            read(dir.path(), "memory.max").as_deref(),
            Some(((8192u64 - 1024) * 1024 * 1024).to_string().as_str())
        );
    }

    #[test]
    fn apply_defers_a_cpu_a_live_job_sits_on() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = SpecEnforcer::new(dir.path().to_path_buf());
        // Spec just took cores {2,3}; a running job is pinned entirely to them,
        // so dropping the root to {0,1} would empty its intersection and widen
        // it — 2 and 3 are kept until the job exits.
        e.adopt(Some(gen_layout(&[2, 3], &[0, 1], 0, 1)));
        e.apply(&[vec![2, 3]], 4096, 10).unwrap();
        assert_eq!(read(dir.path(), "cpuset.cpus").as_deref(), Some("0,1,2,3"));
    }

    #[test]
    fn apply_resets_the_root_when_spec_is_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = SpecEnforcer::new(dir.path().to_path_buf());
        e.adopt(Some(gen_layout(&[127], &[0, 1], 1024, 1)));
        e.apply(&[], 8192, 100).unwrap();
        assert_eq!(read(dir.path(), "cpuset.cpus").as_deref(), Some("0,1"));

        e.adopt(None);
        e.apply(&[], 8192, 100).unwrap();
        assert_eq!(read(dir.path(), "cpuset.cpus").as_deref(), Some(""));
        assert_eq!(read(dir.path(), "memory.max").as_deref(), Some("max"));
    }

    #[test]
    fn apply_never_touches_the_root_without_spec_cores() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = SpecEnforcer::new(dir.path().to_path_buf());
        // No layout held at all.
        assert_eq!(e.apply(&[], 8192, 100).unwrap(), MemReservation::Inactive);
        assert_eq!(read(dir.path(), "cpuset.cpus"), None);
        assert_eq!(read(dir.path(), "memory.max"), None);
    }
}
