// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The node's entitlement ledger. `runtime/` answers "is it alive"; this answers
//! "what is it entitled to", and survives the supervisor that earned it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use spur_core::job::{LedgerDisposition, RunKey, LAUNCH_LIFETIME_MS};
use spur_core::step::StepId;
use spur_sched::cons_tres::ReleaseWarrant;

use crate::stepd::{create_private_dir_all, publish_private, verify_private_dir};

/// Bumped only for a change old readers cannot absorb; additive fields default.
pub const ADMISSION_SCHEMA_VERSION: u32 = 1;

const RUN_FILE: &str = "run.json";
const PARTICIPANTS_DIR: &str = "participants";

/// Records with no expiry of their own age out on this, measured from creation.
pub const DEFAULT_RETENTION_SECS: u64 = 3600;

/// The slice a run is entitled to. Device ids are node-local.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmittedResources {
    #[serde(default)]
    pub cpu_ids: Vec<u32>,
    #[serde(default)]
    pub memory_mb: u64,
    #[serde(default)]
    pub gpu_devices: Vec<u32>,
}

/// Two states, because every decision taken on one asks only whether teardown
/// has finished. The aliases keep records written before that was spelled out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    #[default]
    #[serde(alias = "running", alias = "cleaning")]
    Admitted,
    Cleaned,
}

/// Whether a run's slice may be handed back on the controller's word alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettlePermit {
    /// Teardown is finished and no hook is under it; the release keys to this step.
    Due(StepId),
    /// Nothing here to settle, so nothing to report back as released.
    NoRecord,
    /// A payload or a hook is still on the cores. The controller cannot see this.
    NotQuiescent,
}

/// A hook whose owner died mid-run loads as `Unknown` and is never re-run: the
/// agent cannot tell whether its side effects landed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookState {
    /// No hook is configured for this point. Distinct from `Pending`, which is
    /// what makes a finished teardown with a hook still owed say so.
    #[default]
    NotStarted,
    Pending,
    Running,
    Succeeded,
    Failed,
    Unknown,
}

impl HookState {
    /// What a hook recorded as still owed or still running means once the
    /// process that owed it is gone.
    pub fn settled_after_owner_loss(self) -> Self {
        match self {
            Self::Pending | Self::Running => Self::Unknown,
            other => other,
        }
    }

    /// Whether the hook may still be touching the run's resources. Holding for a
    /// failed or unknowable one strands the slice: nothing ever re-runs a hook.
    pub fn is_in_flight(self) -> bool {
        matches!(self, Self::Pending | Self::Running)
    }
}

/// Whether an epilog is still owed when a run's teardown finishes. The hook runs
/// after the record is marked cleaned, so the debt is written with the mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpilogOwed {
    Yes,
    No,
}

/// Kept as its own node so a record written before the phase field was dropped
/// still finds its epilog where it left it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupState {
    #[serde(default)]
    pub epilog: HookState,
}

/// Why evidence is being preserved instead of released. Set by the agent,
/// cleared only once the controller has reconciled the node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictHold {
    pub reason: String,
    #[serde(default)]
    pub observed_at_unix_ms: u64,
}

/// Whether a conflict hold was newly taken, so callers can log the transition
/// rather than every tick of the sweep that observes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldOutcome {
    Taken,
    AlreadyHeld,
    NoRecord,
    /// The run's slice is already released, so there is nothing left to preserve.
    AlreadyReleased,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControllerAck {
    #[serde(default)]
    pub release_raft_index: Option<u64>,
    /// Legacy: the controller answered a claim it had no record of. Retained
    /// for on-disk compatibility; new code ignores it and trusts only a real index.
    #[serde(default, skip_serializing)]
    pub settled_unrecorded_claim: bool,
}

impl ControllerAck {
    /// Whether the controller has committed this run's completion. Zero means
    /// no record (so no commit); `None` means no acknowledgement at all.
    pub fn is_committed(&self) -> bool {
        self.release_raft_index.is_some_and(|idx| idx > 0)
    }
}

/// `admission/<job>.<attempt>/run.json`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunAdmission {
    pub schema_version: u32,
    pub job_id: u32,
    pub run_attempt: u32,
    /// Verified against this agent's node on load; a foreign record is rejected
    /// rather than adopted, since its slice describes another node's hardware.
    pub node: String,
    #[serde(default)]
    pub allocation: AdmittedResources,
    #[serde(default)]
    pub state: RunState,
    /// Agent wall clock. The only age a run whose launch aborted before any
    /// expiry was set will ever have, so it is what makes such a run collectable.
    #[serde(default)]
    pub created_at_unix_ms: u64,
    #[serde(default)]
    pub reject_before_unix_ms: u64,
    #[serde(default)]
    pub max_launch_expiry_unix_ms: u64,
    #[serde(default)]
    pub lifecycle_owner_step: Option<StepId>,
    #[serde(default)]
    pub cleanup: CleanupState,
    #[serde(default)]
    pub conflict_hold: Option<ConflictHold>,
    #[serde(default)]
    pub controller_ack: ControllerAck,
    /// The controller asked for this run to end. Its own word that the run is
    /// over, which settles the record once teardown has finished.
    #[serde(default)]
    pub cancelled_by_controller: bool,
    /// This run's slice has gone back to the node on an acknowledged release.
    /// A record leaves the ledger cut only once this says it holds nothing.
    #[serde(default)]
    pub slice_released: bool,
}

impl RunAdmission {
    pub fn new(
        job_id: u32,
        run_attempt: u32,
        node: impl Into<String>,
        allocation: AdmittedResources,
        created_at_unix_ms: u64,
    ) -> Self {
        Self {
            schema_version: ADMISSION_SCHEMA_VERSION,
            job_id,
            run_attempt,
            node: node.into(),
            allocation,
            state: RunState::Admitted,
            created_at_unix_ms,
            reject_before_unix_ms: 0,
            max_launch_expiry_unix_ms: 0,
            lifecycle_owner_step: None,
            cleanup: CleanupState::default(),
            conflict_hold: None,
            controller_ack: ControllerAck::default(),
            cancelled_by_controller: false,
            slice_released: false,
        }
    }

    /// The run this record names, or `None` if it names attempt 0, which is no
    /// run at all.
    pub fn key(&self) -> Option<RunKey> {
        RunKey::new(self.job_id, self.run_attempt)
    }

    /// Whether this run's fate is already decided. What such a record still
    /// says about a command describes a dead predecessor, not a live run.
    pub fn is_over(&self) -> bool {
        self.state == RunState::Cleaned
            || self.cancelled_by_controller
            || self.controller_ack.release_raft_index.is_some()
    }

    /// When this run stops being interesting if nothing else ever happens to it.
    fn ages_out_at(&self, retention_ms: u64) -> u64 {
        self.max_launch_expiry_unix_ms
            .max(self.created_at_unix_ms.saturating_add(retention_ms))
    }

    /// Whether the cutoff could still have to refuse a launch. Removing the
    /// record un-fences it, so it must outlive every launch the cutoff covers.
    fn fence_is_live(&self, now_unix_ms: u64) -> bool {
        self.reject_before_unix_ms > 0
            && now_unix_ms
                < self
                    .reject_before_unix_ms
                    .saturating_add(LAUNCH_LIFETIME_MS)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticipantLifecycle {
    #[default]
    Admitted,
    Starting,
    Running,
    Exited,
    Cleaned,
}

/// `(pid, start_ticks)` is unique only within one boot, and the spool outlives a
/// reboot, so the boot scopes the identity. A record from another boot is dead.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupervisorRef {
    #[serde(default)]
    pub pid: u32,
    #[serde(default)]
    pub start_ticks: u64,
    #[serde(default)]
    pub boot_id: Option<String>,
}

/// How a persisted supervisor identity compares to the running system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootScope {
    /// Same boot: `(pid, start_ticks)` is meaningful and may be compared.
    Same,
    /// A different boot: nothing from before it survives.
    Different,
    /// One side has no boot id. Never `Different` — treating an unknown as a
    /// change declares live supervisors dead and releases running jobs' slices.
    Unknown,
}

impl SupervisorRef {
    pub fn boot_scope(&self, current_boot_id: Option<&str>) -> BootScope {
        match (self.boot_id.as_deref(), current_boot_id) {
            (Some(recorded), Some(current)) if recorded == current => BootScope::Same,
            (Some(_), Some(_)) => BootScope::Different,
            _ => BootScope::Unknown,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalReport {
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub acknowledged: bool,
}

/// `admission/<job>.<attempt>/participants/<step>.json`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParticipantAdmission {
    pub schema_version: u32,
    pub job_id: u32,
    pub run_attempt: u32,
    pub step_id: StepId,
    pub node: String,
    #[serde(default)]
    pub command_digest: String,
    #[serde(default)]
    pub allocation_subset: AdmittedResources,
    #[serde(default)]
    pub issued_at_unix_ms: u64,
    /// Controller-stamped and respected as delivered. A duration measured from
    /// local admission would hand a launch delayed in transit a fresh window.
    #[serde(default)]
    pub expires_at_unix_ms: u64,
    #[serde(default)]
    pub lifecycle: ParticipantLifecycle,
    #[serde(default)]
    pub supervisor: Option<SupervisorRef>,
    #[serde(default)]
    pub final_report: FinalReport,
}

impl ParticipantAdmission {
    pub fn new(
        job_id: u32,
        run_attempt: u32,
        step_id: StepId,
        node: impl Into<String>,
        allocation_subset: AdmittedResources,
    ) -> Self {
        Self {
            schema_version: ADMISSION_SCHEMA_VERSION,
            job_id,
            run_attempt,
            step_id,
            node: node.into(),
            command_digest: String::new(),
            allocation_subset,
            issued_at_unix_ms: 0,
            expires_at_unix_ms: 0,
            lifecycle: ParticipantLifecycle::Admitted,
            supervisor: None,
            final_report: FinalReport::default(),
        }
    }

    pub fn key(&self) -> Option<RunKey> {
        RunKey::new(self.job_id, self.run_attempt)
    }

    /// A deadline only ever *extends* a run past its own floor. Absent reads as
    /// "adds nothing", so a pre-upgrade launch cannot pin its run forever.
    fn holds_run_until(&self) -> u64 {
        self.expires_at_unix_ms
    }
}

/// One run and its participants, as loaded from disk.
#[derive(Debug, Clone)]
pub struct AdmittedRun {
    pub run: RunAdmission,
    pub participants: Vec<ParticipantAdmission>,
}

impl AdmittedRun {
    /// Removable once cleanup finished and nothing is still owed. An owed report
    /// or a conflict hold is the evidence that must outlive the processes.
    pub fn is_settled(&self) -> bool {
        self.run.state == RunState::Cleaned
            && self.run.conflict_hold.is_none()
            && self
                .participants
                .iter()
                .all(|p| !p.final_report.required || p.final_report.acknowledged)
    }

    /// The step that speaks for the run's allocation, which is the one a release
    /// and a run-level completion are keyed on.
    pub fn lifecycle_step(&self) -> Option<StepId> {
        self.run
            .lifecycle_owner_step
            .or_else(|| self.participants.first().map(|p| p.step_id))
    }

    /// Whether some participant still owes the controller a report. That debt is
    /// what nothing but an acknowledgement can discharge.
    pub fn owes_a_report(&self) -> bool {
        self.participants
            .iter()
            .any(|p| p.final_report.required && !p.final_report.acknowledged)
    }

    /// Every supervisor this run ever recorded. Empty means none was recorded,
    /// which is not the same as one being gone.
    pub fn recorded_supervisors(&self) -> impl Iterator<Item = &SupervisorRef> {
        self.participants
            .iter()
            .filter_map(|p| p.supervisor.as_ref())
    }

    /// Past every deadline with nothing left that could still act. Gating on a
    /// single `RunState` would make a run whose supervisor died mid-run immortal.
    pub fn aged_out(&self, now_unix_ms: u64, retention_ms: u64) -> bool {
        // An unknown creation time has no age, so it is never old enough.
        if self.run.created_at_unix_ms == 0 {
            return false;
        }
        self.participants.iter().all(|participant| {
            !matches!(
                participant.lifecycle,
                ParticipantLifecycle::Starting | ParticipantLifecycle::Running
            )
            // An owed report is an outstanding obligation, not staleness. Age
            // must never be the thing that drops one.
            && (!participant.final_report.required || participant.final_report.acknowledged)
            && now_unix_ms >= participant.holds_run_until()
        }) && now_unix_ms >= self.run.ages_out_at(retention_ms)
    }
}

/// What the agent found on disk for one admitted run at startup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunEvidence {
    /// A supervisor whose `(pid, start_ticks)` still matches a live process.
    pub supervisor_matches_a_live_process: bool,
    pub boot_scope_of_supervisor: Option<BootScope>,
    pub recorded_exit: Option<(i32, i32)>,
    /// Admitted but never spawned: no supervisor was ever recorded.
    pub never_spawned: bool,
    /// Runtime state with no admission record behind it, or an unreadable one.
    pub residual_or_corrupt: bool,
}

/// What the evidence says happened to a run. It decides what the agent
/// *reports*, never what it frees: only an acknowledgement frees a slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunDisposition {
    Running,
    SettledWithExit {
        exit_code: i32,
        signal: i32,
    },
    /// A reboot leaves nothing to be uncertain about, so this is the one
    /// disposition that reports a definite death with no exit code.
    DeadByReboot,
    NeverStarted,
    /// The supervisor is gone and recorded no exit. The agent does not know
    /// what happened and must say so rather than guess either way.
    Unknown,
    Corrupt,
}

impl RunDisposition {
    /// Whether the controller has to reconcile this run before the node is
    /// trusted: the agent cannot resolve it from local evidence alone.
    pub fn needs_reconciliation(self) -> bool {
        matches!(self, Self::Corrupt | Self::Unknown)
    }
}

pub fn classify_run(evidence: &RunEvidence) -> RunDisposition {
    if evidence.residual_or_corrupt {
        return RunDisposition::Corrupt;
    }
    // Checked before liveness: across a reboot a `(pid, start_ticks)` match is a
    // coincidence, and adopting it would hold a phantom and report it running.
    if evidence.boot_scope_of_supervisor == Some(BootScope::Different) {
        return RunDisposition::DeadByReboot;
    }
    if evidence.supervisor_matches_a_live_process {
        return RunDisposition::Running;
    }
    if let Some((exit_code, signal)) = evidence.recorded_exit {
        return RunDisposition::SettledWithExit { exit_code, signal };
    }
    if evidence.never_spawned {
        return RunDisposition::NeverStarted;
    }
    RunDisposition::Unknown
}

/// Why a launch was refused before anything was spawned for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchRefusal {
    /// Issued at or before this run's cutoff: the controller stopped this run
    /// after issuing the launch.
    Fenced,
    /// Arrived past its own deadline.
    Expired,
    /// Same identity, different command.
    ConflictingDigest,
}

/// Checked before admitting a launch. These defend against stale, duplicated
/// and reordered commands from the controller. They are not authorization.
#[derive(Debug, Clone, Default)]
pub struct LaunchFences {
    pub issued_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub command_digest: String,
}

