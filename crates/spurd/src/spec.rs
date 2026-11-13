// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Node-level core and memory specialization: turns the layout the controller
//! delivers into writes on the `spur/` cgroup root, which every job runs under.

use std::collections::{BTreeSet, HashMap};
use std::io;
use std::path::{Path, PathBuf};

use spur_core::core_layout::{CoreLayout, LayoutRevision};
use spur_core::cpu_topology::format_cpu_list;

/// The CPUs the `spur/` root is confined to: every non-spec CPU, sorted.
pub fn root_cpuset(layout: &CoreLayout) -> Vec<u32> {
    let mut cpus: BTreeSet<u32> = layout.general_cpus.iter().copied().collect();
    for gpu in &layout.gpu_reserved {
        cpus.extend(gpu.cpus.iter().copied());
    }
    cpus.into_iter().collect()
}

/// The root cpuset to write. cgroup v2 widens a child whose cpuset no longer
/// intersects its parent's, so CPUs a live job would otherwise lose entirely
/// stay in the root until it exits.
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
    /// `memory.high` throttles toward the target; `memory.max` waits for a
    /// moment with no process under the root.
    Pending,
    /// `memory.max` holds the target.
    Active,
}

/// What to write to the root's memory controls this tick. `memory_max_mb ==
/// None` leaves `memory.max` as it is; only a full reset loosens it to `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemDecision {
    pub memory_high_mb: u64,
    pub memory_max_mb: Option<u64>,
    pub state: MemReservation,
}

/// `memory.high` (throttle only, never OOM) goes to the target at once.
/// `memory.max` is lowered only while no process runs under the root, since a
/// hard limit below a running job's usage can OOM it, or when the root's
/// current `memory.max` already holds it at or below the target.
pub fn mem_decision(
    target_mb: u64,
    quiescent: bool,
    current_max_mb: Option<u64>,
    max_already_tight: bool,
) -> MemDecision {
    let held_by_max = current_max_mb.is_some_and(|m| m <= target_mb);
    let tight = max_already_tight || quiescent || held_by_max;
    MemDecision {
        memory_high_mb: target_mb,
        memory_max_mb: tight.then_some(target_mb),
        state: if tight {
            MemReservation::Active
        } else {
            MemReservation::Pending
        },
    }
}

/// Where spurd's threads are pinned under `daemons_off_spec`; `None` widens them
/// back to every online CPU. Supervisors inherit the mask in effect when forked.
pub fn daemon_cpuset(layout: &CoreLayout) -> Option<Vec<u32>> {
    if !layout.daemons_off_spec || layout.spec_cpus.is_empty() {
        return None;
    }
    Some(root_cpuset(layout))
}

/// Where a layout delivery came from. Only a registration reply may clear the
/// held layout, and not when the controller reports it could not compute one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Registration {
        layout_error: bool,
        ticket: RegistrationTicket,
    },
    Launch,
}

/// Issued when a registration is sent, so its reply can be told apart from a
/// stale one: replies to an older registration, or that lost a race with a
/// newer delivery, are dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegistrationTicket {
    seq: u64,
    watermark: Option<LayoutRevision>,
}

/// Holds the layout the controller last delivered and applies it to the
/// `spur/` cgroup root.
pub struct SpecEnforcer {
    root: PathBuf,
    /// Present while spurd has controls on the root, so a restart resets only
    /// what spurd itself wrote, never limits an operator set.
    marker: Option<PathBuf>,
    cgroup_enabled: bool,
    held: Option<CoreLayout>,
    /// Newest revision seen, kept across a clear so a delayed launch from before
    /// it cannot reinstall the old layout.
    watermark: Option<LayoutRevision>,
    max_tight: bool,
    active: bool,
    registrations: u64,
    /// Whether a delivery has said what this node's policy is. Until then, and
    /// while it cannot be computed, the root is left exactly as found.
    policy_known: bool,
    layout_unresolved: bool,
    apply_error: Option<String>,
    /// The held layout changed since the last successful apply.
    unapplied: bool,
    /// Last value written per control file; unchanged values are not rewritten,
    /// since lowering `memory.high` reclaims synchronously in the writer.
    written: HashMap<&'static str, String>,
}

