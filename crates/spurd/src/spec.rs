// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Node-level core and memory specialization: turns the layout the controller
//! delivers into writes on the `spur/` cgroup root, which every job runs under.

use std::collections::{BTreeSet, HashMap};
use std::io;
use std::path::{Path, PathBuf};

use spur_core::core_layout::{CoreLayout, LayoutRevision};
use spur_core::cpu_topology::format_cpu_list;
use tracing::info;

/// The CPUs the `spur/` root is confined to: every non-spec CPU, sorted.
pub fn root_cpuset(layout: &CoreLayout) -> Vec<u32> {
    let mut cpus: BTreeSet<u32> = layout.general_cpus.iter().copied().collect();
    for gpu in &layout.gpu_reserved {
        cpus.extend(gpu.cpus.iter().copied());
    }
    cpus.into_iter().collect()
}

/// A job cgroup under the `spur/` root (see crate AGENTS.md "No narrowing"
/// and "Root ownership" for how `populated` governs the memory floor).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveJob {
    pub dir: String,
    pub cpus: Vec<u32>,
    pub populated: bool,
}

/// The root cpuset to write: `desired` plus every CPU a live job's cpuset names
/// (populated or not), so a layout change never narrows a running job.
pub fn reconcile_root_cpuset(desired: &[u32], live_jobs: &[LiveJob]) -> Vec<u32> {
    let mut keep: BTreeSet<u32> = desired.iter().copied().collect();
    for job in live_jobs {
        keep.extend(&job.cpus);
    }
    keep.into_iter().collect()
}

/// Whether the node's reserved memory is being enforced yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemReservation {
    /// No spec memory target (unset or reset).
    Inactive,
    /// `memory.high` throttles toward the target; `memory.max` waits for the
    /// jobs that ran when the layout last changed to end.
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

/// `memory.high` (throttle only, never OOM) goes to the target. `memory.max`
/// is lowered only once no job from before the last layout change runs
/// (`quiescent`), since a hard limit below its usage can OOM it, or when the
/// root's current `memory.max` already holds it at or below the target.
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
/// CPUs a cap trims are host CPUs, not spec cores, so daemons may use them.
pub fn daemon_cpuset(layout: &CoreLayout) -> Option<Vec<u32>> {
    if !layout.daemons_off_spec || layout.spec_cpus.is_empty() {
        return None;
    }
    let mut cpus = root_cpuset(layout);
    cpus.extend(&layout.trimmed_cpus);
    cpus.sort_unstable();
    cpus.dedup();
    Some(cpus)
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

/// Spec and cap-trimmed CPUs no job may take, and each GPU's reserved CPUs,
/// which only the job holding that GPU may take. Re-applied to the ledger
/// whenever it is rebuilt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LedgerExclusions {
    pub spec_cpus: Vec<u32>,
    pub trimmed_cpus: Vec<u32>,
    pub gpu_reserved: Vec<spur_core::core_layout::GpuReservedCpus>,
}

impl LedgerExclusions {
    pub fn apply(&self, ledger: &mut spur_sched::cons_tres::NodeAllocation) {
        let mut excluded = self.spec_cpus.clone();
        excluded.extend(&self.trimmed_cpus);
        ledger.set_spec_exclusions(&excluded);
        ledger.set_gpu_reserved(&self.gpu_reserved);
    }
}