impl LaunchFences {
    /// `None` to admit. A pre-upgrade controller sends zeroes, which disable the
    /// check that field drives rather than refusing every launch.
    pub fn check(
        &self,
        now_unix_ms: u64,
        reject_before_unix_ms: u64,
        admitted_digest: Option<&str>,
    ) -> Option<LaunchRefusal> {
        // Both sides are controller-stamped, so a jump in the agent's clock cannot
        // un-fence a stopped run -- though `fence_run` clamps what it stores.
        if reject_before_unix_ms > 0
            && self.issued_at_unix_ms > 0
            && self.issued_at_unix_ms <= reject_before_unix_ms
        {
            return Some(LaunchRefusal::Fenced);
        }
        if self.expires_at_unix_ms > 0 && now_unix_ms > self.expires_at_unix_ms {
            return Some(LaunchRefusal::Expired);
        }
        // An unstamped launch (pre-upgrade controller) is the one legitimate
        // empty digest; a stamped one must not read an empty digest as a repeat.
        let unstamped = self.issued_at_unix_ms == 0;
        match admitted_digest {
            Some(admitted)
                if admitted != self.command_digest
                    && !(unstamped && (admitted.is_empty() || self.command_digest.is_empty())) =>
            {
                Some(LaunchRefusal::ConflictingDigest)
            }
            _ => None,
        }
    }
}

/// One immutable cut of this node's ledger. Built whole, so absence and
/// emptiness stay distinguishable to the controller.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LedgerCut {
    pub agent_session_id: String,
    /// False when the agent could not enumerate its own state, which forbids the
    /// controller from acting on anything missing from `entries`.
    pub inventory_complete: bool,
    pub entries: Vec<LedgerCutEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerCutEntry {
    pub job_id: u32,
    pub run_attempt: u32,
    pub allocation: AdmittedResources,
    pub disposition: LedgerDisposition,
    pub conflict_hold: bool,
}

fn disposition_of(admitted: &AdmittedRun) -> LedgerDisposition {
    // A finished teardown outranks a hold: it is positive proof the payload is
    // gone, where a hold only says the agent could not account for the claim.
    if admitted.run.state == RunState::Cleaned {
        return LedgerDisposition::OverButCharged;
    }
    if admitted.run.conflict_hold.is_some() || admitted.run.is_over() {
        return LedgerDisposition::Unresolved;
    }
    LedgerDisposition::Held
}

impl AdmissionStore {
    /// One cut of everything this node believes it holds. A read failure yields
    /// an incomplete cut: an empty one asserts "I hold nothing".
    pub fn ledger_cut(&self, agent_session_id: &str) -> LedgerCut {
        let Ok(loaded) = self.load_all() else {
            return LedgerCut {
                agent_session_id: agent_session_id.to_string(),
                inventory_complete: false,
                entries: Vec::new(),
            };
        };
        let entries = loaded
            .runs
            .iter()
            // The cut is what the node physically holds, so only a slice going
            // back takes a record out of it. A lifecycle flag hides a live claim.
            .filter(|admitted| !admitted.run.slice_released)
            .map(|admitted| LedgerCutEntry {
                job_id: admitted.run.job_id,
                run_attempt: admitted.run.run_attempt,
                allocation: admitted.run.allocation.clone(),
                disposition: disposition_of(admitted),
                conflict_hold: admitted.run.conflict_hold.is_some(),
            })
            .collect();
        LedgerCut {
            agent_session_id: agent_session_id.to_string(),
            // A record that could not be read may still hold a claim, so the
            // controller must not treat its absence as proof of anything.
            inventory_complete: loaded.rejected.is_empty(),
            entries,
        }
    }
}

impl LedgerCut {
    /// Holding something the controller can actually settle. An unreadable
    /// record is not one: nothing it can do would ever clear that.
    pub fn wants_reconcile(&self) -> bool {
        self.entries.iter().any(|entry| entry.conflict_hold)
    }
}

/// A record the agent could not adopt. Held rather than deleted, because the
/// record is the only evidence that something here may still hold a claim.
#[derive(Debug, Clone)]
pub struct RejectedAdmission {
    pub path: PathBuf,
    pub reason: String,
    /// Filesystem mtime, the only age an unparseable record has. Zero leaves it
    /// uncollectable rather than measuring its age from 1970.
    pub modified_unix_ms: u64,
}