impl SpecEnforcer {
    pub fn new(root: PathBuf, marker: Option<PathBuf>, cgroup_enabled: bool) -> Self {
        SpecEnforcer {
            root,
            marker,
            cgroup_enabled,
            held: None,
            watermark: None,
            max_tight: false,
            active: false,
            registrations: 0,
            policy_known: false,
            layout_unresolved: false,
            apply_error: None,
            unapplied: false,
            written: HashMap::new(),
        }
    }

    /// The held layout's fingerprint, 0 when none is held.
    pub fn held_fingerprint(&self) -> u64 {
        self.held.as_ref().map(CoreLayout::fingerprint).unwrap_or(0)
    }

    /// Why the node is not enforcing its layout, if it is not.
    pub fn enforcement_error(&self) -> Option<String> {
        if self.layout_unresolved {
            return Some(
                "the controller could not compute this node's layout; root limits are kept as found"
                    .into(),
            );
        }
        if self.has_spec_cores() && !self.cgroup_enabled {
            return Some(
                "[cgroup] is disabled on this node, so jobs would not join the enforced root"
                    .into(),
            );
        }
        self.apply_error.clone()
    }

    /// Whether the node may start workloads: its layout is written to the root.
    pub fn enforcement_healthy(&self) -> bool {
        !self.unapplied && self.enforcement_error().is_none()
    }

    pub fn needs_apply(&self) -> bool {
        self.unapplied || self.apply_error.is_some()
    }

    pub fn has_spec_cores(&self) -> bool {
        self.held.as_ref().is_some_and(|l| !l.spec_cpus.is_empty())
    }

    pub fn spec_cpus(&self) -> Vec<u32> {
        self.held
            .as_ref()
            .map(|l| l.spec_cpus.clone())
            .unwrap_or_default()
    }

    /// Start a registration; pass the ticket back with its reply.
    pub fn begin_registration(&mut self) -> RegistrationTicket {
        self.registrations += 1;
        RegistrationTicket {
            seq: self.registrations,
            watermark: self.watermark,
        }
    }

    /// Adopt a delivered layout; returns true when the held content changed.
    /// A launch older than the watermark is ignored. A registration reply may
    /// move the watermark down (the controller's per-node sequence restarted),
    /// but only if no newer delivery landed while it was in flight.
    pub fn adopt(
        &mut self,
        incoming: Option<CoreLayout>,
        revision: LayoutRevision,
        delivery: Delivery,
    ) -> bool {
        let older = self.watermark.is_some_and(|seen| revision < seen);
        let layout_error = match delivery {
            Delivery::Launch if older => return false,
            Delivery::Launch => false,
            Delivery::Registration { ticket, .. } if ticket.seq != self.registrations => {
                return false
            }
            Delivery::Registration { ticket, .. }
                if older && ticket.watermark != self.watermark =>
            {
                return false
            }
            Delivery::Registration { layout_error, .. } => layout_error,
        };
        self.watermark = Some(revision);
        let may_clear = matches!(delivery, Delivery::Registration { .. }) && !layout_error;
        if incoming.is_some() || may_clear {
            self.policy_known = true;
            self.layout_unresolved = false;
        } else if layout_error && self.held.is_none() {
            self.layout_unresolved = true;
        }
        if incoming.is_none() && !may_clear {
            return false;
        }
        let fingerprint = |l: &Option<CoreLayout>| l.as_ref().map(CoreLayout::fingerprint);
        if fingerprint(&incoming) == fingerprint(&self.held) {
            return false;
        }
        self.held = incoming;
        self.max_tight = false;
        self.unapplied = true;
        true
    }