/// The root's `memory.high` when the held layout last changed (`None` is
/// `max`), held while a job running then is alive: it was admitted against it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MemFloor {
    high_mb: Option<u64>,
    jobs: BTreeSet<String>,
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
    /// The next enforce takes a memory floor: set at start and by a
    /// content-changing adopt, so jobs from before either keep their limits.
    floor_pending: bool,
    mem_floor: Option<MemFloor>,
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
            floor_pending: true,
            mem_floor: None,
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

    /// What the held layout withholds from the job ledger's free pick.
    pub fn ledger_exclusions(&self) -> LedgerExclusions {
        self.held
            .as_ref()
            .map(|l| LedgerExclusions {
                spec_cpus: l.spec_cpus.clone(),
                trimmed_cpus: l.trimmed_cpus.clone(),
                gpu_reserved: l.gpu_reserved.clone(),
            })
            .unwrap_or_default()
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
        self.floor_pending = true;
        true
    }

    /// Apply the held layout to the root. `live_jobs` are the jobs running
    /// under the root; `total_mem_mb` the node's memory, `None` when unreadable.
    pub fn apply(
        &mut self,
        live_jobs: &[LiveJob],
        total_mem_mb: Option<u64>,
    ) -> io::Result<MemReservation> {
        let result = match self.held.clone() {
            Some(layout) if !layout.spec_cpus.is_empty() || !layout.trimmed_cpus.is_empty() => {
                self.enforce(&layout, live_jobs, total_mem_mb)
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
        live_jobs: &[LiveJob],
        total_mem_mb: Option<u64>,
    ) -> io::Result<MemReservation> {
        // The root is otherwise created lazily by the first job.
        std::fs::create_dir_all(&self.root)?;
        self.mark_active()?;
        self.capture_floor(live_jobs);
        let effective = reconcile_root_cpuset(&root_cpuset(layout), live_jobs);
        self.write("cpuset.cpus", format_cpu_list(&effective))?;

        // mem_spec_mb without spec cores is scheduling-only (a trimmed-only
        // layout never gets a root memory limit from reserved_memory_mb).
        if layout.spec_cpus.is_empty() || layout.mem_spec_mb == 0 {
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
        let held_high = self.held_memory_high(target, live_jobs);
        let floor_holds = held_high.is_some();
        self.write(
            "memory.high",
            held_high.unwrap_or_else(|| mb_to_bytes(target)),
        )?;
        let decision = mem_decision(
            target,
            !floor_holds,
            read_limit_mb(&self.root.join("memory.max")),
            self.max_tight,
        );
        if let Some(max) = decision.memory_max_mb {
            self.write("memory.max", mb_to_bytes(max))?;
            self.max_tight = true;
        }
        Ok(decision.state)
    }

    /// Take the floor from the root as it is now, once per change. An
    /// unpopulated (orphaned) job dir never enters the floor's job set.
    fn capture_floor(&mut self, live_jobs: &[LiveJob]) {
        if !std::mem::take(&mut self.floor_pending) {
            return;
        }
        let jobs: BTreeSet<String> = live_jobs
            .iter()
            .filter(|j| j.populated)
            .map(|j| j.dir.clone())
            .collect();
        self.mem_floor = (!jobs.is_empty()).then(|| MemFloor {
            high_mb: read_limit_mb(&self.root.join("memory.high")),
            jobs,
        });
    }

    /// `memory.high` to keep while a job from before the last change runs: the
    /// value then, or the target when that is higher. The floor is dropped
    /// for good once none of its jobs is still populated.
    fn held_memory_high(&mut self, target_mb: u64, live_jobs: &[LiveJob]) -> Option<String> {
        let alive = self.mem_floor.as_ref().is_some_and(|f| {
            live_jobs
                .iter()
                .any(|j| j.populated && f.jobs.contains(&j.dir))
        });
        if !alive {
            self.mem_floor = None;
            return None;
        }
        let floor = self.mem_floor.as_ref()?;
        Some(match floor.high_mb {
            Some(mb) => mb_to_bytes(target_mb.max(mb)),
            None => "max".into(),
        })
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
        // The marker lives in this process's state_dir; the root is
        // host-persistent, so also release when a held layout's root is
        // itself narrowed, regardless of which run narrowed it.
        let has_marker = self.marker.as_deref().is_some_and(Path::exists);
        let narrowed = self.held.is_some() && self.root_narrowed();
        if !self.active && !has_marker && !narrowed {
            return Ok(());
        }
        // Only the narrowing-without-marker case is a surprise worth a log
        // line: the marker and `self.active` cases are spurd's own, expected
        // bookkeeping. This fires once: the root reads clean on the next call.
        if !self.active && !has_marker && narrowed {
            info!(
                cpuset_cpus = %read_cgroup(&self.root, "cpuset.cpus").unwrap_or_default(),
                memory_high = %read_cgroup(&self.root, "memory.high").unwrap_or_default(),
                memory_max = %read_cgroup(&self.root, "memory.max").unwrap_or_default(),
                "releasing a spur root narrowed by an earlier run",
            );
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

    /// Whether the root's on-disk cpuset or memory limits are narrower than
    /// a released root. A missing or unreadable file means nothing to release.
    fn root_narrowed(&self) -> bool {
        let cpuset_narrow = read_cgroup(&self.root, "cpuset.cpus").is_some_and(|s| !s.is_empty());
        let mem_narrow = ["memory.high", "memory.max"]
            .iter()
            .any(|file| read_cgroup(&self.root, file).is_some_and(|s| s != "max"));
        cpuset_narrow || mem_narrow
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

/// A cgroup control file's content, trimmed; `None` when missing or unreadable.
fn read_cgroup(root: &Path, file: &str) -> Option<String> {
    std::fs::read_to_string(root.join(file))
        .ok()
        .map(|s| s.trim().to_string())
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
            trimmed_cpus: vec![],
        }
    }

    fn job(dir: &str, cpus: &[u32]) -> LiveJob {
        LiveJob {
            dir: dir.into(),
            cpus: cpus.to_vec(),
            populated: true,
        }
    }

    fn unpopulated_job(dir: &str, cpus: &[u32]) -> LiveJob {
        LiveJob {
            dir: dir.into(),
            cpus: cpus.to_vec(),
            populated: false,
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
        let out = reconcile_root_cpuset(&[0, 1, 2, 3], &[job("job_1_1", &[0, 1])]);
        assert_eq!(out, vec![0, 1, 2, 3]);
    }

    #[test]
    fn reconcile_defers_a_cpu_a_live_job_still_sits_on() {
        // Spec just took CPU 4; a running job is pinned to {4,5}. Both are kept
        // until the job exits.
        let out = reconcile_root_cpuset(&[0, 1], &[job("job_1_1", &[4, 5])]);
        assert_eq!(out, vec![0, 1, 4, 5]);
    }

    #[test]
    fn reconcile_keeps_root_when_job_still_partly_intersects() {
        // Job on {1,4}: CPU 1 is still in the desired root, and CPU 4 stays
        // too, so the job is never narrowed.
        let out = reconcile_root_cpuset(&[0, 1], &[job("job_1_1", &[1, 4])]);
        assert_eq!(out, vec![0, 1, 4]);
    }

    #[test]
    fn reconcile_ignores_jobs_with_no_recorded_cpuset() {
        let out = reconcile_root_cpuset(&[0, 1], &[job("job_1_1", &[])]);
        assert_eq!(out, vec![0, 1]);
    }

    #[test]
    fn reconcile_keeps_every_cpu_a_live_job_names() {
        let live = [
            job("job_1_1", &[1, 5]),
            job("job_2_1", &[6]),
            job("job_3_1", &[]),
        ];
        assert_eq!(
            reconcile_root_cpuset(&[0, 1, 2], &live),
            vec![0, 1, 2, 5, 6]
        );
        assert_eq!(
            reconcile_root_cpuset(&[0, 1, 2], &[]),
            vec![0, 1, 2],
            "narrows once they end"
        );
    }

    #[test]
    fn reconcile_keeps_an_unpopulated_jobs_cpus_too() {
        // An orphan dir's cpus still widen the root: populated only gates
        // the memory floor, never the cpuset union.
        let out = reconcile_root_cpuset(&[0, 1], &[unpopulated_job("job_9_1", &[4, 5])]);
        assert_eq!(out, vec![0, 1, 4, 5]);
    }

    #[test]
    fn mem_holds_the_hard_limit_while_jobs_run() {
        let d = mem_decision(1000, false, None, false);
        assert_eq!(d.memory_high_mb, 1000);
        assert_eq!(d.memory_max_mb, None);
        assert_eq!(d.state, MemReservation::Pending);
    }

    #[test]
    fn mem_lowers_the_hard_limit_once_pre_change_jobs_end() {
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

    #[test]
    fn cap_trimmed_cpus_stay_out_of_the_job_root_but_not_off_the_daemons() {
        let mut l = layout(&[7], &[0, 1], &[(10, &[2, 3])], 0);
        l.trimmed_cpus = vec![4, 5, 6];
        assert_eq!(root_cpuset(&l), vec![0, 1, 2, 3]);
        l.daemons_off_spec = true;
        assert_eq!(daemon_cpuset(&l), Some(vec![0, 1, 2, 3, 4, 5, 6]));
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

    #[test]
    fn memory_high_is_held_while_pre_adoption_jobs_run() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = enforcer(dir.path());
        register(
            &mut e,
            Some(layout(&[127], &[0, 1], &[], 1024)),
            rev(1, 1, 1),
        );
        assert_eq!(e.apply(&[], Some(8192)).unwrap(), MemReservation::Active);
        let old = job("job_1_1", &[0]);
        e.apply(std::slice::from_ref(&old), Some(8192)).unwrap();

        register(
            &mut e,
            Some(layout(&[127], &[0, 1], &[], 2048)),
            rev(1, 1, 2),
        );
        let later = job("job_2_1", &[1]);
        for live in [vec![old.clone()], vec![old.clone(), later]] {
            assert_eq!(e.apply(&live, Some(8192)).unwrap(), MemReservation::Pending);
            assert_eq!(
                read(dir.path(), "memory.high").as_deref(),
                Some(mb_to_bytes(7168).as_str()),
                "held at the value job 1 was admitted against"
            );
            assert_eq!(
                read(dir.path(), "memory.max").as_deref(),
                Some(mb_to_bytes(7168).as_str())
            );
        }
    }

    #[test]
    fn memory_converges_when_pre_adoption_jobs_end() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = enforcer(dir.path());
        register(
            &mut e,
            Some(layout(&[127], &[0, 1], &[], 1024)),
            rev(1, 1, 1),
        );
        e.apply(&[], Some(8192)).unwrap();
        let old = job("job_1_1", &[0]);
        register(
            &mut e,
            Some(layout(&[127], &[0, 1], &[], 2048)),
            rev(1, 1, 2),
        );
        e.apply(&[old], Some(8192)).unwrap();

        let later = job("job_2_1", &[1]);
        assert_eq!(
            e.apply(std::slice::from_ref(&later), Some(8192)).unwrap(),
            MemReservation::Active,
            "a job started after the change was admitted against the new memory"
        );
        for file in ["memory.high", "memory.max"] {
            assert_eq!(
                read(dir.path(), file).as_deref(),
                Some(mb_to_bytes(6144).as_str()),
                "{file}"
            );
        }
        assert_eq!(
            e.apply(&[later], Some(8192)).unwrap(),
            MemReservation::Active
        );
    }

    #[test]
    fn an_unpopulated_job_never_captures_or_holds_the_floor() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = enforcer(dir.path());
        register(
            &mut e,
            Some(layout(&[127], &[0, 1], &[], 1024)),
            rev(1, 1, 1),
        );
        e.apply(&[], Some(8192)).unwrap();

        register(
            &mut e,
            Some(layout(&[127], &[0, 1], &[], 2048)),
            rev(1, 1, 2),
        );
        let orphan = unpopulated_job("job_1_1", &[0]);
        assert_eq!(
            e.apply(&[orphan], Some(8192)).unwrap(),
            MemReservation::Active,
            "an orphan (unpopulated, not ledger-held) never captures the floor"
        );
        for file in ["memory.high", "memory.max"] {
            assert_eq!(
                read(dir.path(), file).as_deref(),
                Some(mb_to_bytes(6144).as_str()),
                "{file} converges at once: no populated job to hold a floor"
            );
        }
    }

    #[test]
    fn raising_memory_high_applies_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = enforcer(dir.path());
        register(
            &mut e,
            Some(layout(&[127], &[0, 1], &[], 2048)),
            rev(1, 1, 1),
        );
        e.apply(&[], Some(8192)).unwrap();
        let old = job("job_1_1", &[0]);
        register(
            &mut e,
            Some(layout(&[127], &[0, 1], &[], 1024)),
            rev(1, 1, 2),
        );
        assert_eq!(e.apply(&[old], Some(8192)).unwrap(), MemReservation::Active);
        for file in ["memory.high", "memory.max"] {
            assert_eq!(
                read(dir.path(), file).as_deref(),
                Some(mb_to_bytes(7168).as_str()),
                "{file}"
            );
        }
    }

    #[test]
    fn a_restarted_agent_takes_its_floor_from_the_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("memory.high"), mb_to_bytes(7168)).unwrap();
        std::fs::write(dir.path().join("memory.max"), mb_to_bytes(7168)).unwrap();
        std::fs::write(dir.path().join("marker"), "").unwrap();
        let mut e = enforcer(dir.path());
        register(
            &mut e,
            Some(layout(&[127], &[0, 1], &[], 2048)),
            rev(1, 2, 1),
        );

        let survivor = job("job_5_1", &[0]);
        assert_eq!(
            e.apply(&[survivor], Some(8192)).unwrap(),
            MemReservation::Pending
        );
        for file in ["memory.high", "memory.max"] {
            assert_eq!(
                read(dir.path(), file).as_deref(),
                Some(mb_to_bytes(7168).as_str()),
                "{file} kept while the job that survived the restart runs"
            );
        }

        assert_eq!(e.apply(&[], Some(8192)).unwrap(), MemReservation::Active);
        for file in ["memory.high", "memory.max"] {
            assert_eq!(
                read(dir.path(), file).as_deref(),
                Some(mb_to_bytes(6144).as_str()),
                "{file}"
            );
        }
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
        e.apply(&[job("job_1_1", &[2, 3])], Some(4096)).unwrap();
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
    fn release_converges_a_stale_cpuset_even_without_the_marker() {
        // Simulates a reserved-cores-only layout landing on a root a
        // different state_dir (or spurd life) already narrowed: no marker
        // for this process, but the root itself shows the leftover.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cpuset.cpus"), "0-233").unwrap();
        let mut e = enforcer(dir.path());
        register(&mut e, Some(layout(&[], &[0, 1], &[], 0)), rev(1, 1, 1));
        e.apply(&[], Some(8192)).unwrap();
        assert_eq!(read(dir.path(), "cpuset.cpus").as_deref(), Some("\n"));
    }

    #[test]
    fn release_converges_a_stale_memory_max_even_without_the_marker() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("memory.max"), mb_to_bytes(4096)).unwrap();
        let mut e = enforcer(dir.path());
        register(&mut e, Some(layout(&[], &[0, 1], &[], 0)), rev(1, 1, 1));
        e.apply(&[], Some(8192)).unwrap();
        assert_eq!(read(dir.path(), "memory.max").as_deref(), Some("max"));
    }

    #[test]
    fn release_leaves_an_already_clean_root_untouched() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cpuset.cpus"), "").unwrap();
        std::fs::write(dir.path().join("memory.high"), "max").unwrap();
        std::fs::write(dir.path().join("memory.max"), "max").unwrap();
        let mut e = enforcer(dir.path());
        register(&mut e, Some(layout(&[], &[0, 1], &[], 0)), rev(1, 1, 1));
        e.apply(&[], Some(8192)).unwrap();
        assert_eq!(read(dir.path(), "cpuset.cpus").as_deref(), Some(""));
        assert_eq!(read(dir.path(), "memory.high").as_deref(), Some("max"));
        assert_eq!(read(dir.path(), "memory.max").as_deref(), Some("max"));
    }

    #[test]
    fn a_trimmed_only_layout_narrows_the_root_cpuset_but_leaves_memory_at_max() {
        // No spec cores, but a `cpus =` cap trimmed CPUs 4-6 out of jobs'
        // reach: the root cpuset must exclude them too, even though memory
        // enforcement (out of scope here) stays off.
        let dir = tempfile::tempdir().unwrap();
        let mut e = enforcer(dir.path());
        let mut l = layout(&[], &[0, 1], &[(10, &[2, 3])], 2048);
        l.trimmed_cpus = vec![4, 5, 6];
        register(&mut e, Some(l), rev(1, 1, 1));
        assert_eq!(e.apply(&[], Some(8192)).unwrap(), MemReservation::Inactive);
        assert_eq!(read(dir.path(), "cpuset.cpus").as_deref(), Some("0-3"));
        assert_eq!(read(dir.path(), "memory.high").as_deref(), Some("max"));
        assert_eq!(
            read(dir.path(), "memory.max").as_deref(),
            Some("max"),
            "mem_spec_mb without spec cores is scheduling-only, never a root limit"
        );
    }

    #[test]
    fn a_restricted_cores_only_layout_releases_a_root_narrowed_as_if_by_an_operator() {
        // Same on-disk state as operator_set_root_limits_are_left_alone, but
        // this node holds a restricted_cores_per_gpu-only layout (no spec
        // cores): it is spur-managed, so the root is released regardless,
        // unlike a node with no layout at all.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cpuset.cpus"), "0,1").unwrap();
        std::fs::write(dir.path().join("memory.max"), "123456").unwrap();
        let mut e = enforcer(dir.path());
        register(
            &mut e,
            Some(layout(&[], &[2, 3], &[(10, &[0, 1])], 0)),
            rev(1, 1, 1),
        );
        e.apply(&[], Some(8192)).unwrap();
        assert_eq!(read(dir.path(), "cpuset.cpus").as_deref(), Some("\n"));
        assert_eq!(read(dir.path(), "memory.max").as_deref(), Some("max"));
    }

    #[test]
    fn an_unknown_policy_leaves_a_narrowed_root_untouched_even_without_the_marker() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cpuset.cpus"), "0-233").unwrap();
        let mut e = enforcer(dir.path());
        e.apply(&[], Some(8192)).unwrap();
        assert_eq!(read(dir.path(), "cpuset.cpus").as_deref(), Some("0-233"));
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