impl RejectedAdmission {
    fn new(path: PathBuf, reason: impl Into<String>) -> Self {
        let modified_unix_ms = modified_unix_ms(&path);
        Self {
            path,
            reason: reason.into(),
            modified_unix_ms,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct LoadedAdmissions {
    pub runs: Vec<AdmittedRun>,
    pub rejected: Vec<RejectedAdmission>,
}

#[derive(Clone)]
pub struct AdmissionStore {
    root: PathBuf,
    node: String,
    /// One mutex per run, guarding its load-mutate-write cycle so concurrent
    /// callers serialize per run rather than node-wide. Shared by every clone.
    run_locks: Arc<Mutex<HashMap<RunKey, Arc<Mutex<()>>>>>,
}

impl AdmissionStore {
    /// Whether this step answers for the run: the named owner alone, or --
    /// with none named -- any single participant, but only while it has one.
    fn step_answers_for_run(
        &self,
        run: &RunAdmission,
        run_key: RunKey,
        step_id: StepId,
    ) -> io::Result<bool> {
        if let Some(owner) = run.lifecycle_owner_step {
            return Ok(owner == step_id);
        }
        let (participants, _) = self.participants(run_key)?;
        Ok(match participants.as_slice() {
            [] => true,
            [only] => only.step_id == step_id,
            _ => false,
        })
    }

    pub fn new(state_dir: impl Into<PathBuf>, node: impl Into<String>) -> Self {
        Self {
            root: state_dir.into().join("admission"),
            node: node.into(),
            run_locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The mutex for one run's identity, created on first use. Held for a
    /// whole load-mutate-write cycle, never just the write.
    fn run_lock(&self, run_key: RunKey) -> Arc<Mutex<()>> {
        let mut locks = self.run_locks.lock().unwrap_or_else(|e| e.into_inner());
        locks
            .entry(run_key)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Runs `body` with this run's lock held for its entire duration, so no
    /// other mutator can observe or clobber a half-applied update to it.
    fn with_run_lock<T>(
        &self,
        run_key: RunKey,
        body: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<T> {
        // A struct so pruning still runs (via Drop) if `body` panics, rather
        // than leaking this run's map entry on every unwind past this call.
        struct PruneOnDrop<'a> {
            store: &'a AdmissionStore,
            run_key: RunKey,
            lock: Arc<Mutex<()>>,
        }
        impl Drop for PruneOnDrop<'_> {
            fn drop(&mut self) {
                self.store.prune_run_lock(self.run_key, &self.lock);
            }
        }

        let holder = PruneOnDrop {
            store: self,
            run_key,
            lock: self.run_lock(run_key),
        };
        let _guard = holder.lock.lock().unwrap_or_else(|e| e.into_inner());
        body()
    }

    /// Drop a run's lock entry once nothing else holds it, so a long-lived
    /// agent's lock map doesn't grow for every run it has ever admitted.
    fn prune_run_lock(&self, run_key: RunKey, lock: &Arc<Mutex<()>>) {
        let mut locks = self.run_locks.lock().unwrap_or_else(|e| e.into_inner());
        // Under this lock, a strong count of 2 (the map's own entry plus ours)
        // means no concurrent caller has been handed a clone to contend on it.
        if Arc::strong_count(lock) <= 2 {
            locks.remove(&run_key);
        }
    }

    pub(crate) fn participant_path(&self, run_key: RunKey, step_id: StepId) -> io::Result<PathBuf> {
        Ok(self
            .participants_dir(run_key)?
            .join(format!("{step_id}.json")))
    }

    /// Where a run's records live. The tree is keyed per attempt, so a widened
    /// key names a directory no run ever had and must not resolve to one.
    pub fn run_dir(&self, run: RunKey) -> io::Result<PathBuf> {
        let attempt = addressable_attempt(run)?;
        Ok(self.root.join(format!("{}.{attempt}", run.job_id())))
    }

    fn participants_dir(&self, run: RunKey) -> io::Result<PathBuf> {
        Ok(self.run_dir(run)?.join(PARTICIPANTS_DIR))
    }

    /// Every level, because `create_dir_all` leaves intermediates at the umask
    /// default and this tree holds the environment a run was admitted with.
    pub(crate) fn prepare_run_dir(&self, run: RunKey) -> io::Result<PathBuf> {
        let dir = self.run_dir(run)?;
        create_private_dir_all(&self.root)?;
        create_private_dir_all(&dir)?;
        Ok(dir)
    }

    fn prepare_participants_dir(&self, run: RunKey) -> io::Result<PathBuf> {
        let dir = self.prepare_run_dir(run)?.join(PARTICIPANTS_DIR);
        create_private_dir_all(&dir)?;
        Ok(dir)
    }

    /// Persist the entitlement before anything is spawned against it. Monotonic
    /// fields carry forward, so a relaunch cannot re-admit what a cutoff fenced.
    pub fn admit_run(&self, run: &RunAdmission) -> io::Result<()> {
        let key = run.key().ok_or_else(|| unaddressable(run.job_id))?;
        self.with_run_lock(key, || self.admit_run_locked(run))
    }

    /// The merge-and-write body of `admit_run`, for a caller that already
    /// holds this run's lock -- so it neither deadlocks on nor re-pays for it.
    fn admit_run_locked(&self, run: &RunAdmission) -> io::Result<()> {
        let key = run.key().ok_or_else(|| unaddressable(run.job_id))?;
        let mut run = run.clone();
        // Only a genuinely absent record starts from a blank slate; any other
        // read failure would silently reset the cutoffs a launch is fenced by.
        match self.load_run(key) {
            Ok(existing) => {
                run.reject_before_unix_ms = run
                    .reject_before_unix_ms
                    .max(existing.reject_before_unix_ms);
                run.max_launch_expiry_unix_ms = run
                    .max_launch_expiry_unix_ms
                    .max(existing.max_launch_expiry_unix_ms);
                // Cleared only by a write that itself carries a fresh ack.
                if run.conflict_hold.is_none() && run.controller_ack.release_raft_index.is_none() {
                    run.conflict_hold = existing.conflict_hold;
                }
                // A relaunch is always built fresh; without this, one arriving
                // after the controller ends this run would un-decide it here.
                if existing.state == RunState::Cleaned {
                    run.state = RunState::Cleaned;
                }
                // Same reasoning for the epilog debt: only a relaunch's fresh default
                // is refused, never a caller's own forward move (e.g. record_epilog).
                if run.cleanup.epilog == HookState::NotStarted
                    && existing.cleanup.epilog != HookState::NotStarted
                {
                    run.cleanup = existing.cleanup;
                }
                run.cancelled_by_controller |= existing.cancelled_by_controller;
                run.slice_released |= existing.slice_released;
                if run.controller_ack.release_raft_index.is_none() {
                    run.controller_ack = existing.controller_ack;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let dir = self.prepare_run_dir(key)?;
        publish_private(&dir, RUN_FILE, &encode(&run)?)
    }

    /// The write fsyncs twice. Callers hold locks every other job RPC needs, so
    /// it must not also occupy the executor thread.
    pub async fn admit_run_async(&self, run: RunAdmission) -> io::Result<()> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.admit_run(&run))
            .await
            .map_err(|error| io::Error::other(format!("admission write failed: {error}")))?
    }

    pub async fn admit_participant_async(
        &self,
        participant: ParticipantAdmission,
    ) -> io::Result<()> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.admit_participant(&participant))
            .await
            .map_err(|error| io::Error::other(format!("admission write failed: {error}")))?
    }

    pub fn admit_participant(&self, participant: &ParticipantAdmission) -> io::Result<()> {
        let key = participant
            .key()
            .ok_or_else(|| unaddressable(participant.job_id))?;
        self.with_run_lock(key, || self.admit_participant_locked(participant))
    }

    /// The write body of `admit_participant`, for a caller that already holds
    /// this run's lock -- so it neither deadlocks on nor re-pays for it.
    fn admit_participant_locked(&self, participant: &ParticipantAdmission) -> io::Result<()> {
        let key = participant
            .key()
            .ok_or_else(|| unaddressable(participant.job_id))?;
        let dir = self.prepare_participants_dir(key)?;
        publish_private(
            &dir,
            &format!("{}.json", participant.step_id),
            &encode(participant)?,
        )
    }

    pub fn load_run(&self, run_key: RunKey) -> io::Result<RunAdmission> {
        let dir = self.run_dir(run_key)?;
        // A record the agent could not have written is one it cannot trust, and
        // the read side has to reach the same verdict the write side does.
        verify_private_dir(&dir)?;
        let run: RunAdmission = decode(&fs::read(dir.join(RUN_FILE))?)?;
        self.validate_run(&run, run_key)?;
        Ok(run)
    }

    fn validate_run(&self, run: &RunAdmission, run_key: RunKey) -> io::Result<()> {
        if run.schema_version > ADMISSION_SCHEMA_VERSION {
            return Err(invalid(format!(
                "run record schema {} is newer than {ADMISSION_SCHEMA_VERSION}",
                run.schema_version
            )));
        }
        if run.key() != Some(run_key) {
            return Err(invalid(format!(
                "run record names {}.{} but lives in {run_key}",
                run.job_id, run.run_attempt
            )));
        }
        if run.node != self.node {
            return Err(invalid(format!(
                "run record belongs to node '{}', not '{}'",
                run.node, self.node
            )));
        }
        Ok(())
    }

    /// This run's participants, plus the files that could not be read.
    pub fn participants(
        &self,
        run_key: RunKey,
    ) -> io::Result<(Vec<ParticipantAdmission>, Vec<RejectedAdmission>)> {
        let dir = self.participants_dir(run_key)?;
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok((Vec::new(), Vec::new()))
            }
            Err(error) => return Err(error),
        };
        // Sorted by step so recovery order does not depend on the filesystem.
        let mut found: BTreeMap<StepId, ParticipantAdmission> = BTreeMap::new();
        let mut rejected = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            // A publish in flight is named `<step>.json.<uuid>.tmp`.
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            match self.load_participant(&path, run_key) {
                Ok(participant) => {
                    found.insert(participant.step_id, participant);
                }
                // One damaged file must not discard an otherwise-good run: that
                // would forget a claim the node may still be holding.
                Err(error) => rejected.push(RejectedAdmission::new(path, error.to_string())),
            }
        }
        Ok((found.into_values().collect(), rejected))
    }

    fn load_participant(&self, path: &Path, run_key: RunKey) -> io::Result<ParticipantAdmission> {
        let participant: ParticipantAdmission = decode(&fs::read(path)?)?;
        if participant.schema_version > ADMISSION_SCHEMA_VERSION {
            return Err(invalid(format!(
                "participant record schema {} is newer than {ADMISSION_SCHEMA_VERSION}",
                participant.schema_version
            )));
        }
        if participant.key() != Some(run_key) || participant.node != self.node {
            return Err(invalid(format!(
                "participant record at {} does not belong to {run_key} on '{}'",
                path.display(),
                self.node
            )));
        }
        Ok(participant)
    }

    fn run_dirs(&self) -> io::Result<Vec<PathBuf>> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(true))
            .map(|entry| entry.path())
            .collect();
        dirs.sort();
        Ok(dirs)
    }

    /// Every run on this node. One unreadable record is reported, never fatal:
    /// this runs at startup, and refusing to boot would strand the whole node.
    pub fn load_all(&self) -> io::Result<LoadedAdmissions> {
        let mut loaded = LoadedAdmissions::default();
        for dir in self.run_dirs()? {
            let Some((job_id, run_attempt)) = parse_run_dir_name(&dir) else {
                loaded.rejected.push(RejectedAdmission::new(
                    dir,
                    "run directory name is not <job>.<attempt>",
                ));
                continue;
            };
            let Some(run_key) = RunKey::new(job_id, run_attempt) else {
                loaded.rejected.push(RejectedAdmission::new(
                    dir,
                    "run directory names attempt 0, which is no run",
                ));
                continue;
            };
            let run = match self.load_run(run_key) {
                Ok(run) => run,
                Err(error) => {
                    loaded
                        .rejected
                        .push(RejectedAdmission::new(dir, error.to_string()));
                    continue;
                }
            };
            match self.participants(run_key) {
                Ok((participants, rejected)) => {
                    loaded.rejected.extend(rejected);
                    loaded.runs.push(AdmittedRun { run, participants });
                }
                Err(error) => loaded
                    .rejected
                    .push(RejectedAdmission::new(dir, error.to_string())),
            }
        }
        Ok(loaded)
    }

    /// Read-modify-write so cleanup cannot discard a hold or the creation time.
    /// The epilog debt only ever widens here: clearing one is the hook's to do.
    pub fn mark_run_cleaned(&self, run_key: RunKey, epilog: EpilogOwed) -> io::Result<bool> {
        self.with_run_lock(run_key, || {
            let mut run = match self.load_run(run_key) {
                Ok(run) => run,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error),
            };
            let already_cleaned = run.state == RunState::Cleaned;
            // Owner-loss is resolve_supervised_epilogs's call to make, on real
            // liveness evidence -- never this function's, first mark or not.
            let epilog = match (epilog, run.cleanup.epilog) {
                (EpilogOwed::Yes, HookState::NotStarted) => HookState::Pending,
                (_, recorded) => recorded,
            };
            if already_cleaned && run.cleanup.epilog == epilog {
                return Ok(true);
            }
            run.state = RunState::Cleaned;
            run.cleanup.epilog = epilog;
            self.admit_run_locked(&run)?;
            Ok(true)
        })
    }

    /// Settle a run the controller has answered, sparing a hook still in
    /// flight -- the answer and the cleaned state it settles, in one write.
    pub fn record_acknowledged_completion(
        &self,
        run_key: RunKey,
        answered_by: StepId,
        release_raft_index: u64,
    ) -> io::Result<bool> {
        self.with_run_lock(run_key, || {
            let mut run = match self.load_run(run_key) {
                Ok(run) => run,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error),
            };
            // A step that does not answer for the run cannot acknowledge it: its own
            // exit is not the controller's word that the run is over.
            if !self.step_answers_for_run(&run, run_key, answered_by)? {
                return Ok(false);
            }
            run.controller_ack.release_raft_index = Some(release_raft_index);
            // An acknowledged completion resolves exactly what a hold taken for an
            // untracked claim was preserving, and nothing else would ever clear it.
            run.conflict_hold = None;
            // Settling a hook still in flight is the teardown's to do, never an
            // acknowledgement's: the controller cannot see whose hook is still running.
            if !run.cleanup.epilog.is_in_flight() {
                run.state = RunState::Cleaned;
            }
            self.admit_run_locked(&run)?;
            Ok(true)
        })
    }

    /// Record a hook the run has heard nothing about yet, deciding and writing
    /// under one read so an outcome that landed meanwhile is never overwritten.
    pub fn record_epilog_if_unstarted(
        &self,
        run_key: RunKey,
        state: HookState,
    ) -> io::Result<bool> {
        self.with_run_lock(run_key, || {
            let mut run = match self.load_run(run_key) {
                Ok(run) => run,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error),
            };
            if run.cleanup.epilog != HookState::NotStarted {
                return Ok(false);
            }
            run.cleanup.epilog = state;
            self.admit_run_locked(&run)?;
            Ok(true)
        })
    }

    /// One run and everything admitted under it, plus what could not be read: a
    /// damaged participant file otherwise reads as a run that never had one.
    pub fn load_admitted(
        &self,
        run_key: RunKey,
    ) -> io::Result<(AdmittedRun, Vec<RejectedAdmission>)> {
        let run = self.load_run(run_key)?;
        let (participants, rejected) = self.participants(run_key)?;
        Ok((AdmittedRun { run, participants }, rejected))
    }

    /// Settle every hook whose owner `owner_is_gone` proves cannot still be running it.
    /// May call `owner_is_gone` twice per run; keep it cheap and idempotent.
    pub fn settle_hooks_whose_owner_is_gone(
        &self,
        owner_is_gone: impl Fn(&AdmittedRun) -> bool,
    ) -> io::Result<usize> {
        let loaded = self.load_all()?;
        let mut settled = 0;
        let mut failure = None;
        for admitted in loaded.runs {
            if !admitted.run.cleanup.epilog.is_in_flight() || !owner_is_gone(&admitted) {
                continue;
            }
            let Some(run_key) = admitted.run.key() else {
                continue;
            };
            // One unwritable record must not leave every other run's hook in
            // flight: nothing runs this again, so the rest would strand.
            match self.settle_epilog_if_owner_still_gone(run_key, &owner_is_gone) {
                Ok(true) => settled += 1,
                Ok(false) => {}
                Err(error) => failure = failure.or(Some(error)),
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(settled),
        }
    }

    /// Re-derives the target from a fresh load under the lock, so a settle that
    /// landed after the caller's unlocked scan is not overwritten by it.
    fn settle_epilog_if_owner_still_gone(
        &self,
        run_key: RunKey,
        owner_is_gone: &impl Fn(&AdmittedRun) -> bool,
    ) -> io::Result<bool> {
        self.with_run_lock(run_key, || {
            let (admitted, _) = match self.load_admitted(run_key) {
                Ok(pair) => pair,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error),
            };
            if !admitted.run.cleanup.epilog.is_in_flight() || !owner_is_gone(&admitted) {
                return Ok(false);
            }
            let mut run = admitted.run;
            run.cleanup.epilog = run.cleanup.epilog.settled_after_owner_loss();
            self.admit_run_locked(&run)?;
            Ok(true)
        })
    }

    /// Record the supervisor that now speaks for a participant. Read-modify-write
    /// so it cannot discard the deadline or digest the launch was admitted under.
    pub fn record_supervisor(
        &self,
        run_key: RunKey,
        step_id: StepId,
        supervisor: SupervisorRef,
    ) -> io::Result<bool> {
        self.with_run_lock(run_key, || {
            let path = self.participant_path(run_key, step_id)?;
            let mut participant = match self.load_participant(&path, run_key) {
                Ok(participant) => participant,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error),
            };
            // A registration for a participant already past Running is stale or
            // reordered; adopting it would resurrect a finished record instead.
            if !matches!(
                participant.lifecycle,
                ParticipantLifecycle::Admitted | ParticipantLifecycle::Starting
            ) {
                tracing::debug!(
                    %run_key,
                    %step_id,
                    lifecycle = ?participant.lifecycle,
                    "ignoring a supervisor registration for a participant past Running"
                );
                return Ok(false);
            }
            participant.supervisor = Some(supervisor);
            participant.lifecycle = ParticipantLifecycle::Running;
            self.admit_participant_locked(&participant)?;
            Ok(true)
        })
    }

    /// Mark a run as needing the controller's attention, preserving everything
    /// already on it. Idempotent: the first reason recorded is the one kept.
    pub fn take_conflict_hold(&self, run_key: RunKey, reason: &str) -> io::Result<HoldOutcome> {
        self.with_run_lock(run_key, || {
            let mut run = match self.load_run(run_key) {
                Ok(run) => run,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Ok(HoldOutcome::NoRecord)
                }
                Err(error) => return Err(error),
            };
            // Otherwise a release landing after the caller's snapshot re-plants a hold
            // on an already-freed slice; is_committed() alone misses a settled zero-index.
            if run.slice_released || run.controller_ack.is_committed() {
                return Ok(HoldOutcome::AlreadyReleased);
            }
            if run.conflict_hold.is_some() {
                return Ok(HoldOutcome::AlreadyHeld);
            }
            run.conflict_hold = Some(ConflictHold {
                reason: reason.to_string(),
                observed_at_unix_ms: now_unix_ms(),
            });
            self.admit_run_locked(&run)?;
            Ok(HoldOutcome::Taken)
        })
    }

    /// Raise a run's cutoff. Monotonic: a lower value is ignored, so a reordered
    /// or replayed fence can never un-cancel a run.
    pub fn fence_run(&self, run_key: RunKey, reject_before_unix_ms: u64) -> io::Result<u64> {
        let attempt = addressable_attempt(run_key)?;
        // Agent clock against a controller cutoff: running ahead leaves the strand
        // it prevents, behind can lower a cutoff past a launch that should fence.
        let reject_before_unix_ms =
            reject_before_unix_ms.min(now_unix_ms().saturating_add(LAUNCH_LIFETIME_MS));
        self.with_run_lock(run_key, || {
            let mut run = match self.load_run(run_key) {
                Ok(run) => run,
                // Fencing a run this node has no record of still has to hold: the
                // record is created below so a launch already in flight is refused.
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    let mut fresh = RunAdmission::new(
                        run_key.job_id(),
                        attempt,
                        &self.node,
                        AdmittedResources::default(),
                        now_unix_ms(),
                    );
                    fresh.reject_before_unix_ms = reject_before_unix_ms;
                    self.admit_run_locked(&fresh)?;
                    return Ok(reject_before_unix_ms);
                }
                Err(error) => return Err(error),
            };
            if reject_before_unix_ms <= run.reject_before_unix_ms {
                return Ok(run.reject_before_unix_ms);
            }
            run.reject_before_unix_ms = reject_before_unix_ms;
            self.admit_run_locked(&run)?;
            Ok(reject_before_unix_ms)
        })
    }

    /// The cutoff this run enforces, or none if it has no record yet. Read on
    /// every launch, so it must outlive the agent that recorded it.
    pub fn reject_before(&self, run_key: RunKey) -> Option<u64> {
        self.load_run(run_key)
            .ok()
            .map(|run| run.reject_before_unix_ms)
    }

    /// Whether this run's allocation may now be released: the owner step only, its
    /// completion acknowledged, and, per the record, no epilog still in flight.
    pub fn release_is_due(
        &self,
        run_key: RunKey,
        step_id: StepId,
    ) -> io::Result<Option<ReleaseWarrant>> {
        // Locked like a mutator: the run and, when the owner is unset, a second
        // read of participants must come from one consistent instant, not two.
        self.with_run_lock(run_key, || {
            let run = match self.load_run(run_key) {
                Ok(run) => run,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error),
            };
            // Without this the release fires for whichever participant happens to be
            // acknowledged first, which for a multi-step run is not the owner.
            if !self.step_answers_for_run(&run, run_key, step_id)? {
                return Ok(None);
            }
            if run.cleanup.epilog.is_in_flight() {
                return Ok(None);
            }
            Ok(match run.controller_ack.release_raft_index {
                Some(0) => Some(ReleaseWarrant::settled_unrecorded_claim(run_key)),
                Some(index) => Some(ReleaseWarrant::acknowledged(run_key, index)),
                None => None,
            })
        })
    }

    /// Whether the controller's word that it is not accounting for this run may be
    /// acted on yet. The agent keeps the veto: only it can see the hooks.
    pub fn settle_permit(&self, run_key: RunKey) -> io::Result<SettlePermit> {
        // Locked like a mutator: the run and the participants fallback read
        // below must come from one consistent instant, not two.
        self.with_run_lock(run_key, || {
            let run = match self.load_run(run_key) {
                Ok(run) => run,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Ok(SettlePermit::NoRecord)
                }
                Err(error) => return Err(error),
            };
            if run.state != RunState::Cleaned || run.cleanup.epilog.is_in_flight() {
                return Ok(SettlePermit::NotQuiescent);
            }
            let step_id = match run.lifecycle_owner_step {
                Some(step_id) => step_id,
                None => self
                    .participants(run_key)?
                    .0
                    .first()
                    .map(|participant| participant.step_id)
                    .unwrap_or(spur_core::step::STEP_BATCH),
            };
            Ok(SettlePermit::Due(step_id))
        })
    }

    /// Record how this run's epilog is going. The gate reads this, so a hook
    /// whose outcome never lands here is a gate that cannot bite.
    pub fn record_epilog(&self, run_key: RunKey, state: HookState) -> io::Result<bool> {
        self.with_run_lock(run_key, || {
            let mut run = match self.load_run(run_key) {
                Ok(run) => run,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error),
            };
            if run.cleanup.epilog == state {
                return Ok(true);
            }
            run.cleanup.epilog = state;
            self.admit_run_locked(&run)?;
            Ok(true)
        })
    }

    /// Record that the controller has committed this run's completion. The
    /// index is what distinguishes an acknowledgement from an unanswered RPC.
    pub fn record_controller_ack(
        &self,
        run_key: RunKey,
        answered_by: StepId,
        release_raft_index: u64,
    ) -> io::Result<bool> {
        self.take_controller_ack(run_key, Some(answered_by), |ack| {
            ack.release_raft_index = Some(release_raft_index)
        })
    }

    /// Record the controller's answer to a claim it has no record of -- an
    /// answer, not a commit. It answers for the whole run, so no step does.
    pub fn record_settled_claim(&self, run_key: RunKey) -> io::Result<bool> {
        self.take_controller_ack(run_key, None, |ack| {
            ack.settled_unrecorded_claim = true;
            // Zero means "no record at controller", not a real commit -- but
            // still settled enough to free resources.
            ack.release_raft_index = Some(0);
        })
    }

    fn take_controller_ack(
        &self,
        run_key: RunKey,
        answered_by: Option<StepId>,
        record: impl FnOnce(&mut ControllerAck),
    ) -> io::Result<bool> {
        self.with_run_lock(run_key, || {
            let mut run = match self.load_run(run_key) {
                Ok(run) => run,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error),
            };
            // A step that does not answer for the run cannot acknowledge it: its
            // own exit is not the controller's word that the run is over.
            if let Some(step_id) = answered_by {
                if !self.step_answers_for_run(&run, run_key, step_id)? {
                    return Ok(false);
                }
            }
            record(&mut run.controller_ack);
            // An acknowledged completion resolves exactly what a hold taken for an
            // untracked claim was preserving, and nothing else would ever clear it.
            run.conflict_hold = None;
            self.admit_run_locked(&run)?;
            Ok(true)
        })
    }

    /// Record the controller's cancel. Never creates a record: a cancel for a
    /// run this node never admitted has nothing to settle.
    pub fn mark_controller_cancelled(&self, run_key: RunKey) -> io::Result<bool> {
        self.with_run_lock(run_key, || {
            let mut run = match self.load_run(run_key) {
                Ok(run) => run,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error),
            };
            if run.cancelled_by_controller {
                return Ok(true);
            }
            run.cancelled_by_controller = true;
            self.admit_run_locked(&run)?;
            Ok(true)
        })
    }

    /// Note that a run's slice has gone back to the node. Callers must have
    /// released it against an acknowledgement, not merely intend to.
    pub fn record_slice_released(&self, run_key: RunKey) -> io::Result<bool> {
        self.with_run_lock(run_key, || {
            let mut run = match self.load_run(run_key) {
                Ok(run) => run,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error),
            };
            if run.slice_released {
                return Ok(true);
            }
            run.slice_released = true;
            self.admit_run_locked(&run)?;
            Ok(true)
        })
    }

    /// Discharge every report a run still owes. Sound only once the controller
    /// has taken the run's own completion: nothing answers a sibling's after that.
    pub fn discharge_owed_reports(&self, run_key: RunKey) -> io::Result<()> {
        let (participants, _) = self.participants(run_key)?;
        for participant in participants {
            self.record_report_acknowledged(run_key, participant.step_id)?;
        }
        Ok(())
    }

    /// Settle a run the controller has answered. Owed reports go with it, or the
    /// record is immortal; the slice flag is the release's to write, not this.
    pub fn settle_acknowledged_run(&self, run_key: RunKey) -> io::Result<bool> {
        if !self.record_settled_claim(run_key)? {
            return Ok(false);
        }
        self.discharge_owed_reports(run_key)?;
        self.mark_run_cleaned(run_key, EpilogOwed::No)
    }

    /// Mark a participant's completion as acknowledged, so the durable retry
    /// stops rediscovering it.
    pub fn record_report_acknowledged(&self, run_key: RunKey, step_id: StepId) -> io::Result<bool> {
        self.with_run_lock(run_key, || {
            let path = self.participant_path(run_key, step_id)?;
            let mut participant = match self.load_participant(&path, run_key) {
                Ok(participant) => participant,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error),
            };
            participant.final_report.acknowledged = true;
            participant.lifecycle = ParticipantLifecycle::Exited;
            self.admit_participant_locked(&participant)?;
            Ok(true)
        })
    }

    pub fn remove_participant(&self, run_key: RunKey, step_id: StepId) -> io::Result<()> {
        self.with_run_lock(run_key, || {
            let path = self.participant_path(run_key, step_id)?;
            match fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            }
        })
    }

    pub fn remove_run(&self, run_key: RunKey) -> io::Result<()> {
        self.with_run_lock(run_key, || {
            match fs::remove_dir_all(self.run_dir(run_key)?) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            }
        })
    }

    /// As [`Self::sweep`]'s per-run body, but re-validated under this run's own
    /// lock so a write landing between the unlocked scan and this call can't be swept.
    fn remove_run_if_eligible(
        &self,
        run_key: RunKey,
        now_unix_ms: u64,
        retention_ms: u64,
        charged: &HashSet<RunKey>,
    ) -> io::Result<bool> {
        self.with_run_lock(run_key, || {
            let run = match self.load_run(run_key) {
                Ok(run) => run,
                // Already gone -- another sweep, or the run's own teardown beat
                // us to it. Either way there is nothing left to remove.
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error),
            };
            if run.conflict_hold.is_some() || run.fence_is_live(now_unix_ms) {
                return Ok(false);
            }
            if charge_covers(charged, run.job_id, run.run_attempt) {
                return Ok(false);
            }
            let (participants, _) = self.participants(run_key)?;
            let admitted = AdmittedRun { run, participants };
            if !(admitted.is_settled() || admitted.aged_out(now_unix_ms, retention_ms)) {
                return Ok(false);
            }
            match fs::remove_dir_all(self.run_dir(run_key)?) {
                Ok(()) => Ok(true),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
                Err(error) => Err(error),
            }
        })
    }

    /// Three removal rules, and nothing else may delete a record: settled because it
    /// owes nothing, aged out because it never can, unreadable because only age can.
    pub fn sweep(
        &self,
        now_unix_ms: u64,
        retention_ms: u64,
        charged: &HashSet<RunKey>,
    ) -> io::Result<usize> {
        // A zero floor would collect a run the instant it is admitted, which is
        // before its launch has even spawned.
        let retention_ms = retention_ms.max(1);
        let mut removed = 0;
        let loaded = self.load_all()?;
        for admitted in loaded.runs {
            let run = &admitted.run;
            if run.conflict_hold.is_some() || run.fence_is_live(now_unix_ms) {
                continue;
            }
            // The record is the only instruction to release a slice, so taking
            // one still charged strands it with nothing left to say so.
            let Some(run_key) = run.key() else { continue };
            if charge_covers(charged, run.job_id, run.run_attempt) {
                continue;
            }
            // This snapshot only picks candidates worth locking for; the actual
            // delete decision is re-made fresh under the lock, above.
            if (admitted.is_settled() || admitted.aged_out(now_unix_ms, retention_ms))
                && self.remove_run_if_eligible(run_key, now_unix_ms, retention_ms, charged)?
            {
                removed += 1;
            }
        }
        removed += self.sweep_rejected(&loaded.rejected, now_unix_ms, retention_ms, charged)?;
        Ok(removed)
    }

    /// An unreadable record can never settle, so only age collects it -- and only
    /// run directories, since it may still describe a live claim.
    fn sweep_rejected(
        &self,
        rejected: &[RejectedAdmission],
        now_unix_ms: u64,
        retention_ms: u64,
        charged: &HashSet<RunKey>,
    ) -> io::Result<usize> {
        let mut removed = 0;
        for entry in rejected {
            // A damaged participant lives under a run that loaded fine; taking
            // the whole directory would discard a claim that reads perfectly.
            if entry.path.parent() != Some(self.root.as_path()) {
                continue;
            }
            // Unreadable is not evidence the slice it names came back.
            if parse_run_dir_name(&entry.path)
                .is_some_and(|(job_id, attempt)| charge_covers(charged, job_id, attempt))
            {
                continue;
            }
            if entry.modified_unix_ms == 0
                || now_unix_ms < entry.modified_unix_ms.saturating_add(retention_ms)
            {
                continue;
            }
            tracing::warn!(
                path = %entry.path.display(),
                reason = %entry.reason,
                "collecting an unreadable admission record past its retention"
            );
            // Route through the run's own lock whenever the name resolves to one,
            // so this delete can't race a concurrent locked writer for the same run.
            match parse_run_dir_name(&entry.path)
                .and_then(|(job_id, attempt)| RunKey::new(job_id, attempt))
            {
                Some(run_key) => self.remove_run(run_key)?,
                None => remove_path(&entry.path)?,
            }
            removed += 1;
        }
        Ok(removed)
    }
}