    /// Apply the held layout to the root. `live_job_cpusets` are the live jobs'
    /// `cpuset.cpus`; `total_mem_mb` the node's memory, `None` when unreadable.
    pub fn apply(
        &mut self,
        live_job_cpusets: &[Vec<u32>],
        total_mem_mb: Option<u64>,
    ) -> io::Result<MemReservation> {
        let result = match self.held.clone() {
            Some(layout) if !layout.spec_cpus.is_empty() => {
                self.enforce(&layout, live_job_cpusets, total_mem_mb)
            }
            // Never reset a root before the policy is known: jobs may survive
            // this agent's restart under limits it would otherwise drop.
            _ if !self.policy_known => Ok(MemReservation::Inactive),
            _ => self.release_root().map(|()| MemReservation::Inactive),
        };
        match &result {
            Ok(_) => {
                self.apply_error = None;
                self.unapplied = false;
            }
            Err(e) if e.kind() == io::ErrorKind::InvalidInput => {
                self.apply_error = Some(e.to_string())
            }
            Err(e) => self.apply_error = Some(format!("cannot write the spur cgroup root: {e}")),
        }
        result
    }

    fn enforce(
        &mut self,
        layout: &CoreLayout,
        live_job_cpusets: &[Vec<u32>],
        total_mem_mb: Option<u64>,
    ) -> io::Result<MemReservation> {
        // The root is otherwise created lazily by the first job.
        std::fs::create_dir_all(&self.root)?;
        self.mark_active()?;
        let effective = reconcile_root_cpuset(&root_cpuset(layout), live_job_cpusets);
        self.write("cpuset.cpus", format_cpu_list(&effective))?;

        if layout.mem_spec_mb == 0 {
            self.write("memory.high", "max".into())?;
            self.write("memory.max", "max".into())?;
            return Ok(MemReservation::Inactive);
        }
        let Some(total_mem_mb) = total_mem_mb else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot read the node's memory to size the reservation",
            ));
        };
        let target = total_mem_mb.saturating_sub(layout.mem_spec_mb);
        if target == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "reserved memory {} MB leaves nothing of {total_mem_mb} MB for jobs",
                    layout.mem_spec_mb
                ),
            ));
        }
        self.write("memory.high", mb_to_bytes(target))?;
        let decision = mem_decision(
            target,
            root_is_idle(&self.root),
            read_limit_mb(&self.root.join("memory.max")),
            self.max_tight,
        );
        if let Some(max) = decision.memory_max_mb {
            self.write("memory.max", mb_to_bytes(max))?;
            self.max_tight = true;
        }
        Ok(decision.state)
    }

    /// The CPUs spurd's threads should be pinned to, or `None` to widen back.
    pub fn daemon_cpuset(&self) -> Option<Vec<u32>> {
        self.held.as_ref().and_then(daemon_cpuset)
    }

    fn mark_active(&mut self) -> io::Result<()> {
        if let Some(marker) = &self.marker {
            if !self.active {
                if let Some(dir) = marker.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(marker, "")?;
            }
        }
        self.active = true;
        Ok(())
    }

    /// Undo spurd's root controls once it no longer holds spec cores, including
    /// ones a previous spurd left behind. A cpuset is cleared with a newline:
    /// a zero-byte write does not reset it.
    fn release_root(&mut self) -> io::Result<()> {
        let left_by_earlier_run = self.marker.as_deref().is_some_and(Path::exists);
        if !self.active && !left_by_earlier_run {
            return Ok(());
        }
        // A reboot removes the root but not the marker in persistent state.
        if self.root.exists() {
            write_cgroup(&self.root, "cpuset.cpus", "\n")?;
            write_cgroup(&self.root, "memory.high", "max")?;
            write_cgroup(&self.root, "memory.max", "max")?;
        }
        self.written.clear();
        if let Some(marker) = &self.marker {
            match std::fs::remove_file(marker) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        self.active = false;
        Ok(())
    }

    fn write(&mut self, file: &'static str, value: String) -> io::Result<()> {
        if self.written.get(file) == Some(&value) {
            return Ok(());
        }
        write_cgroup(&self.root, file, &value)?;
        self.written.insert(file, value);
        Ok(())
    }
}