/// Whether anything charged names this run: its own key, or the widened key of a
/// charge no attempt could be put to. Exact containment would collect that one.
fn charge_covers(charged: &HashSet<RunKey>, job_id: u32, run_attempt: u32) -> bool {
    RunKey::new(job_id, run_attempt).is_some_and(|run| charged.contains(&run))
        || charged.contains(&RunKey::any_attempt(job_id))
}

/// Removes a run directory or a stray file left in the ledger root; a record
/// the scan could not parse may be either.
fn remove_path(path: &Path) -> io::Result<()> {
    let outcome = if path.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    match outcome {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn modified_unix_ms(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|since| since.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

pub fn parse_run_dir_name(path: &Path) -> Option<(u32, u32)> {
    let name = path.file_name()?.to_str()?;
    let (job, attempt) = name.split_once('.')?;
    let (job_id, run_attempt): (u32, u32) = (job.parse().ok()?, attempt.parse().ok()?);
    // `007.1` and `+7.1` both parse to (7, 1); accepting either would make one
    // run load twice and be swept on a sibling's behalf.
    (name == format!("{job_id}.{run_attempt}")).then_some((job_id, run_attempt))
}

fn encode<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|error| invalid(format!("serialize admission: {error}")))
}

fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> io::Result<T> {
    serde_json::from_slice(bytes).map_err(|error| invalid(format!("parse admission: {error}")))
}

/// The one place a widened key is refused: the ledger tree is keyed per attempt,
/// so there is no directory for "every attempt of this job".
fn addressable_attempt(run: RunKey) -> io::Result<u32> {
    run.attempt()
        .ok_or_else(|| invalid(format!("{run} names no single run to address")))
}

fn unaddressable(job_id: u32) -> io::Error {
    invalid(format!("run attempt 0 names no run of job {job_id}"))
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Milliseconds since the epoch, saturating rather than panicking on a clock
/// set before 1970.
pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

/// The kernel's boot id, when the platform exposes one. Constant for the
/// process, and read on every descriptor write, so it is read once.
pub fn current_boot_id() -> Option<String> {
    static BOOT_ID: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BOOT_ID
        .get_or_init(|| {
            let raw = fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
            let trimmed = raw.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use spur_core::step::{STEP_BATCH, STEP_INTERACTIVE};

    fn key(job_id: u32, run_attempt: u32) -> RunKey {
        RunKey::new(job_id, run_attempt).expect("attempts start at 1")
    }

    fn store(dir: &tempfile::TempDir) -> AdmissionStore {
        AdmissionStore::new(dir.path(), "n1")
    }

    /// The half of the agent's owner-loss proof this crate can express: a run
    /// that named no supervisor had the previous agent's own hook.
    fn no_supervisor_was_recorded(admitted: &AdmittedRun) -> bool {
        admitted.recorded_supervisors().next().is_none()
    }

    fn run_with(job_id: u32, attempt: u32, created_at: u64) -> RunAdmission {
        RunAdmission::new(
            job_id,
            attempt,
            "n1",
            AdmittedResources {
                cpu_ids: vec![0, 1],
                memory_mb: 4096,
                gpu_devices: vec![3],
            },
            created_at,
        )
    }

    #[test]
    fn a_run_round_trips_with_its_participants() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let run = run_with(7, 2, 1_000);
        store.admit_run(&run).unwrap();
        let mut participant =
            ParticipantAdmission::new(7, 2, STEP_BATCH, "n1", run.allocation.clone());
        participant.expires_at_unix_ms = 5_000;
        store.admit_participant(&participant).unwrap();

        let loaded = store.load_all().unwrap();
        assert!(loaded.rejected.is_empty());
        assert_eq!(loaded.runs.len(), 1);
        assert_eq!(loaded.runs[0].run, run);
        assert_eq!(loaded.runs[0].participants, vec![participant]);
    }

    // The whole point of writing to disk: a fresh handle after the process
    // that wrote it is gone must recover the same state, not a stale copy.
    #[test]
    fn a_fresh_handle_recovers_a_prior_handles_state() {
        let dir = tempfile::tempdir().unwrap();
        let run = run_with(7, 2, 1_000);
        let participant = ParticipantAdmission::new(7, 2, STEP_BATCH, "n1", run.allocation.clone());
        {
            let first = store(&dir);
            first.admit_run(&run).unwrap();
            first.admit_participant(&participant).unwrap();
            first
                .record_controller_ack(key(7, 2), STEP_BATCH, 42)
                .unwrap();
        }

        let second = store(&dir);
        let loaded = second.load_all().unwrap();
        assert!(loaded.rejected.is_empty());
        assert_eq!(loaded.runs.len(), 1);
        assert_eq!(
            loaded.runs[0].run.controller_ack.release_raft_index,
            Some(42)
        );
        assert_eq!(loaded.runs[0].participants, vec![participant]);
        assert!(second
            .release_is_due(key(7, 2), STEP_BATCH)
            .unwrap()
            .is_some());
    }

    #[test]
    fn records_and_directories_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let run = run_with(7, 1, 0);
        store.admit_run(&run).unwrap();
        store
            .admit_participant(&ParticipantAdmission::new(
                7,
                1,
                STEP_BATCH,
                "n1",
                AdmittedResources::default(),
            ))
            .unwrap();

        let mode = |p: PathBuf| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(store.root().to_path_buf()), 0o700);
        assert_eq!(mode(store.run_dir(key(7, 1)).unwrap()), 0o700);
        assert_eq!(mode(store.participants_dir(key(7, 1)).unwrap()), 0o700);
        assert_eq!(
            mode(store.run_dir(key(7, 1)).unwrap().join(RUN_FILE)),
            0o600
        );
        assert_eq!(
            mode(
                store
                    .participants_dir(key(7, 1))
                    .unwrap()
                    .join(format!("{STEP_BATCH}.json"))
            ),
            0o600
        );
    }

    #[test]
    fn a_foreign_node_record_is_rejected_not_adopted() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let mut run = run_with(7, 1, 0);
        run.node = "somewhere-else".into();
        store.admit_run(&run).unwrap();

        let loaded = store.load_all().unwrap();
        assert!(
            loaded.runs.is_empty(),
            "a foreign slice must not be adopted"
        );
        assert_eq!(loaded.rejected.len(), 1);
        assert!(loaded.rejected[0].reason.contains("somewhere-else"));
    }

    // A directory anyone could have written is not evidence the agent may act
    // on: reading it as good let a permissions slip free a live run's slice.
    #[test]
    fn a_run_directory_that_is_not_private_is_rejected_rather_than_read() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        std::fs::set_permissions(
            store.run_dir(key(7, 1)).unwrap(),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();

        let loaded = store.load_all().unwrap();
        assert!(
            loaded.runs.is_empty(),
            "a record the agent could not have written must not read as one it did"
        );
        assert_eq!(loaded.rejected.len(), 1);
        assert!(loaded.rejected[0].reason.contains("is not private"));
        assert!(
            !store.ledger_cut("session").inventory_complete,
            "a cut that cannot see this claim must not assert it is complete"
        );
    }

    #[test]
    fn a_record_that_contradicts_its_directory_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let run = run_with(7, 1, 0);
        let target = store.prepare_run_dir(key(9, 4)).unwrap();
        publish_private(&target, RUN_FILE, &encode(&run).unwrap()).unwrap();

        let loaded = store.load_all().unwrap();
        assert!(loaded.runs.is_empty());
        assert_eq!(loaded.rejected.len(), 1);
    }

    #[test]
    fn a_newer_schema_is_rejected_rather_than_partially_understood() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let mut run = run_with(7, 1, 0);
        run.schema_version = ADMISSION_SCHEMA_VERSION + 1;
        store.admit_run(&run).unwrap();

        let loaded = store.load_all().unwrap();
        assert!(loaded.runs.is_empty());
        assert!(loaded.rejected[0].reason.contains("newer"));
    }

    #[test]
    fn an_unreadable_record_is_reported_without_failing_the_scan() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 0)).unwrap();
        let broken = store.prepare_run_dir(key(8, 1)).unwrap();
        publish_private(&broken, RUN_FILE, b"{ not json").unwrap();

        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.runs.len(), 1, "the good run must still load");
        assert_eq!(loaded.rejected.len(), 1);
    }

    #[test]
    fn sweep_collects_an_unreadable_record_once_it_is_past_retention() {
        // Nothing can ever settle it, so without an age the scan rediscovers
        // the same broken directory on every tick, forever.
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let broken = store.prepare_run_dir(key(8, 1)).unwrap();
        publish_private(&broken, RUN_FILE, b"{ not json").unwrap();

        assert_eq!(
            store.sweep(now_unix_ms(), 60_000, &HashSet::new()).unwrap(),
            0,
            "an unreadable record may still describe a live claim"
        );
        assert!(broken.exists());

        assert_eq!(
            store
                .sweep(now_unix_ms() + 60_001, 60_000, &HashSet::new())
                .unwrap(),
            1
        );
        assert!(!broken.exists());
    }

    #[test]
    fn sweep_collects_a_run_directory_it_cannot_even_name() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let junk = store.root().join("not-a-run");
        create_private_dir_all(&junk).unwrap();

        assert_eq!(
            store.sweep(now_unix_ms(), 60_000, &HashSet::new()).unwrap(),
            0
        );
        assert_eq!(
            store
                .sweep(now_unix_ms() + 60_001, 60_000, &HashSet::new())
                .unwrap(),
            1
        );
        assert!(!junk.exists());
    }

    #[test]
    fn sweeping_a_damaged_participant_never_takes_its_run_with_it() {
        // The run reads perfectly and still owes a report; collecting it on a
        // sibling file's behalf would forget a claim the node holds.
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        let mut owed = ParticipantAdmission::new(7, 1, STEP_BATCH, "n1", Default::default());
        owed.final_report.required = true;
        store.admit_participant(&owed).unwrap();
        publish_private(
            &store.run_dir(key(7, 1)).unwrap().join("participants"),
            "5.json",
            b"{ x",
        )
        .unwrap();

        assert_eq!(store.sweep(u64::MAX, 60_000, &HashSet::new()).unwrap(), 0);
        assert!(store.load_run(key(7, 1)).is_ok());
    }

    #[test]
    fn sweep_removes_a_settled_run_and_keeps_one_still_owed() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);

        let mut settled = run_with(1, 1, 1);
        settled.state = RunState::Cleaned;
        store.admit_run(&settled).unwrap();
        let mut reported = ParticipantAdmission::new(1, 1, STEP_BATCH, "n1", Default::default());
        reported.final_report = FinalReport {
            required: true,
            acknowledged: true,
        };
        store.admit_participant(&reported).unwrap();

        let mut owed = run_with(2, 1, 1);
        owed.state = RunState::Cleaned;
        store.admit_run(&owed).unwrap();
        let mut unacked = ParticipantAdmission::new(2, 1, STEP_BATCH, "n1", Default::default());
        unacked.final_report = FinalReport {
            required: true,
            acknowledged: false,
        };
        store.admit_participant(&unacked).unwrap();

        assert_eq!(store.sweep(10_000, 1_000, &HashSet::new()).unwrap(), 1);
        assert!(store.load_run(key(1, 1)).is_err());
        assert!(
            store.load_run(key(2, 1)).is_ok(),
            "an owed report must be kept"
        );
    }

    #[test]
    fn sweep_keeps_a_settled_run_whose_slice_is_still_charged() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);

        let mut settled = run_with(1, 1, 1);
        settled.state = RunState::Cleaned;
        store.admit_run(&settled).unwrap();

        let charged = HashSet::from([key(1, 1)]);
        assert_eq!(store.sweep(u64::MAX, 1_000, &charged).unwrap(), 0);
        assert!(
            store.load_run(key(1, 1)).is_ok(),
            "the record is the only thing that can still order the release"
        );
        assert_eq!(store.sweep(u64::MAX, 1_000, &HashSet::new()).unwrap(), 1);
    }

    #[test]
    fn sweep_keeps_an_unreadable_record_whose_slice_is_still_charged() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let broken = store.prepare_run_dir(key(1, 1)).unwrap();
        publish_private(&broken, RUN_FILE, b"{ not json").unwrap();

        let charged = HashSet::from([key(1, 1)]);
        assert_eq!(store.sweep(u64::MAX, 1, &charged).unwrap(), 0);
        assert_eq!(store.sweep(u64::MAX, 1, &HashSet::new()).unwrap(), 1);
    }

    // A charge no attempt names has to spare its own job and no one else's;
    // halting the sweep node-wide instead grows the ledger without bound.
    #[test]
    fn a_widened_charge_spares_its_own_job_and_sweeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);

        let mut held = run_with(1, 2, 1);
        held.state = RunState::Cleaned;
        store.admit_run(&held).unwrap();
        let unreadable = store.prepare_run_dir(key(1, 3)).unwrap();
        publish_private(&unreadable, RUN_FILE, b"{ not json").unwrap();

        let mut collectable = run_with(2, 1, 1);
        collectable.state = RunState::Cleaned;
        store.admit_run(&collectable).unwrap();

        let charged = HashSet::from([RunKey::any_attempt(1)]);
        assert_eq!(store.sweep(u64::MAX, 1, &charged).unwrap(), 1);
        assert!(
            store.load_run(key(1, 2)).is_ok(),
            "an unnameable charge still owes its job's records a release"
        );
        assert!(
            store.root().join("1.3").exists(),
            "unreadable is not evidence the widened charge came back"
        );
        assert!(
            store.load_run(key(2, 1)).is_err(),
            "another job's settled record must still be collected"
        );
    }

    #[test]
    fn sweep_keeps_a_fenced_record_only_while_its_cutoff_could_refuse_a_launch() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let mut fenced = run_with(7, 1, 1_000);
        fenced.state = RunState::Cleaned;
        store.admit_run(&fenced).unwrap();
        store.fence_run(key(7, 1), 10_000).unwrap();

        assert_eq!(
            store
                .sweep(10_000 + LAUNCH_LIFETIME_MS - 1, 1, &HashSet::new())
                .unwrap(),
            0,
            "collecting the record would re-admit the launch the cutoff refuses"
        );
        assert!(store.load_run(key(7, 1)).is_ok());
        assert_eq!(
            store
                .sweep(10_000 + LAUNCH_LIFETIME_MS, 1, &HashSet::new())
                .unwrap(),
            1,
            "an expired cutoff must not keep a settled record forever"
        );
    }

    #[test]
    fn settling_a_cancelled_run_discharges_the_report_that_would_outlive_it() {
        // The controller answers no report for a run it has forgotten, so an
        // owed one left behind by a cancel would make the record immortal.
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1_000)).unwrap();
        let mut owed = ParticipantAdmission::new(7, 1, STEP_BATCH, "n1", Default::default());
        owed.final_report.required = true;
        store.admit_participant(&owed).unwrap();

        assert!(store.settle_acknowledged_run(key(7, 1)).unwrap());
        assert_eq!(
            store.sweep(u64::MAX, 1, &HashSet::new()).unwrap(),
            1,
            "a settled cancel must leave nothing behind"
        );
    }

    #[test]
    fn remove_run_if_eligible_re_checks_under_the_lock_instead_of_trusting_a_stale_decision() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1_000)).unwrap();
        let mut owed = ParticipantAdmission::new(7, 1, STEP_BATCH, "n1", Default::default());
        owed.final_report.required = true;
        store.admit_participant(&owed).unwrap();
        assert!(store.settle_acknowledged_run(key(7, 1)).unwrap());

        // Simulates a write landing between `sweep`'s unlocked scan (which saw the settled
        // snapshot above) and the locked recheck below: a fresh, unacknowledged participant.
        let mut fresh = ParticipantAdmission::new(7, 1, STEP_INTERACTIVE, "n1", Default::default());
        fresh.final_report.required = true;
        store.admit_participant(&fresh).unwrap();

        let removed = store
            .remove_run_if_eligible(key(7, 1), u64::MAX, 1, &HashSet::new())
            .unwrap();
        assert!(
            !removed,
            "a fresh unacknowledged participant must block the delete a stale snapshot decided on"
        );
        assert!(
            store.load_run(key(7, 1)).is_ok(),
            "the record, and the participant that just landed, must survive"
        );
    }

    #[test]
    fn a_fence_for_attempt_zero_leaves_no_phantom_record() {
        assert!(
            RunKey::new(7, 0).is_none(),
            "attempt 0 must not be nameable, or a fence invents a record for it"
        );
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        assert!(store.fence_run(RunKey::any_attempt(7), 5_000).is_err());
        assert!(
            store.load_all().unwrap().runs.is_empty(),
            "a wildcard attempt must not be given a record to hold a claim with"
        );
    }

    #[test]
    fn sweep_collects_a_run_whose_launch_never_produced_anything() {
        // Its launch aborted before any expiry was set, so created_at is the
        // only age it has; without that it could never be collected.
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1_000)).unwrap();

        assert_eq!(
            store.sweep(1_500, 1_000, &HashSet::new()).unwrap(),
            0,
            "not yet aged out"
        );
        assert_eq!(store.sweep(2_001, 1_000, &HashSet::new()).unwrap(), 1);
    }

    #[test]
    fn sweep_never_collects_a_run_under_a_conflict_hold() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let hold = || {
            Some(ConflictHold {
                reason: "residual runtime state".into(),
                observed_at_unix_ms: 0,
            })
        };

        let mut cleaned = run_with(7, 1, 1);
        cleaned.state = RunState::Cleaned;
        cleaned.conflict_hold = hold();
        store.admit_run(&cleaned).unwrap();

        // Aged out as well: the hold has to outrank both removal rules, and
        // only this one reaches the age path.
        let mut aged = run_with(8, 1, 1);
        aged.conflict_hold = hold();
        store.admit_run(&aged).unwrap();

        assert_eq!(store.sweep(u64::MAX, 0, &HashSet::new()).unwrap(), 0);
        assert!(store.load_run(key(7, 1)).is_ok());
        assert!(store.load_run(key(8, 1)).is_ok());
    }

    #[test]
    fn sweep_keeps_a_run_whose_participant_started() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        let mut started = ParticipantAdmission::new(7, 1, STEP_BATCH, "n1", Default::default());
        started.lifecycle = ParticipantLifecycle::Running;
        store.admit_participant(&started).unwrap();

        assert_eq!(store.sweep(u64::MAX, 0, &HashSet::new()).unwrap(), 0);
    }

    #[test]
    fn a_participant_with_no_deadline_does_not_pin_its_run() {
        // A pre-upgrade controller sends no expiry; reading absent as "never
        // expires" would make such a run uncollectable.
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let run = run_with(7, 1, 1_000);
        store.admit_run(&run).unwrap();
        let participant = ParticipantAdmission::new(7, 1, STEP_BATCH, "n1", Default::default());
        assert_eq!(participant.expires_at_unix_ms, 0);
        store.admit_participant(&participant).unwrap();

        assert_eq!(store.sweep(1_500, 1_000, &HashSet::new()).unwrap(), 0);
        assert_eq!(store.sweep(2_001, 1_000, &HashSet::new()).unwrap(), 1);
    }

    #[test]
    fn an_unexpired_participant_deadline_holds_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        let mut participant = ParticipantAdmission::new(7, 1, STEP_BATCH, "n1", Default::default());
        participant.expires_at_unix_ms = 9_000;
        store.admit_participant(&participant).unwrap();

        assert_eq!(store.sweep(5_000, 1_000, &HashSet::new()).unwrap(), 0);
        assert_eq!(store.sweep(9_001, 1_000, &HashSet::new()).unwrap(), 1);
    }

    #[test]
    fn a_run_whose_supervisor_died_mid_run_still_ages_out() {
        // Gating age-out on one RunState made such a record immortal: it
        // reaches neither Cleaned nor stays Admitted.
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let stuck = run_with(7, 1, 1_000);
        store.admit_run(&stuck).unwrap();
        let mut exited = ParticipantAdmission::new(7, 1, STEP_BATCH, "n1", Default::default());
        exited.lifecycle = ParticipantLifecycle::Exited;
        store.admit_participant(&exited).unwrap();

        assert_eq!(store.sweep(1_500, 1_000, &HashSet::new()).unwrap(), 0);
        assert_eq!(store.sweep(2_001, 1_000, &HashSet::new()).unwrap(), 1);
    }

    #[test]
    fn a_record_with_no_creation_time_is_never_aged_out() {
        // Its age would otherwise be measured from 1970, making every such
        // record instantly collectable.
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 0)).unwrap();

        assert_eq!(store.sweep(u64::MAX, 1_000, &HashSet::new()).unwrap(), 0);
        assert!(store.load_run(key(7, 1)).is_ok());
    }

    #[test]
    fn a_zero_retention_does_not_collect_a_just_admitted_run() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 5_000)).unwrap();

        assert_eq!(store.sweep(5_000, 0, &HashSet::new()).unwrap(), 0);
        assert!(store.load_run(key(7, 1)).is_ok());
    }

    #[test]
    fn marking_a_run_cleaned_preserves_the_evidence_on_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let mut run = run_with(7, 1, 1_234);
        run.conflict_hold = Some(ConflictHold {
            reason: "residual runtime state".into(),
            observed_at_unix_ms: 99,
        });
        store.admit_run(&run).unwrap();

        assert!(store.mark_run_cleaned(key(7, 1), EpilogOwed::No).unwrap());
        let after = store.load_run(key(7, 1)).unwrap();
        assert_eq!(after.state, RunState::Cleaned);
        assert_eq!(after.created_at_unix_ms, 1_234, "the age must survive");
        assert_eq!(after.allocation, run.allocation);
        assert!(after.conflict_hold.is_some(), "a hold must survive cleanup");
    }

    #[test]
    fn marking_a_run_that_was_never_admitted_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!store(&dir)
            .mark_run_cleaned(key(9, 9), EpilogOwed::No)
            .unwrap());
    }

    #[test]
    fn one_damaged_participant_does_not_discard_its_run() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        store
            .admit_participant(&ParticipantAdmission::new(
                7,
                1,
                STEP_BATCH,
                "n1",
                Default::default(),
            ))
            .unwrap();
        let participants = store.run_dir(key(7, 1)).unwrap().join("participants");
        publish_private(&participants, "5.json", b"{ not json").unwrap();

        let loaded = store.load_all().unwrap();
        assert_eq!(
            loaded.runs.len(),
            1,
            "forgetting the run would forget a claim the node may still hold"
        );
        assert_eq!(loaded.runs[0].participants.len(), 1);
        assert_eq!(loaded.rejected.len(), 1);
    }

    #[test]
    fn a_non_canonical_run_directory_is_rejected() {
        // `007.1` parses to (7, 1) and would make the real run load twice, then
        // be swept on the impostor's behalf.
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        let impostor = store.root().join("007.1");
        create_private_dir_all(&impostor).unwrap();
        publish_private(&impostor, RUN_FILE, &encode(&run_with(7, 1, 1)).unwrap()).unwrap();

        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.runs.len(), 1);
        assert_eq!(loaded.rejected.len(), 1);
    }

    #[test]
    fn a_launch_issued_before_the_cutoff_is_fenced() {
        let fences = LaunchFences {
            issued_at_unix_ms: 1_000,
            ..Default::default()
        };
        assert_eq!(
            fences.check(9_999, 1_000, None),
            Some(LaunchRefusal::Fenced),
            "issued at the cutoff is fenced, not merely before it"
        );
        assert_eq!(fences.check(9_999, 999, None), None);
    }

    #[test]
    fn the_fence_does_not_depend_on_the_agent_clock() {
        // Both sides are controller-stamped, so no clock jump on this node can
        // un-fence a run the controller already stopped.
        let fences = LaunchFences {
            issued_at_unix_ms: 500,
            ..Default::default()
        };
        for agent_now in [0, 1, u64::MAX] {
            assert_eq!(
                fences.check(agent_now, 1_000, None),
                Some(LaunchRefusal::Fenced)
            );
        }
    }

    #[test]
    fn a_launch_past_its_deadline_is_refused() {
        let fences = LaunchFences {
            issued_at_unix_ms: 100,
            expires_at_unix_ms: 1_000,
            ..Default::default()
        };
        assert_eq!(
            fences.check(1_000, 0, None),
            None,
            "the deadline is inclusive"
        );
        assert_eq!(fences.check(1_001, 0, None), Some(LaunchRefusal::Expired));
    }

    #[test]
    fn a_conflicting_command_under_one_identity_is_refused() {
        let fences = LaunchFences {
            issued_at_unix_ms: 100,
            command_digest: "aaa".into(),
            ..Default::default()
        };
        assert_eq!(
            fences.check(200, 0, Some("aaa")),
            None,
            "an exact repeat is idempotent"
        );
        assert_eq!(
            fences.check(200, 0, Some("bbb")),
            Some(LaunchRefusal::ConflictingDigest)
        );
    }

    #[test]
    fn a_pre_upgrade_launch_carrying_no_fences_is_admitted() {
        // A controller that stamps nothing must not have every launch refused.
        let fences = LaunchFences::default();
        assert_eq!(fences.check(u64::MAX, 5_000, Some("aaa")), None);
    }

    #[test]
    fn a_fence_never_moves_backwards() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();

        assert_eq!(store.fence_run(key(7, 1), 5_000).unwrap(), 5_000);
        // A reordered or replayed fence must not un-cancel a stopped run.
        assert_eq!(store.fence_run(key(7, 1), 1_000).unwrap(), 5_000);
        assert_eq!(store.reject_before(key(7, 1)), Some(5_000));
    }

    #[test]
    fn a_fence_past_every_launch_it_could_cover_is_clamped() {
        // A cutoff the clock never reaches fences the run for good, and the
        // phantom record it creates has no other writer to lift it.
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let far_future = now_unix_ms().saturating_add(LAUNCH_LIFETIME_MS * 1000);

        let applied = store.fence_run(key(7, 1), far_future).unwrap();
        assert!(
            applied < far_future,
            "a skewed cutoff must not be taken verbatim"
        );
        assert!(
            applied <= now_unix_ms().saturating_add(LAUNCH_LIFETIME_MS),
            "a cutoff past the last launch it could cover never ages out"
        );
        assert_eq!(store.reject_before(key(7, 1)), Some(applied));
    }

    #[test]
    fn fencing_a_run_with_no_record_still_takes_effect() {
        // The launch may be in flight right now; with no record the cutoff has
        // nowhere to live and the launch would land after its own cancel.
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        assert_eq!(store.fence_run(key(7, 1), 5_000).unwrap(), 5_000);
        assert_eq!(store.reject_before(key(7, 1)), Some(5_000));
    }

    #[test]
    fn a_fence_preserves_what_the_run_already_holds() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let run = run_with(7, 1, 42);
        store.admit_run(&run).unwrap();

        store.fence_run(key(7, 1), 5_000).unwrap();
        let after = store.load_run(key(7, 1)).unwrap();
        assert_eq!(after.allocation, run.allocation);
        assert_eq!(after.created_at_unix_ms, 42);
    }

    // Reading "no prior record" out of an IO error un-fences the run and writes
    // over the only evidence that it may still hold a claim.
    #[test]
    fn a_record_that_cannot_be_read_is_not_a_record_that_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let damaged = store.prepare_run_dir(key(7, 1)).unwrap();
        publish_private(&damaged, RUN_FILE, b"{ not json").unwrap();

        let error = store
            .admit_run(&run_with(7, 1, 1))
            .expect_err("an unreadable prior record must not be written over");
        assert_ne!(error.kind(), io::ErrorKind::NotFound);

        let loaded = store.load_all().unwrap();
        assert!(loaded.runs.is_empty());
        assert_eq!(
            loaded.rejected.len(),
            1,
            "the damaged record is the only thing saying this run may hold a claim"
        );
    }

    // The controller cannot repair a file on this node, so asking it to try
    // again on every heartbeat is a loop with no exit.
    #[test]
    fn an_unreadable_record_is_not_something_to_ask_the_controller_about() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let broken = store.prepare_run_dir(key(8, 1)).unwrap();
        publish_private(&broken, RUN_FILE, b"{ not json").unwrap();

        let cut = store.ledger_cut("session-a");
        assert!(
            !cut.inventory_complete,
            "the controller still has to be told"
        );
        assert!(
            !cut.wants_reconcile(),
            "nothing a reconcile can do would clear an unreadable record"
        );

        store.admit_run(&run_with(7, 1, 1)).unwrap();
        store
            .take_conflict_hold(key(7, 1), "held with no tracked job")
            .unwrap();
        assert!(
            store.ledger_cut("session-a").wants_reconcile(),
            "a claim with no job behind it is exactly what the controller settles"
        );
    }

    // The strand this fixes: teardown finished, the report never landed, and a
    // hold that reported "cannot tell" left the controller nothing to answer.
    #[test]
    fn a_finished_run_still_holding_its_slice_reports_that_it_is_over() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        store.mark_run_cleaned(key(7, 1), EpilogOwed::No).unwrap();
        store
            .take_conflict_hold(key(7, 1), "held with no tracked job")
            .unwrap();

        let entry = store
            .ledger_cut("session-a")
            .entries
            .into_iter()
            .find(|entry| entry.job_id == 7)
            .expect("a run holding a slice stays in the cut");
        assert_eq!(
            entry.disposition,
            LedgerDisposition::OverButCharged,
            "a finished teardown is what licenses the controller to answer the claim"
        );
        assert!(
            entry.conflict_hold,
            "the hold still travels; it just no longer hides the teardown"
        );
    }

    #[test]
    fn a_run_the_agent_cannot_account_for_still_reports_unresolved() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        store
            .take_conflict_hold(key(7, 1), "held with no tracked job")
            .unwrap();

        let entry = store
            .ledger_cut("session-a")
            .entries
            .into_iter()
            .find(|entry| entry.job_id == 7)
            .expect("a run holding a slice stays in the cut");
        assert_eq!(
            entry.disposition,
            LedgerDisposition::Unresolved,
            "nothing proves this run is over, so nothing may settle it"
        );
        assert!(!entry.disposition.may_be_settled());
    }

    #[test]
    fn a_settled_unrecorded_claim_reports_unresolved_before_teardown_is_marked() {
        // release_raft_index=Some(0) is still a controller decision that ends
        // the run; a cut taken before Cleaned must not read it as an ordinary
        // live claim.
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        store.record_settled_claim(key(7, 1)).unwrap();

        let entry = store
            .ledger_cut("session-a")
            .entries
            .into_iter()
            .find(|entry| entry.job_id == 7)
            .expect("a run holding a slice stays in the cut");
        assert_eq!(entry.disposition, LedgerDisposition::Unresolved);
    }

    #[test]
    fn a_cancel_whose_teardown_is_still_running_may_not_be_settled() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        store.mark_controller_cancelled(key(7, 1)).unwrap();

        let entry = store
            .ledger_cut("session-a")
            .entries
            .into_iter()
            .find(|entry| entry.job_id == 7)
            .expect("a run holding a slice stays in the cut");
        assert!(
            !entry.disposition.may_be_settled(),
            "the payload may still be exiting; releasing its cores hands them to a second job"
        );
        assert!(
            entry.disposition.already_accounted_for(),
            "it is still not something to kill"
        );
    }

    // What every completion looks like on a cluster with an epilog hook: teardown
    // has marked the record cleaned and the hook is still on the cores.
    #[test]
    fn a_finished_teardown_with_a_live_epilog_asks_to_be_settled_but_refuses_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        store.mark_run_cleaned(key(7, 1), EpilogOwed::No).unwrap();
        store.record_epilog(key(7, 1), HookState::Running).unwrap();

        let entry = store
            .ledger_cut("session-a")
            .entries
            .into_iter()
            .find(|entry| entry.job_id == 7)
            .expect("a run holding a slice stays in the cut");
        assert!(
            entry.disposition.may_be_settled(),
            "the cut cannot see the hook, so the controller will ask"
        );
        assert_eq!(
            store.settle_permit(key(7, 1)).unwrap(),
            SettlePermit::NotQuiescent,
            "and the agent, which can see it, is what refuses"
        );

        store
            .record_epilog(key(7, 1), HookState::Succeeded)
            .unwrap();
        assert!(matches!(
            store.settle_permit(key(7, 1)).unwrap(),
            SettlePermit::Due(_)
        ));
    }

    #[test]
    fn a_settle_permit_for_a_run_with_no_record_names_no_step() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        assert_eq!(
            store.settle_permit(key(7, 1)).unwrap(),
            SettlePermit::NoRecord
        );
    }

    // The window the record could not describe: a hook owed but not yet started,
    // which `not_started` cannot tell apart from a node with no hook at all.
    #[test]
    fn a_teardown_that_still_owes_a_hook_cannot_record_itself_as_quiescent() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        store.mark_run_cleaned(key(7, 1), EpilogOwed::Yes).unwrap();

        let run = store.load_run(key(7, 1)).unwrap();
        assert_eq!(run.state, RunState::Cleaned);
        assert_eq!(run.cleanup.epilog, HookState::Pending);
        assert_eq!(
            store.settle_permit(key(7, 1)).unwrap(),
            SettlePermit::NotQuiescent,
            "the hook has not run yet, so these cores are not the node's"
        );
        assert!(store
            .release_is_due(key(7, 1), spur_core::step::STEP_BATCH)
            .unwrap()
            .is_none());

        store.record_epilog(key(7, 1), HookState::Running).unwrap();
        assert_eq!(
            store.settle_permit(key(7, 1)).unwrap(),
            SettlePermit::NotQuiescent
        );
        store
            .record_epilog(key(7, 1), HookState::Succeeded)
            .unwrap();
        assert!(matches!(
            store.settle_permit(key(7, 1)).unwrap(),
            SettlePermit::Due(_)
        ));
    }

    // A node with no epilog configured must not be held by the debt a node with
    // one records, or every completion waits for a hook that never comes.
    #[test]
    fn a_teardown_that_owes_no_hook_is_quiescent_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        store.mark_run_cleaned(key(7, 1), EpilogOwed::No).unwrap();

        assert_eq!(
            store.load_run(key(7, 1)).unwrap().cleanup.epilog,
            HookState::NotStarted
        );
        assert!(matches!(
            store.settle_permit(key(7, 1)).unwrap(),
            SettlePermit::Due(_)
        ));
    }

    // Teardown can be marked by one caller and the hook owed by a later one, so a
    // mark that stops at the first would drop the debt the gate is built on.
    #[test]
    fn a_later_mark_can_still_add_the_hook_a_cleaned_run_owes() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        store.mark_run_cleaned(key(7, 1), EpilogOwed::No).unwrap();

        store.mark_run_cleaned(key(7, 1), EpilogOwed::Yes).unwrap();

        assert_eq!(
            store.load_run(key(7, 1)).unwrap().cleanup.epilog,
            HookState::Pending,
            "a run already marked cleaned must still be able to take on a hook"
        );
        assert!(
            store
                .release_is_due(key(7, 1), spur_core::step::STEP_BATCH)
                .unwrap()
                .is_none(),
            "and that debt must hold the release"
        );
    }

    // Three of the mark's callers hardcode "nothing owed". None of them has seen
    // the hook, so none may be the one that declares it over.
    #[test]
    fn a_later_mark_never_settles_a_hook_still_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        store.mark_run_cleaned(key(7, 1), EpilogOwed::Yes).unwrap();
        store.record_epilog(key(7, 1), HookState::Running).unwrap();

        store.mark_run_cleaned(key(7, 1), EpilogOwed::No).unwrap();

        assert_eq!(
            store.load_run(key(7, 1)).unwrap().cleanup.epilog,
            HookState::Running,
            "a caller that never saw the hook must not settle it"
        );
        assert!(matches!(
            store.settle_permit(key(7, 1)).unwrap(),
            SettlePermit::NotQuiescent
        ));
    }

    // Real production order: hold_cancelled_run_for_epilog records Pending, then
    // the process exit's first cleanup mark must not treat that as owner-loss.
    #[test]
    fn the_first_mark_never_settles_a_hook_already_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        store
            .record_epilog_if_unstarted(key(7, 1), HookState::Pending)
            .unwrap();

        store.mark_run_cleaned(key(7, 1), EpilogOwed::Yes).unwrap();

        assert_eq!(
            store.load_run(key(7, 1)).unwrap().cleanup.epilog,
            HookState::Pending,
            "a hook already in flight when the run is first cleaned must stay \
             trackable, not be settled as if its owner were already gone"
        );
        assert!(matches!(
            store.settle_permit(key(7, 1)).unwrap(),
            SettlePermit::NotQuiescent
        ));
    }

    // Nothing re-runs a hook, so one whose owner the caller proves gone would
    // otherwise hold its slice for as long as the record survives.
    #[test]
    fn a_hook_left_in_flight_by_a_dead_agent_is_settled_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        store.mark_run_cleaned(key(7, 1), EpilogOwed::Yes).unwrap();
        store.admit_run(&run_with(8, 1, 1)).unwrap();
        store.mark_run_cleaned(key(8, 1), EpilogOwed::Yes).unwrap();
        store.record_epilog(key(8, 1), HookState::Running).unwrap();
        store.admit_run(&run_with(9, 1, 1)).unwrap();
        store.record_epilog(key(9, 1), HookState::Failed).unwrap();

        assert_eq!(
            store
                .settle_hooks_whose_owner_is_gone(no_supervisor_was_recorded)
                .unwrap(),
            2
        );

        for job_id in [7, 8] {
            assert_eq!(
                store.load_run(key(job_id, 1)).unwrap().cleanup.epilog,
                HookState::Unknown,
                "an owed hook nobody will run must not hold the slice forever"
            );
        }
        assert_eq!(
            store.load_run(key(9, 1)).unwrap().cleanup.epilog,
            HookState::Failed,
            "an outcome already recorded is evidence, not something to overwrite"
        );
    }

    // The sweep's whole safety now rests on the caller's proof, so a run it
    // cannot vouch for must come through untouched rather than settled.
    #[test]
    fn a_hook_the_caller_cannot_prove_is_ownerless_survives_the_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        store.mark_run_cleaned(key(7, 1), EpilogOwed::Yes).unwrap();

        assert_eq!(
            store.settle_hooks_whose_owner_is_gone(|_| false).unwrap(),
            0
        );

        assert_eq!(
            store.load_run(key(7, 1)).unwrap().cleanup.epilog,
            HookState::Pending,
            "a hook whose owner may still be running it keeps its slice"
        );
        assert!(matches!(
            store.settle_permit(key(7, 1)).unwrap(),
            SettlePermit::NotQuiescent
        ));
    }

    // A real hook outcome landing between the sweep's scan and its write must
    // survive, not be clobbered back to Unknown by the scan's stale decision.
    #[test]
    fn a_hook_that_settles_itself_during_the_sweep_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        store.mark_run_cleaned(key(7, 1), EpilogOwed::Yes).unwrap();

        let already_raced = std::cell::Cell::new(false);
        let settled = store
            .settle_hooks_whose_owner_is_gone(|_| {
                if !already_raced.get() {
                    already_raced.set(true);
                    store
                        .record_epilog(key(7, 1), HookState::Succeeded)
                        .unwrap();
                }
                true
            })
            .unwrap();

        assert_eq!(
            settled, 0,
            "the fresh recheck must see it is no longer in flight"
        );
        assert_eq!(
            store.load_run(key(7, 1)).unwrap().cleanup.epilog,
            HookState::Succeeded,
            "the real outcome must survive the sweep that raced it"
        );
    }

    // Settling licenses the release; it is not the release. A cut that drops the
    // entry here stops advertising a claim the node is still physically holding.
    #[test]
    fn settling_a_claim_does_not_take_it_out_of_the_cut() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        store.mark_run_cleaned(key(7, 1), EpilogOwed::No).unwrap();

        assert!(store.settle_acknowledged_run(key(7, 1)).unwrap());
        assert!(
            !store.load_run(key(7, 1)).unwrap().slice_released,
            "only the release itself may say the cores went back"
        );
        assert_eq!(store.ledger_cut("session-a").entries.len(), 1);

        store.record_slice_released(key(7, 1)).unwrap();
        assert!(store.ledger_cut("session-a").entries.is_empty());
    }

    // A NoSuchRun still releases the slice, holding it serves nothing -- but
    // it is not "committed", since it names no real Raft write.
    #[test]
    fn a_nosuchrun_releases_the_slice_but_is_not_committed() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        store.mark_run_cleaned(key(7, 1), EpilogOwed::No).unwrap();
        // Legacy settle path (NoSuchRun) — sets release_raft_index = Some(0).
        store.settle_acknowledged_run(key(7, 1)).unwrap();

        // A warrant IS issued (to free the slice), naming its real ground.
        let warrant = store
            .release_is_due(key(7, 1), spur_core::step::STEP_BATCH)
            .unwrap()
            .expect("a NoSuchRun answer licenses a release to free resources");
        assert!(matches!(
            warrant.ground(),
            spur_sched::cons_tres::ReleaseGround::SettledUnrecordedClaim
        ));
        // But it does NOT count as committed (no real Raft write).
        assert!(!store
            .load_run(key(7, 1))
            .unwrap()
            .controller_ack
            .is_committed());
    }

    // With a real Raft index, the release is licensed.
    #[test]
    fn a_real_raft_index_releases_the_slice() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        store.mark_run_cleaned(key(7, 1), EpilogOwed::No).unwrap();
        store
            .record_acknowledged_completion(key(7, 1), spur_core::step::STEP_BATCH, 42)
            .unwrap();

        let warrant = store
            .release_is_due(key(7, 1), spur_core::step::STEP_BATCH)
            .unwrap()
            .expect("a real Raft index licenses the release");
        assert!(matches!(
            warrant.ground(),
            spur_sched::cons_tres::ReleaseGround::Acknowledged(_)
        ));
    }

    #[test]
    fn a_relaunch_of_the_same_attempt_cannot_reset_the_cutoff() {
        // Otherwise the fence evaporates on any same-attempt redispatch and the
        // stale launch it was meant to refuse is admitted.
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        store.fence_run(key(7, 1), 5_000).unwrap();

        store.admit_run(&run_with(7, 1, 9_000)).unwrap();
        assert_eq!(store.reject_before(key(7, 1)), Some(5_000));
    }

    #[test]
    fn an_acknowledged_completion_clears_the_hold_it_resolved() {
        // A hold taken while the controller was unreachable would otherwise pin
        // the record forever: it never settles and never ages out.
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        store
            .take_conflict_hold(key(7, 1), "held with no tracked job")
            .unwrap();
        assert!(store.load_run(key(7, 1)).unwrap().conflict_hold.is_some());

        store
            .record_controller_ack(key(7, 1), STEP_BATCH, 9)
            .unwrap();
        assert!(store.load_run(key(7, 1)).unwrap().conflict_hold.is_none());
    }

    #[test]
    fn a_released_run_takes_no_conflict_hold() {
        // The unbacked set is a snapshot, so a release can land between reading
        // it and acting on it. The record must not then claim to hold a free core.
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        store
            .record_controller_ack(key(7, 1), STEP_BATCH, 9)
            .unwrap();

        assert_eq!(
            store
                .take_conflict_hold(key(7, 1), "held with no tracked job")
                .unwrap(),
            HoldOutcome::AlreadyReleased
        );
        assert!(store.load_run(key(7, 1)).unwrap().conflict_hold.is_none());
    }

    #[test]
    fn a_settled_and_released_run_takes_no_conflict_hold() {
        // Once settled and released, a stale unbacked-allocations snapshot
        // must not re-plant a hold nothing will ever clear.
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        assert!(store.settle_acknowledged_run(key(7, 1)).unwrap());
        assert!(store.record_slice_released(key(7, 1)).unwrap());

        let settled = store.load_run(key(7, 1)).unwrap();
        assert_eq!(settled.controller_ack.release_raft_index, Some(0));
        assert!(!settled.controller_ack.is_committed());
        assert!(settled.slice_released);

        assert_eq!(
            store
                .take_conflict_hold(key(7, 1), "stale unbacked allocation")
                .unwrap(),
            HoldOutcome::AlreadyReleased
        );
        assert!(store.load_run(key(7, 1)).unwrap().conflict_hold.is_none());
    }

    #[test]
    fn a_relaunch_cannot_drop_a_conflict_hold() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        store.take_conflict_hold(key(7, 1), "contested").unwrap();

        store.admit_run(&run_with(7, 1, 2)).unwrap();
        assert!(store.load_run(key(7, 1)).unwrap().conflict_hold.is_some());
    }

    #[test]
    fn a_ledger_cut_reports_every_held_run() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        store.admit_run(&run_with(8, 2, 1)).unwrap();
        store.take_conflict_hold(key(8, 2), "contested").unwrap();

        let cut = store.ledger_cut("session-a");
        assert_eq!(cut.agent_session_id, "session-a");
        assert!(cut.inventory_complete);
        assert_eq!(cut.entries.len(), 2);
        let held = cut.entries.iter().find(|e| e.job_id == 8).unwrap();
        assert_eq!(held.run_attempt, 2);
        assert!(held.conflict_hold);
        assert_eq!(held.allocation.cpu_ids, vec![0, 1]);
    }

    /// A cancel the cut does not register is re-issued on every later pass,
    /// forever, for a run that has already finished.
    #[test]
    fn a_cancelled_claim_leaves_the_cut_once_its_slice_goes_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        // The two ways a cancel lands: on a run still tearing down, and on one
        // the agent no longer tracks.
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        store.admit_run(&run_with(8, 1, 1)).unwrap();

        let mut cancels: BTreeMap<u32, u32> = BTreeMap::new();
        for pass in 0..5 {
            for entry in store.ledger_cut("session-a").entries {
                let entry_key = key(entry.job_id, entry.run_attempt);
                *cancels.entry(entry.job_id).or_default() += 1;
                // Job 7 is still tearing down on the first pass, so its cancel
                // can only be noted; the rest give the slice back outright.
                if entry.job_id == 7 && pass == 0 {
                    store.mark_controller_cancelled(entry_key).unwrap();
                    continue;
                }
                store.settle_acknowledged_run(entry_key).unwrap();
                // The pair the agent performs: settling licenses the release,
                // and only the release itself says the slice went back.
                store.record_slice_released(entry_key).unwrap();
            }
        }

        assert_eq!(
            cancels.get(&7).copied(),
            Some(2),
            "a claim still being torn down is cancelled again, then converges"
        );
        assert_eq!(cancels.get(&8).copied(), Some(1));
        assert!(
            store.ledger_cut("session-a").entries.is_empty(),
            "a slice given back is not a claim the controller should still see"
        );
    }

    #[test]
    fn a_decided_run_that_still_holds_its_slice_stays_in_the_cut() {
        // Hiding one is how a node came to report holding nothing while its
        // records still charged every core on it.
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        for job_id in [7, 8, 9] {
            store.admit_run(&run_with(job_id, 1, 1)).unwrap();
        }
        store.mark_run_cleaned(key(7, 1), EpilogOwed::No).unwrap();
        store.mark_controller_cancelled(key(8, 1)).unwrap();
        store
            .record_controller_ack(key(9, 1), STEP_BATCH, 42)
            .unwrap();

        assert_eq!(
            store.ledger_cut("session-a").entries.len(),
            3,
            "every lifecycle state above still holds the cores it names"
        );

        store.record_slice_released(key(8, 1)).unwrap();
        let left: Vec<u32> = store
            .ledger_cut("session-a")
            .entries
            .iter()
            .map(|entry| entry.job_id)
            .collect();
        assert_eq!(left, vec![7, 9]);
    }

    #[test]
    fn a_released_slice_does_not_discharge_the_report_still_owed_for_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1_000)).unwrap();
        let mut owed = ParticipantAdmission::new(7, 1, STEP_BATCH, "n1", Default::default());
        owed.final_report.required = true;
        store.admit_participant(&owed).unwrap();

        assert!(store.record_slice_released(key(7, 1)).unwrap());
        assert!(store.ledger_cut("session-a").entries.is_empty());
        assert_eq!(
            store.sweep(u64::MAX, 1, &HashSet::new()).unwrap(),
            0,
            "the report it owes outlives the cores it gave back"
        );
        assert!(store.load_run(key(7, 1)).is_ok());
    }

    #[test]
    fn releasing_a_slice_for_a_run_with_no_record_records_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!store(&dir).record_slice_released(key(9, 9)).unwrap());
    }

    #[test]
    fn a_runs_owed_reports_and_supervisors_are_readable_from_the_record_alone() {
        // The report pass runs off the ledger, so what it keys on has to be
        // there when the runtime session that produced it is long gone.
        let mut admitted = AdmittedRun {
            run: RunAdmission::new(7, 1, "n1", AdmittedResources::default(), 1),
            participants: vec![ParticipantAdmission::new(
                7,
                1,
                STEP_BATCH,
                "n1",
                Default::default(),
            )],
        };
        assert!(!admitted.owes_a_report());
        assert_eq!(admitted.recorded_supervisors().count(), 0);
        assert_eq!(admitted.lifecycle_step(), Some(STEP_BATCH));

        admitted.participants[0].final_report.required = true;
        admitted.participants[0].supervisor = Some(SupervisorRef {
            pid: 5,
            start_ticks: 9,
            boot_id: None,
        });
        assert!(admitted.owes_a_report());
        assert_eq!(admitted.recorded_supervisors().count(), 1);

        admitted.run.lifecycle_owner_step = Some(3);
        assert_eq!(
            admitted.lifecycle_step(),
            Some(3),
            "the owner outranks whichever participant happens to be first"
        );
    }

    #[test]
    fn an_unreadable_record_makes_the_cut_incomplete_not_empty() {
        // An empty ledger asserts "I hold nothing", which licenses the
        // controller to free everything it placed here.
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        let broken = store.prepare_run_dir(key(8, 1)).unwrap();
        publish_private(&broken, RUN_FILE, b"{ not json").unwrap();

        let cut = store.ledger_cut("session-a");
        assert!(
            !cut.inventory_complete,
            "the controller must not act on what is missing from a partial cut"
        );
        assert_eq!(cut.entries.len(), 1, "what could be read is still reported");
    }

    #[test]
    fn nothing_is_released_without_an_acknowledgement() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let mut run = run_with(7, 1, 1);
        run.lifecycle_owner_step = Some(STEP_BATCH);
        store.admit_run(&run).unwrap();

        assert!(
            store
                .release_is_due(key(7, 1), STEP_BATCH)
                .unwrap()
                .is_none(),
            "an exit is not a completion"
        );
        store
            .record_controller_ack(key(7, 1), STEP_BATCH, 42)
            .unwrap();
        assert!(store
            .release_is_due(key(7, 1), STEP_BATCH)
            .unwrap()
            .is_some());
    }

    // The acknowledgement and the cleaned state it settles share one write, so
    // every guard either of them had has to still bite on its own.
    #[test]
    fn an_acknowledged_completion_records_the_answer_and_the_cleanup_together() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let mut run = run_with(7, 1, 1);
        run.lifecycle_owner_step = Some(STEP_BATCH);
        store.admit_run(&run).unwrap();
        // A hold taken while the controller was unreachable. Nothing else ever
        // clears one, so a record that keeps it never settles and never ages out.
        store
            .take_conflict_hold(key(7, 1), "held with no tracked job")
            .unwrap();

        assert!(store
            .record_acknowledged_completion(key(7, 1), STEP_BATCH, 42)
            .unwrap());

        let settled = store.load_run(key(7, 1)).unwrap();
        assert_eq!(settled.controller_ack.release_raft_index, Some(42));
        assert_eq!(settled.state, RunState::Cleaned);
        assert!(settled.conflict_hold.is_none());
    }

    // A hook still running is the teardown's to settle. The answer is still the
    // controller's word, so losing it to the same write would strand the slice.
    #[test]
    fn an_acknowledged_completion_leaves_a_live_epilog_uncleaned_but_still_answered() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let mut run = run_with(7, 1, 1);
        run.lifecycle_owner_step = Some(STEP_BATCH);
        store.admit_run(&run).unwrap();
        store.record_epilog(key(7, 1), HookState::Running).unwrap();

        assert!(store
            .record_acknowledged_completion(key(7, 1), STEP_BATCH, 42)
            .unwrap());

        let settled = store.load_run(key(7, 1)).unwrap();
        assert_eq!(settled.controller_ack.release_raft_index, Some(42));
        assert_ne!(
            settled.state,
            RunState::Cleaned,
            "a run whose epilog is still running is not cleaned up"
        );
    }

    // A numbered step's own exit is not the controller's word that the run is
    // over, so neither half of the write may land on its say-so.
    #[test]
    fn an_acknowledged_completion_from_a_step_that_does_not_own_the_run_records_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let mut run = run_with(7, 1, 1);
        run.lifecycle_owner_step = Some(STEP_BATCH);
        store.admit_run(&run).unwrap();

        assert!(!store
            .record_acknowledged_completion(key(7, 1), 3, 42)
            .unwrap());

        let untouched = store.load_run(key(7, 1)).unwrap();
        assert_eq!(untouched.controller_ack.release_raft_index, None);
        assert_ne!(untouched.state, RunState::Cleaned);
    }

    #[test]
    fn only_the_owner_step_releases_the_runs_slice() {
        // Otherwise whichever participant is acknowledged first frees a slice
        // its siblings are still drawing on.
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let mut run = run_with(7, 1, 1);
        run.lifecycle_owner_step = Some(STEP_BATCH);
        store.admit_run(&run).unwrap();
        store
            .record_controller_ack(key(7, 1), STEP_BATCH, 42)
            .unwrap();

        assert!(store.release_is_due(key(7, 1), 3).unwrap().is_none());
        assert!(store
            .release_is_due(key(7, 1), STEP_BATCH)
            .unwrap()
            .is_some());
    }

    #[test]
    fn an_unowned_run_still_checks_the_lone_participants_identity() {
        // No lifecycle_owner_step set: only the single participant on record
        // may answer for the run, not any step_id a caller happens to pass.
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let run = run_with(7, 1, 1);
        store.admit_run(&run).unwrap();
        store
            .admit_participant(&ParticipantAdmission::new(
                7,
                1,
                STEP_BATCH,
                "n1",
                AdmittedResources::default(),
            ))
            .unwrap();
        store
            .record_controller_ack(key(7, 1), STEP_BATCH, 42)
            .unwrap();

        assert!(store.release_is_due(key(7, 1), 3).unwrap().is_none());
        assert!(store
            .release_is_due(key(7, 1), STEP_BATCH)
            .unwrap()
            .is_some());
    }

    #[test]
    fn an_epilog_still_running_holds_the_slice() {
        // The gate reads the record, so an outcome that never reaches it is a
        // gate that never bites -- which is how a live hook lost its guard.
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let mut run = run_with(7, 1, 1);
        run.lifecycle_owner_step = Some(STEP_BATCH);
        store.admit_run(&run).unwrap();
        store
            .record_controller_ack(key(7, 1), STEP_BATCH, 42)
            .unwrap();

        store.record_epilog(key(7, 1), HookState::Running).unwrap();
        assert!(store
            .release_is_due(key(7, 1), STEP_BATCH)
            .unwrap()
            .is_none());
        store
            .record_epilog(key(7, 1), HookState::Succeeded)
            .unwrap();
        assert!(store
            .release_is_due(key(7, 1), STEP_BATCH)
            .unwrap()
            .is_some());
    }

    #[test]
    fn a_settled_epilog_never_holds_the_slice_forever() {
        // Nothing ever re-runs a hook, so holding on either of these would be a
        // slice with no path back. A failed epilog drains the node instead.
        for settled in [HookState::Failed, HookState::Unknown, HookState::NotStarted] {
            let dir = tempfile::tempdir().unwrap();
            let store = store(&dir);
            let mut run = run_with(7, 1, 1);
            run.lifecycle_owner_step = Some(STEP_BATCH);
            store.admit_run(&run).unwrap();
            store
                .record_controller_ack(key(7, 1), STEP_BATCH, 42)
                .unwrap();
            store.record_epilog(key(7, 1), settled).unwrap();

            assert!(
                store
                    .release_is_due(key(7, 1), STEP_BATCH)
                    .unwrap()
                    .is_some(),
                "{settled:?} left the slice held"
            );
        }
    }

    #[test]
    fn a_recorded_epilog_outcome_survives_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();

        store.record_epilog(key(7, 1), HookState::Failed).unwrap();
        store.mark_run_cleaned(key(7, 1), EpilogOwed::No).unwrap();

        assert_eq!(
            store.load_run(key(7, 1)).unwrap().cleanup.epilog,
            HookState::Failed
        );
    }

    #[test]
    fn an_epilog_outcome_for_an_unknown_run_records_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!store(&dir)
            .record_epilog(key(9, 9), HookState::Failed)
            .unwrap());
    }

    #[test]
    fn a_run_with_no_record_releases_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(store(&dir)
            .release_is_due(key(9, 9), STEP_BATCH)
            .unwrap()
            .is_none());
    }

    #[test]
    fn acknowledging_a_report_stops_the_retry_rediscovering_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        let mut participant = ParticipantAdmission::new(7, 1, STEP_BATCH, "n1", Default::default());
        participant.final_report.required = true;
        store.admit_participant(&participant).unwrap();

        assert!(store
            .record_report_acknowledged(key(7, 1), STEP_BATCH)
            .unwrap());
        let (participants, _) = store.participants(key(7, 1)).unwrap();
        assert!(participants[0].final_report.acknowledged);
        assert_eq!(participants[0].lifecycle, ParticipantLifecycle::Exited);
    }

    #[test]
    fn a_live_supervisor_is_adopted() {
        assert_eq!(
            classify_run(&RunEvidence {
                supervisor_matches_a_live_process: true,
                boot_scope_of_supervisor: Some(BootScope::Same),
                ..Default::default()
            }),
            RunDisposition::Running
        );
    }

    #[test]
    fn a_reboot_outranks_a_matching_pid() {
        // Across a reboot the pid and tick pair can collide. Adopting it holds a
        // phantom and reports a dead job as running.
        assert_eq!(
            classify_run(&RunEvidence {
                supervisor_matches_a_live_process: true,
                boot_scope_of_supervisor: Some(BootScope::Different),
                ..Default::default()
            }),
            RunDisposition::DeadByReboot
        );
    }

    #[test]
    fn an_unknown_boot_still_adopts_a_matching_live_supervisor() {
        // A pre-upgrade record has no boot id. Reading that as a reboot would
        // declare running jobs dead.
        assert_eq!(
            classify_run(&RunEvidence {
                supervisor_matches_a_live_process: true,
                boot_scope_of_supervisor: Some(BootScope::Unknown),
                ..Default::default()
            }),
            RunDisposition::Running
        );
    }

    #[test]
    fn a_recorded_exit_settles_a_dead_supervisor() {
        assert_eq!(
            classify_run(&RunEvidence {
                boot_scope_of_supervisor: Some(BootScope::Same),
                recorded_exit: Some((3, 0)),
                ..Default::default()
            }),
            RunDisposition::SettledWithExit {
                exit_code: 3,
                signal: 0
            }
        );
    }

    #[test]
    fn a_dead_supervisor_with_no_exit_is_unknown_not_finished() {
        // Treating this as finished is the inference that frees a slice the
        // node may still be using.
        assert_eq!(
            classify_run(&RunEvidence {
                boot_scope_of_supervisor: Some(BootScope::Same),
                ..Default::default()
            }),
            RunDisposition::Unknown
        );
    }

    #[test]
    fn an_admitted_run_that_never_spawned_is_not_running_or_finished() {
        assert_eq!(
            classify_run(&RunEvidence {
                never_spawned: true,
                ..Default::default()
            }),
            RunDisposition::NeverStarted
        );
    }

    #[test]
    fn corruption_outranks_every_other_signal() {
        let corrupt = classify_run(&RunEvidence {
            supervisor_matches_a_live_process: true,
            recorded_exit: Some((0, 0)),
            residual_or_corrupt: true,
            ..Default::default()
        });
        assert_eq!(corrupt, RunDisposition::Corrupt);
        assert!(corrupt.needs_reconciliation());
    }

    #[test]
    fn an_absent_boot_id_is_never_read_as_a_reboot() {
        // Inverting this declares live supervisors dead and releases the
        // allocations of running jobs.
        let no_record = SupervisorRef::default();
        assert_eq!(no_record.boot_scope(Some("abc")), BootScope::Unknown);

        let recorded = SupervisorRef {
            pid: 5,
            start_ticks: 9,
            boot_id: Some("abc".into()),
        };
        assert_eq!(recorded.boot_scope(None), BootScope::Unknown);
        assert_eq!(recorded.boot_scope(Some("abc")), BootScope::Same);
        assert_eq!(recorded.boot_scope(Some("def")), BootScope::Different);
    }

    // A stale or replayed registration arriving after the participant moved
    // on must be a no-op, not a resurrection back to Running.
    #[test]
    fn a_stale_supervisor_registration_does_not_resurrect_a_finished_participant() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        let mut exited = ParticipantAdmission::new(7, 1, STEP_BATCH, "n1", Default::default());
        exited.lifecycle = ParticipantLifecycle::Exited;
        store.admit_participant(&exited).unwrap();

        let resurrecting = SupervisorRef {
            pid: 12345,
            start_ticks: 1,
            boot_id: current_boot_id(),
        };
        assert!(!store
            .record_supervisor(key(7, 1), STEP_BATCH, resurrecting)
            .unwrap());

        let (participants, _) = store.participants(key(7, 1)).unwrap();
        assert_eq!(
            participants[0].lifecycle,
            ParticipantLifecycle::Exited,
            "a stale registration must not resurrect a finished participant"
        );
        assert!(participants[0].supervisor.is_none());
    }

    #[test]
    fn a_fresh_supervisor_registration_still_adopts_a_just_admitted_participant() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        store
            .admit_participant(&ParticipantAdmission::new(
                7,
                1,
                STEP_BATCH,
                "n1",
                Default::default(),
            ))
            .unwrap();

        assert!(store
            .record_supervisor(
                key(7, 1),
                STEP_BATCH,
                SupervisorRef {
                    pid: 999,
                    start_ticks: 1,
                    boot_id: current_boot_id(),
                }
            )
            .unwrap());

        let (participants, _) = store.participants(key(7, 1)).unwrap();
        assert_eq!(participants[0].lifecycle, ParticipantLifecycle::Running);
        assert!(participants[0].supervisor.is_some());
    }

    // A relaunch is always constructed fresh; without carrying these forward,
    // a stale one arriving after the controller ends a run un-decides it.
    #[test]
    fn a_relaunch_cannot_un_cancel_un_commit_or_un_release_a_finished_run() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        store.mark_run_cleaned(key(7, 1), EpilogOwed::No).unwrap();
        store.mark_controller_cancelled(key(7, 1)).unwrap();
        store
            .record_controller_ack(key(7, 1), STEP_BATCH, 42)
            .unwrap();
        store.record_slice_released(key(7, 1)).unwrap();

        // A stale relaunch, built the same way a real one is: fresh defaults.
        store.admit_run(&run_with(7, 1, 9_000)).unwrap();

        let after = store.load_run(key(7, 1)).unwrap();
        assert_eq!(after.state, RunState::Cleaned, "must stay cleaned");
        assert!(after.cancelled_by_controller, "must stay cancelled");
        assert!(after.slice_released, "must stay released");
        assert_eq!(
            after.controller_ack.release_raft_index,
            Some(42),
            "must keep the real commit"
        );
    }

    // Same reasoning as above: settle_permit trusts cleanup.epilog to gate release,
    // so resetting it to NotStarted would release while the epilog is still running.
    #[test]
    fn a_relaunch_cannot_erase_a_real_in_flight_epilog_debt() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        store.mark_run_cleaned(key(7, 1), EpilogOwed::Yes).unwrap();
        assert_eq!(
            store.load_run(key(7, 1)).unwrap().cleanup.epilog,
            HookState::Pending
        );

        // A stale relaunch, built the same way a real one is: fresh defaults.
        store.admit_run(&run_with(7, 1, 9_000)).unwrap();

        assert_eq!(
            store.load_run(key(7, 1)).unwrap().cleanup.epilog,
            HookState::Pending,
            "must not erase a real in-flight epilog debt"
        );
    }

    // fence_run's own read-check-write is the case B4 names: two racing
    // callers must not let the lower cutoff's writer clobber the higher one.
    #[test]
    fn concurrent_fence_run_calls_on_one_key_never_lose_the_higher_cutoff() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let run_key = key(7, 1);
        store.admit_run(&run_with(7, 1, 1)).unwrap();

        for round in 0..200u64 {
            let low = 1_000 + round * 10;
            let high = low + 5;
            let barrier = std::sync::Barrier::new(2);
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    barrier.wait();
                    store.fence_run(run_key, low).unwrap();
                });
                scope.spawn(|| {
                    barrier.wait();
                    store.fence_run(run_key, high).unwrap();
                });
            });
            assert_eq!(
                store.reject_before(run_key),
                Some(high),
                "round {round}: the lower fence's writer must never win the race"
            );
        }
    }

    // Every mutator goes through with_run_lock, which must prune its map entry
    // once unreferenced, or a long-lived agent's lock map only grows.
    #[test]
    fn with_run_lock_prunes_its_entry_once_uncontended() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        for i in 0..50u32 {
            store.admit_run(&run_with(i, 1, 1)).unwrap();
            store.fence_run(key(i, 1), 2).unwrap();
        }
        assert_eq!(
            store.run_locks.lock().unwrap().len(),
            0,
            "no run's lock should still be held once every mutator call has returned"
        );
    }

    // A race that leaves the owner unset must not let whichever sibling acks
    // first free a slice the other is still drawing on.
    #[test]
    fn an_unset_owner_with_a_sibling_present_answers_for_neither_step() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        store
            .admit_participant(&ParticipantAdmission::new(
                7,
                1,
                STEP_BATCH,
                "n1",
                Default::default(),
            ))
            .unwrap();
        store
            .admit_participant(&ParticipantAdmission::new(
                7,
                1,
                3,
                "n1",
                Default::default(),
            ))
            .unwrap();

        assert!(!store
            .record_controller_ack(key(7, 1), STEP_BATCH, 42)
            .unwrap());
        assert!(store
            .release_is_due(key(7, 1), STEP_BATCH)
            .unwrap()
            .is_none());
        assert!(store.release_is_due(key(7, 1), 3).unwrap().is_none());
    }

    #[test]
    fn an_unset_owner_with_no_sibling_still_answers_for_its_only_participant() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.admit_run(&run_with(7, 1, 1)).unwrap();
        store
            .admit_participant(&ParticipantAdmission::new(
                7,
                1,
                STEP_BATCH,
                "n1",
                Default::default(),
            ))
            .unwrap();

        assert!(store
            .record_controller_ack(key(7, 1), STEP_BATCH, 42)
            .unwrap());
        assert!(store
            .release_is_due(key(7, 1), STEP_BATCH)
            .unwrap()
            .is_some());
    }

    #[test]
    fn a_stamped_empty_digest_does_not_bypass_a_conflicting_recorded_one() {
        let fences = LaunchFences {
            issued_at_unix_ms: 100,
            command_digest: String::new(),
            ..Default::default()
        };
        assert_eq!(
            fences.check(200, 0, Some("aaa")),
            Some(LaunchRefusal::ConflictingDigest),
            "a stamped launch's empty digest must not read as an automatic repeat"
        );
    }

    #[test]
    fn two_empty_digests_on_a_stamped_launch_are_still_an_idempotent_repeat() {
        let fences = LaunchFences {
            issued_at_unix_ms: 100,
            command_digest: String::new(),
            ..Default::default()
        };
        assert_eq!(fences.check(200, 0, Some("")), None);
    }

    #[test]
    fn a_hook_running_when_its_owner_died_settles_as_unknown() {
        for in_flight in [HookState::Pending, HookState::Running] {
            assert_eq!(in_flight.settled_after_owner_loss(), HookState::Unknown);
            assert!(in_flight.is_in_flight());
        }
        for settled in [
            HookState::NotStarted,
            HookState::Succeeded,
            HookState::Failed,
            HookState::Unknown,
        ] {
            assert_eq!(settled.settled_after_owner_loss(), settled);
            assert!(!settled.is_in_flight());
        }
    }

    /// Frozen at the first shipped shape. Never regenerate it: a field added
    /// without `#[serde(default)]` must fail here rather than on a live node.
    const FROZEN_RUN_V1: &str = r#"{
        "schema_version": 1,
        "job_id": 42,
        "run_attempt": 3,
        "node": "n1",
        "allocation": {"cpu_ids": [0,1], "memory_mb": 2048, "gpu_devices": [2]},
        "state": "running",
        "created_at_unix_ms": 1700000000000,
        "reject_before_unix_ms": 0,
        "max_launch_expiry_unix_ms": 1700000120000,
        "prolog": "succeeded",
        "lifecycle_owner_step": 4294967294,
        "cleanup": {"phase": "not_started", "epilog": "not_started"},
        "conflict_hold": null,
        "controller_ack": {"release_raft_index": null}
    }"#;

    const FROZEN_PARTICIPANT_V1: &str = r#"{
        "schema_version": 1,
        "job_id": 42,
        "run_attempt": 3,
        "step_id": 4294967294,
        "node": "n1",
        "command_digest": "sha256:abc",
        "allocation_subset": {"cpu_ids": [0], "memory_mb": 1024, "gpu_devices": []},
        "issued_at_unix_ms": 1700000000000,
        "expires_at_unix_ms": 1700000120000,
        "lifecycle": "running",
        "supervisor": {"pid": 991, "start_ticks": 7788, "boot_id": "b-1"},
        "final_report": {"required": true, "acknowledged": false}
    }"#;

    #[test]
    fn frozen_records_still_load() {
        let run: RunAdmission = serde_json::from_str(FROZEN_RUN_V1).unwrap();
        assert_eq!(run.job_id, 42);
        assert_eq!(
            run.state,
            RunState::Admitted,
            "a legacy in-flight state is not settled"
        );
        assert_eq!(run.allocation.gpu_devices, vec![2]);

        let participant: ParticipantAdmission =
            serde_json::from_str(FROZEN_PARTICIPANT_V1).unwrap();
        assert_eq!(participant.lifecycle, ParticipantLifecycle::Running);
        assert_eq!(
            participant.supervisor.unwrap().boot_id.as_deref(),
            Some("b-1")
        );
        assert!(participant.final_report.required);
    }

    /// The minimum a pre-upgrade or partially-written record can carry. Every
    /// optional field must default rather than fail the load.
    #[test]
    fn a_record_with_only_required_fields_loads() {
        let run: RunAdmission =
            serde_json::from_str(r#"{"schema_version":1,"job_id":1,"run_attempt":0,"node":"n1"}"#)
                .unwrap();
        assert_eq!(run.state, RunState::Admitted);
        assert!(run.conflict_hold.is_none());

        let participant: ParticipantAdmission = serde_json::from_str(
            r#"{"schema_version":1,"job_id":1,"run_attempt":0,"step_id":1,"node":"n1"}"#,
        )
        .unwrap();
        assert_eq!(participant.lifecycle, ParticipantLifecycle::Admitted);
        assert!(participant.supervisor.is_none());
    }
}