/// Whether no process runs anywhere under the root (`populated 0`). Unreadable
/// counts as busy.
fn root_is_idle(root: &Path) -> bool {
    std::fs::read_to_string(root.join("cgroup.events"))
        .is_ok_and(|events| events.lines().any(|l| l.trim() == "populated 0"))
}

/// A cgroup limit in MB, rounded down; `None` for `max` or an unreadable file.
fn read_limit_mb(path: &Path) -> Option<u64> {
    let raw = std::fs::read_to_string(path).ok()?;
    raw.trim().parse::<u64>().ok().map(|b| b / (1024 * 1024))
}

fn mb_to_bytes(mb: u64) -> String {
    (mb.saturating_mul(1024 * 1024)).to_string()
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
    fn mem_holds_the_hard_limit_while_jobs_run() {
        let d = mem_decision(1000, false, None, false);
        assert_eq!(d.memory_high_mb, 1000);
        assert_eq!(d.memory_max_mb, None);
        assert_eq!(d.state, MemReservation::Pending);
    }

    #[test]
    fn mem_lowers_the_hard_limit_only_when_the_root_is_idle() {
        let d = mem_decision(1000, true, None, false);
        assert_eq!(d.memory_max_mb, Some(1000));
        assert_eq!(d.state, MemReservation::Active);
    }

    #[test]
    fn mem_stays_active_once_tight_even_when_busy() {
        let d = mem_decision(1000, false, None, true);
        assert_eq!(d.memory_max_mb, Some(1000));
        assert_eq!(d.state, MemReservation::Active);
    }

    #[test]
    fn mem_is_active_at_once_when_the_root_max_already_holds_the_target() {
        // A restarted spurd finds the limit its predecessor set.
        let d = mem_decision(1000, false, Some(1000), false);
        assert_eq!(d.memory_max_mb, Some(1000));
        assert_eq!(d.state, MemReservation::Active);
        // A tighter leftover limit is raised to the target, not to `max`.
        let d = mem_decision(1000, false, Some(600), false);
        assert_eq!(d.memory_max_mb, Some(1000));
    }

    #[test]
    fn daemon_cpuset_none_unless_off_spec_and_spec_present() {
        let mut l = layout(&[127], &[0, 1], &[], 0);
        assert_eq!(daemon_cpuset(&l), None);
        l.daemons_off_spec = true;
        assert_eq!(daemon_cpuset(&l), Some(vec![0, 1]));
        l.spec_cpus.clear();
        assert_eq!(daemon_cpuset(&l), None);
    }

    fn rev(term: u64, boot: u64, seq: u64) -> LayoutRevision {
        LayoutRevision { term, boot, seq }
    }

    fn register(
        e: &mut SpecEnforcer,
        incoming: Option<CoreLayout>,
        revision: LayoutRevision,
    ) -> bool {
        let ticket = e.begin_registration();
        e.adopt(
            incoming,
            revision,
            Delivery::Registration {
                layout_error: false,
                ticket,
            },
        )
    }

    fn register_errored(
        e: &mut SpecEnforcer,
        incoming: Option<CoreLayout>,
        revision: LayoutRevision,
    ) -> bool {
        let ticket = e.begin_registration();
        e.adopt(
            incoming,
            revision,
            Delivery::Registration {
                layout_error: true,
                ticket,
            },
        )
    }

    fn enforcer(root: &Path) -> SpecEnforcer {
        SpecEnforcer::new(root.to_path_buf(), Some(root.join("marker")), true)
    }

    fn unused() -> SpecEnforcer {
        SpecEnforcer::new(PathBuf::from("/unused"), None, true)
    }

    #[test]
    fn adopt_holds_and_reports_fingerprint() {
        let mut e = unused();
        assert_eq!(e.held_fingerprint(), 0);
        let l = layout(&[127], &[0, 1], &[], 1024);
        assert!(e.adopt(Some(l.clone()), rev(1, 1, 5), Delivery::Launch));
        assert_eq!(e.held_fingerprint(), l.fingerprint());
    }

    #[test]
    fn a_launch_older_than_the_watermark_is_ignored() {
        let mut e = unused();
        let current = layout(&[127], &[0, 1], &[], 1024);
        let other = layout(&[63], &[0, 2], &[], 1024);
        register(&mut e, Some(current.clone()), rev(3, 2, 1));

        assert!(!e.adopt(Some(other.clone()), rev(3, 1, 9), Delivery::Launch));
        assert_eq!(e.held_fingerprint(), current.fingerprint());
        assert!(e.adopt(Some(other.clone()), rev(3, 2, 2), Delivery::Launch));
        assert_eq!(e.held_fingerprint(), other.fingerprint());
    }

    #[test]
    fn a_registration_after_a_sequence_reset_is_accepted() {
        // The controller's per-node sequence restarts when a node is re-added.
        let mut e = unused();
        register(&mut e, Some(layout(&[127], &[0, 1], &[], 0)), rev(1, 1, 7));
        let fresh = layout(&[63], &[0, 2], &[], 0);
        assert!(register(&mut e, Some(fresh.clone()), rev(1, 1, 1)));
        assert_eq!(e.held_fingerprint(), fresh.fingerprint());
        // The watermark moved down with it, so a launch at the new revision lands.
        assert!(!e.adopt(Some(fresh), rev(1, 1, 1), Delivery::Launch));
    }

    #[test]
    fn a_registration_reply_older_than_a_launch_that_landed_in_flight_is_dropped() {
        let mut e = unused();
        register(&mut e, Some(layout(&[127], &[0, 1], &[], 0)), rev(1, 1, 1));
        let ticket = e.begin_registration();
        let newer = layout(&[63], &[0, 2], &[], 0);
        assert!(e.adopt(Some(newer.clone()), rev(1, 1, 2), Delivery::Launch));

        let stale = Delivery::Registration {
            layout_error: false,
            ticket,
        };
        assert!(!e.adopt(Some(layout(&[127], &[0, 1], &[], 0)), rev(1, 1, 1), stale));
        assert_eq!(e.held_fingerprint(), newer.fingerprint());
    }

    #[test]
    fn a_reply_to_a_superseded_registration_is_dropped() {
        let mut e = unused();
        let first = e.begin_registration();
        let current = layout(&[63], &[0, 2], &[], 0);
        assert!(register(&mut e, Some(current.clone()), rev(1, 1, 2)));

        let late = Delivery::Registration {
            layout_error: false,
            ticket: first,
        };
        assert!(!e.adopt(None, rev(1, 1, 3), late));
        assert_eq!(e.held_fingerprint(), current.fingerprint());
    }

    #[test]
    fn a_clear_keeps_its_watermark_against_a_delayed_launch() {
        let mut e = unused();
        let old = layout(&[127], &[0, 1], &[], 1024);
        register(&mut e, Some(old.clone()), rev(1, 1, 1));
        assert!(register(&mut e, None, rev(1, 1, 2)));
        assert_eq!(e.held_fingerprint(), 0);

        assert!(!e.adopt(Some(old), rev(1, 1, 1), Delivery::Launch));
        assert_eq!(e.held_fingerprint(), 0);
    }

    #[test]
    fn only_an_error_free_registration_clears_the_layout() {
        let mut e = unused();
        let l = layout(&[127], &[0, 1], &[], 1024);
        register(&mut e, Some(l.clone()), rev(1, 1, 1));

        assert!(!e.adopt(None, rev(1, 1, 2), Delivery::Launch));
        assert!(!register_errored(&mut e, None, rev(1, 1, 3)));
        assert_eq!(e.held_fingerprint(), l.fingerprint());
        assert!(register(&mut e, None, rev(1, 1, 4)));
    }

    #[test]
    fn identical_content_still_advances_the_watermark() {
        let mut e = unused();
        let a = layout(&[127], &[0, 1], &[], 1024);
        register(&mut e, Some(a.clone()), rev(1, 1, 1));
        assert!(!e.adopt(Some(a.clone()), rev(1, 1, 5), Delivery::Launch));

        let b = layout(&[63], &[0, 2], &[], 1024);
        assert!(!e.adopt(Some(b), rev(1, 1, 3), Delivery::Launch));
        assert_eq!(e.held_fingerprint(), a.fingerprint());
    }

    fn read(root: &Path, file: &str) -> Option<String> {
        std::fs::read_to_string(root.join(file)).ok()
    }

    fn set_populated(root: &Path, populated: bool) {
        let events = format!("populated {}\nfrozen 0\n", u8::from(populated));
        std::fs::write(root.join("cgroup.events"), events).unwrap();
    }

    #[test]
    fn apply_throttles_at_once_and_lowers_max_only_when_no_job_runs() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = enforcer(dir.path());
        register(
            &mut e,
            Some(layout(&[127, 255], &[0, 1, 2], &[], 1024)),
            rev(1, 1, 1),
        );
        set_populated(dir.path(), true);

        for _ in 0..3 {
            assert_eq!(e.apply(&[], Some(8192)).unwrap(), MemReservation::Pending);
        }
        assert_eq!(read(dir.path(), "cpuset.cpus").as_deref(), Some("0-2"));
        assert_eq!(
            read(dir.path(), "memory.high").as_deref(),
            Some(mb_to_bytes(7168).as_str())
        );
        assert_eq!(
            read(dir.path(), "memory.max"),
            None,
            "busy: max is left alone"
        );

        set_populated(dir.path(), false);
        assert_eq!(e.apply(&[], Some(8192)).unwrap(), MemReservation::Active);
        assert_eq!(
            read(dir.path(), "memory.max").as_deref(),
            Some(mb_to_bytes(7168).as_str())
        );

        set_populated(dir.path(), true);
        assert_eq!(e.apply(&[], Some(8192)).unwrap(), MemReservation::Active);
    }

    #[test]
    fn an_unreadable_populated_state_counts_as_busy() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = enforcer(dir.path());
        register(
            &mut e,
            Some(layout(&[127], &[0, 1], &[], 1024)),
            rev(1, 1, 1),
        );
        assert_eq!(e.apply(&[], Some(8192)).unwrap(), MemReservation::Pending);
        assert_eq!(read(dir.path(), "memory.max"), None);
    }

    #[test]
    fn an_errored_startup_registration_keeps_the_root_and_refuses_launches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cpuset.cpus"), "0-1").unwrap();
        std::fs::write(dir.path().join("memory.max"), mb_to_bytes(7168)).unwrap();
        std::fs::write(dir.path().join("marker"), "").unwrap();
        let mut e = enforcer(dir.path());

        register_errored(&mut e, None, rev(2, 1, 0));
        e.apply(&[], Some(8192)).unwrap();
        assert_eq!(read(dir.path(), "cpuset.cpus").as_deref(), Some("0-1"));
        assert_eq!(
            read(dir.path(), "memory.max").as_deref(),
            Some(mb_to_bytes(7168).as_str())
        );
        assert!(!e.enforcement_healthy());
        assert!(e.enforcement_error().unwrap().contains("could not compute"));

        // A valid policy resolves it.
        register(&mut e, Some(layout(&[127], &[0, 1], &[], 0)), rev(2, 1, 1));
        e.apply(&[], Some(8192)).unwrap();
        assert!(e.enforcement_healthy());
    }

    #[test]
    fn the_root_is_left_alone_until_the_policy_is_known() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cpuset.cpus"), "0-1").unwrap();
        std::fs::write(dir.path().join("marker"), "").unwrap();
        let mut e = enforcer(dir.path());
        e.apply(&[], Some(8192)).unwrap();
        assert_eq!(read(dir.path(), "cpuset.cpus").as_deref(), Some("0-1"));
        assert!(dir.path().join("marker").exists());
    }

    #[test]
    fn an_absent_root_after_a_reboot_counts_as_released() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker");
        std::fs::write(&marker, "").unwrap();
        let mut e = SpecEnforcer::new(dir.path().join("gone"), Some(marker.clone()), true);
        register(&mut e, None, rev(1, 1, 1));

        assert_eq!(e.apply(&[], Some(8192)).unwrap(), MemReservation::Inactive);
        assert!(!marker.exists());
        assert!(e.enforcement_healthy());
    }

    #[test]
    fn a_restarted_agent_never_loosens_a_max_already_at_target() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("memory.max"), mb_to_bytes(7168)).unwrap();
        let mut e = enforcer(dir.path());
        register(
            &mut e,
            Some(layout(&[127], &[0, 1], &[], 1024)),
            rev(1, 2, 1),
        );

        // Usage sits over target, as it does under a live throttle.
        let state = e.apply(&[], Some(8192)).unwrap();
        assert_eq!(state, MemReservation::Active);
        assert_eq!(
            read(dir.path(), "memory.max").as_deref(),
            Some(mb_to_bytes(7168).as_str())
        );
    }

    #[test]
    fn a_reservation_covering_all_memory_is_refused_not_written_as_zero() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = enforcer(dir.path());
        register(
            &mut e,
            Some(layout(&[127], &[0, 1], &[], 8192)),
            rev(1, 1, 1),
        );
        assert!(e.apply(&[], Some(8192)).is_err());
        assert!(!e.enforcement_healthy());
        assert_eq!(read(dir.path(), "memory.high"), None);
        assert_eq!(read(dir.path(), "memory.max"), None);
    }

    #[test]
    fn without_a_reservation_memory_stays_unlimited_whatever_the_total() {
        for total in [Some(8192), None, Some(0)] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("memory.max"), mb_to_bytes(4096)).unwrap();
            let mut e = enforcer(dir.path());
            register(&mut e, Some(layout(&[127], &[0, 1], &[], 0)), rev(1, 1, 1));
            assert_eq!(e.apply(&[], total).unwrap(), MemReservation::Inactive);
            assert_eq!(read(dir.path(), "memory.high").as_deref(), Some("max"));
            assert_eq!(read(dir.path(), "memory.max").as_deref(), Some("max"));
            assert!(e.enforcement_healthy());
        }
    }

    #[test]
    fn an_unreadable_total_with_a_reservation_writes_no_memory_and_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = enforcer(dir.path());
        register(
            &mut e,
            Some(layout(&[127], &[0, 1], &[], 1024)),
            rev(1, 1, 1),
        );
        assert!(e.apply(&[], None).is_err());
        assert_eq!(read(dir.path(), "cpuset.cpus").as_deref(), Some("0-1"));
        assert_eq!(read(dir.path(), "memory.high"), None);
        assert_eq!(read(dir.path(), "memory.max"), None);
        assert!(e
            .enforcement_error()
            .unwrap()
            .contains("cannot read the node's memory"));
    }

    #[test]
    fn unchanged_values_are_not_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = enforcer(dir.path());
        register(
            &mut e,
            Some(layout(&[127], &[0, 1], &[], 1024)),
            rev(1, 1, 1),
        );
        e.apply(&[], Some(8192)).unwrap();
        std::fs::write(dir.path().join("memory.high"), "sentinel").unwrap();
        e.apply(&[], Some(8192)).unwrap();
        assert_eq!(read(dir.path(), "memory.high").as_deref(), Some("sentinel"));
    }

    #[test]
    fn apply_defers_a_cpu_a_live_job_sits_on() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = enforcer(dir.path());
        register(&mut e, Some(layout(&[2, 3], &[0, 1], &[], 0)), rev(1, 1, 1));
        e.apply(&[vec![2, 3]], Some(4096)).unwrap();
        assert_eq!(read(dir.path(), "cpuset.cpus").as_deref(), Some("0-3"));
    }

    #[test]
    fn apply_resets_the_root_when_spec_is_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = enforcer(dir.path());
        register(
            &mut e,
            Some(layout(&[127], &[0, 1], &[], 1024)),
            rev(1, 1, 1),
        );
        e.apply(&[], Some(8192)).unwrap();
        assert!(dir.path().join("marker").exists());

        register(&mut e, None, rev(1, 1, 2));
        e.apply(&[], Some(8192)).unwrap();
        assert_eq!(read(dir.path(), "cpuset.cpus").as_deref(), Some("\n"));
        assert_eq!(read(dir.path(), "memory.max").as_deref(), Some("max"));
        assert!(!dir.path().join("marker").exists());
    }

    #[test]
    fn a_restarted_agent_resets_controls_its_predecessor_left() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cpuset.cpus"), "0,1").unwrap();
        std::fs::write(dir.path().join("memory.max"), "123456").unwrap();
        std::fs::write(dir.path().join("marker"), "").unwrap();

        let mut e = enforcer(dir.path());
        register(&mut e, None, rev(1, 1, 1));
        assert_eq!(e.apply(&[], Some(8192)).unwrap(), MemReservation::Inactive);
        assert_eq!(read(dir.path(), "cpuset.cpus").as_deref(), Some("\n"));
        assert_eq!(read(dir.path(), "memory.max").as_deref(), Some("max"));
    }

    #[test]
    fn operator_set_root_limits_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cpuset.cpus"), "0,1").unwrap();
        std::fs::write(dir.path().join("memory.max"), "123456").unwrap();

        let mut e = enforcer(dir.path());
        register(&mut e, None, rev(1, 1, 1));
        e.apply(&[], Some(8192)).unwrap();
        assert_eq!(read(dir.path(), "cpuset.cpus").as_deref(), Some("0,1"));
        assert_eq!(read(dir.path(), "memory.max").as_deref(), Some("123456"));
        assert!(e.enforcement_healthy());
    }

    #[test]
    fn an_adopted_layout_is_not_enforced_until_applied() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = enforcer(dir.path());
        e.adopt(
            Some(layout(&[127], &[0, 1], &[], 1024)),
            rev(1, 1, 1),
            Delivery::Launch,
        );
        assert!(e.needs_apply());
        assert!(!e.enforcement_healthy());

        e.apply(&[], Some(8192)).unwrap();
        assert!(!e.needs_apply());
        assert!(e.enforcement_healthy());
        assert_eq!(read(dir.path(), "cpuset.cpus").as_deref(), Some("0-1"));
    }

    #[test]
    fn spec_cores_with_cgroups_disabled_report_an_enforcement_error() {
        let mut e = SpecEnforcer::new(PathBuf::from("/unused"), None, false);
        assert_eq!(e.enforcement_error(), None);
        register(&mut e, Some(layout(&[127], &[0, 1], &[], 0)), rev(1, 1, 1));
        assert!(e.enforcement_error().unwrap().contains("[cgroup]"));
    }

    #[test]
    fn apply_reports_an_error_when_the_root_write_fails() {
        // A regular file as an ancestor fails with ENOTDIR even for root.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, "").unwrap();
        let mut e = SpecEnforcer::new(file.join("spur-root"), None, true);
        register(
            &mut e,
            Some(layout(&[127], &[0, 1], &[], 1024)),
            rev(1, 1, 1),
        );
        assert!(e.apply(&[], Some(8192)).is_err());
        assert!(!e.enforcement_healthy());
        assert!(e.enforcement_error().unwrap().contains("spur cgroup root"));
    }
}
