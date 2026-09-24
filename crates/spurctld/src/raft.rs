// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Raft-based consensus for spurctld.
//!
//! Raft is always-on: even single-node deployments run a 1-member Raft
//! cluster that self-elects instantly.  The Raft log is the sole durable
//! store — entries are `WalOperation` values proposed via
//! `ClusterManager::propose()` and applied through `StateMachineApply`.

use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use openraft::error::Fatal;
use openraft::storage::{LogState, RaftLogReader, Snapshot, SnapshotMeta};
use openraft::{
    BasicNode, Config, Entry, EntryPayload, LogId, Raft, RaftSnapshotBuilder, StorageError,
    StoredMembership, Vote,
};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use spur_core::end_reason::{CancelProvenance, EndReason};
use spur_core::job::{Job, JobState};
use spur_core::wal::WalOperation;

pub type NodeId = u64;

openraft::declare_raft_types!(
    pub SpurTypeConfig:
        D = WalOperation,
        R = ClientResponse,
        Node = BasicNode,
);

pub type SpurRaft = Raft<SpurTypeConfig>;

/// Maximum size of a single Raft gRPC message (request or response), in bytes.
///
/// tonic defaults both encode and decode limits to 4 MiB. Snapshots ship in
/// chunks of `RAFT_SNAPSHOT_MAX_CHUNK_SIZE` raw bytes, and `serde_json` inflates
/// the chunk's `Vec<u8>` payload roughly 3.4x (each byte becomes a decimal
/// integer plus a comma), so the largest message we ever put on the wire is
/// about `3.4 * RAFT_SNAPSHOT_MAX_CHUNK_SIZE`. This limit keeps large headroom
/// over that bound. Invariant: `4 * RAFT_SNAPSHOT_MAX_CHUNK_SIZE < RAFT_MAX_MESSAGE_SIZE`.
pub const RAFT_MAX_MESSAGE_SIZE: usize = 32 * 1024 * 1024;

/// Raw bytes per `install_snapshot` chunk. Bounded so the JSON-encoded message
/// stays well under `RAFT_MAX_MESSAGE_SIZE` (see the invariant above). A larger
/// snapshot simply ships in more chunks.
pub const RAFT_SNAPSHOT_MAX_CHUNK_SIZE: u64 = 1024 * 1024;

/// Build a Raft gRPC client with message-size limits raised to
/// `RAFT_MAX_MESSAGE_SIZE`. Used by both the live network path and tests so the
/// exact transport configuration is covered.
pub(crate) fn raft_client(
    channel: tonic::transport::Channel,
) -> spur_proto::raft_proto::raft_internal_client::RaftInternalClient<tonic::transport::Channel> {
    spur_proto::raft_proto::raft_internal_client::RaftInternalClient::new(channel)
        .max_decoding_message_size(RAFT_MAX_MESSAGE_SIZE)
        .max_encoding_message_size(RAFT_MAX_MESSAGE_SIZE)
}

/// Set when a committed WAL entry transitions a job to a terminal state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobFinalized {
    pub job_id: u32,
    pub state: JobState,
    pub exit_code: i32,
    /// Computed in apply, so a later entry cannot change it before the
    /// accounting write reads it.
    #[serde(skip)]
    pub reason: EndReason,
    #[serde(skip)]
    pub reason_detail: Option<String>,
    #[serde(skip)]
    pub ended_by: Option<CancelProvenance>,
}

impl JobFinalized {
    pub fn for_job(job: &Job, state: JobState, exit_code: i32) -> Self {
        let (reason, reason_detail) = job.end_reason(state);
        Self {
            job_id: job.job_id,
            state,
            exit_code,
            reason,
            reason_detail,
            ended_by: job.ended_by(reason).cloned(),
        }
    }
}

/// Response returned after a Raft write is committed.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClientResponse {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs_finalized: Vec<JobFinalized>,
    #[serde(default)]
    pub reservation_created: bool,
    #[serde(default)]
    pub partition_created: bool,
    /// False when a `JobStart` apply hit the duplicate-reservation backstop
    /// instead of actually charging the placement.
    #[serde(default)]
    pub job_started: bool,
    /// False when a `JobStateChange` carrying `expected_run_attempt` was
    /// refused as stale instead of actually transitioning the job.
    #[serde(default)]
    pub state_change_applied: bool,
}

/// Trait for applying committed Raft entries to the cluster state.
/// Implemented by ClusterManager to avoid a circular dependency with SpurStore.
pub trait StateMachineApply: Send + Sync {
    fn apply_operation(&self, op: &WalOperation) -> ClientResponse;
    fn snapshot_state(&self) -> Result<Vec<u8>, anyhow::Error>;
    fn restore_from_snapshot(&self, data: &[u8]) -> Result<(), anyhow::Error>;
}

/// Disk-backed Raft storage.
/// Layout: `{state_dir}/raft/{vote.json, log/*.json, snapshot.json}`
pub struct SpurStore {
    inner: RwLock<StoreInner>,
    /// Served from memory so a reader never races an on-disk rewrite.
    current_snapshot: RwLock<Option<Arc<PersistedSnapshot>>>,
    /// Serializes snapshot writes so an older one cannot overwrite a newer one.
    persist_lock: Mutex<()>,
    raft_dir: PathBuf,
    applier: Arc<dyn StateMachineApply>,
}

impl std::fmt::Debug for SpurStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpurStore")
            .field("raft_dir", &self.raft_dir)
            .finish()
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreInner {
    vote: Option<Vote<NodeId>>,
    committed: Option<LogId<NodeId>>,
    last_purged: Option<LogId<NodeId>>,
    log: BTreeMap<u64, Entry<SpurTypeConfig>>,
    last_applied: Option<LogId<NodeId>>,
    last_membership: StoredMembership<NodeId, BasicNode>,
    applied_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedSnapshot {
    meta: SnapshotMeta<NodeId, BasicNode>,
    data: Vec<u8>,
}

/// Borrowed mirror of [`PersistedSnapshot`] so large snapshots serialize without
/// a clone; field names and order match, so the JSON is identical.
#[derive(Serialize)]
struct PersistedSnapshotRef<'a> {
    meta: &'a SnapshotMeta<NodeId, BasicNode>,
    data: &'a [u8],
}

impl SpurStore {
    /// Recover storage with `strict = true`: any unreadable/undeserializable vote,
    /// log entry, or snapshot is a hard error. This is the safe default — see
    /// `new_with_recovery_mode` for why silently skipping such records is
    /// dangerous and should only ever be an explicit, deliberate choice.
    pub fn new(state_dir: &Path, applier: Arc<dyn StateMachineApply>) -> anyhow::Result<Self> {
        Self::new_with_recovery_mode(state_dir, applier, true)
    }

    /// Recover storage from `{state_dir}/raft/`.
    ///
    /// `strict` controls what happens when a vote, log entry, or snapshot record
    /// exists but cannot be deserialized (disk corruption, or a restart with a
    /// `spurctld` build whose `WalOperation`/state schema has drifted from
    /// whatever wrote the record). These are records of already-committed
    /// cluster state — nodes, jobs, reservations — so losing one is a
    /// correctness violation, not a cosmetic issue.
    ///
    /// `strict = true` (default, via `new`) refuses to start: the affected
    /// record is named in the error so an operator can roll back to a
    /// compatible binary. `strict = false` preserves the old behavior of
    /// logging and skipping the record, for deliberate forensic recovery
    /// when the loss has already been assessed as acceptable.
    pub fn new_with_recovery_mode(
        state_dir: &Path,
        applier: Arc<dyn StateMachineApply>,
        strict: bool,
    ) -> anyhow::Result<Self> {
        let raft_dir = state_dir.join("raft");
        let log_dir = raft_dir.join("log");
        std::fs::create_dir_all(&log_dir)?;

        // Real files are only ever renamed-into targets, so a leftover `*.tmp`
        // is a partial write and safe to drop.
        remove_stale_temp_files(&raft_dir);

        let mut inner = StoreInner::default();
        let mut skipped_records = 0u64;
        let mut recovered_snapshot: Option<Arc<PersistedSnapshot>> = None;

        let vote_path = raft_dir.join("vote.json");
        if vote_path.exists() {
            match std::fs::read_to_string(&vote_path) {
                Ok(data) => match serde_json::from_str(&data) {
                    Ok(v) => inner.vote = Some(v),
                    Err(e) => {
                        if strict {
                            anyhow::bail!("failed to parse vote.json: {e}");
                        }
                        warn!("failed to parse vote.json: {e}");
                        skipped_records += 1;
                    }
                },
                Err(e) => {
                    if strict {
                        anyhow::bail!("failed to read vote.json: {e}");
                    }
                    warn!("failed to read vote.json: {e}");
                    skipped_records += 1;
                }
            }
        }

        if let Ok(entries) = std::fs::read_dir(&log_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "json") {
                    match std::fs::read_to_string(&path) {
                        Ok(data) => match serde_json::from_str::<Entry<SpurTypeConfig>>(&data) {
                            Ok(e) => {
                                inner.log.insert(e.log_id.index, e);
                            }
                            Err(e) => {
                                if strict {
                                    anyhow::bail!("failed to parse log entry {path:?}: {e}");
                                }
                                warn!("failed to parse log entry {:?}: {e}", path);
                                skipped_records += 1;
                            }
                        },
                        Err(e) => {
                            if strict {
                                anyhow::bail!("failed to read log entry {path:?}: {e}");
                            }
                            warn!("failed to read log entry {:?}: {e}", path);
                            skipped_records += 1;
                        }
                    }
                }
            }
        }

        let snap_path = raft_dir.join("snapshot.json");
        if snap_path.exists() {
            match load_snapshot(&snap_path, applier.as_ref()) {
                Ok(ps) => {
                    inner.last_applied = ps.meta.last_log_id;
                    inner.last_membership = ps.meta.last_membership.clone();
                    recovered_snapshot = Some(Arc::new(ps));
                }
                Err(e) if strict => return Err(e),
                Err(e) => {
                    warn!("{e}");
                    skipped_records += 1;
                    quarantine_snapshot(&snap_path);
                }
            }
        }

        let purged_path = raft_dir.join("purged.json");
        if purged_path.exists() {
            // Always hard-fail: a corrupt purged.json cannot be recovered safely
            // even in lenient mode, since it would silently un-purge history.
            let data = std::fs::read_to_string(&purged_path)?;
            inner.last_purged = Some(
                serde_json::from_str::<LogId<NodeId>>(&data)
                    .map_err(|e| anyhow::anyhow!("failed to parse purged.json: {e}"))?,
            );
        }

        info!(
            log_entries = inner.log.len(),
            vote = ?inner.vote,
            "raft store recovered from disk"
        );
        if skipped_records > 0 {
            tracing::error!(
                skipped_records,
                "raft store recovered with GAPS: {skipped_records} record(s) could not be \
                 deserialized and were dropped (lenient recovery mode) — cluster state may be \
                 missing nodes, jobs, or other committed changes"
            );
        }

        Ok(Self {
            inner: RwLock::new(inner),
            current_snapshot: RwLock::new(recovered_snapshot),
            persist_lock: Mutex::new(()),
            raft_dir,
            applier,
        })
    }

    fn persist_last_purged(&self, log_id: &LogId<NodeId>) -> Result<(), std::io::Error> {
        let path = self.raft_dir.join("purged.json");
        let data = serde_json::to_vec(log_id)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        write_atomic(&path, &data)
    }

    fn persist_vote(&self, vote: &Vote<NodeId>) -> Result<(), std::io::Error> {
        let path = self.raft_dir.join("vote.json");
        let data = serde_json::to_vec(vote)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        write_atomic(&path, &data)
    }

    fn persist_log_entry(&self, entry: &Entry<SpurTypeConfig>) -> Result<(), std::io::Error> {
        let path = self
            .raft_dir
            .join("log")
            .join(format!("{:020}.json", entry.log_id.index));
        let data = serde_json::to_vec(entry)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(&path, &data)
            .map_err(|e| std::io::Error::new(e.kind(), format!("{path:?}: {e}")))
    }

    fn remove_log_entry(&self, index: u64) {
        let path = self
            .raft_dir
            .join("log")
            .join(format!("{:020}.json", index));
        let _ = std::fs::remove_file(&path);
    }

    /// Persist and cache a snapshot unless a newer one is already current (a
    /// build task can race a leader install). Returns the snapshot now current.
    fn persist_and_cache_snapshot(
        &self,
        meta: SnapshotMeta<NodeId, BasicNode>,
        data: Vec<u8>,
    ) -> Result<Arc<PersistedSnapshot>, std::io::Error> {
        let _guard = self.persist_lock.lock();

        if let Some(existing) = self.current_snapshot.read().as_ref() {
            if existing.meta.last_log_id >= meta.last_log_id {
                return Ok(existing.clone());
            }
        }

        let path = self.raft_dir.join("snapshot.json");
        let json = serde_json::to_vec(&PersistedSnapshotRef {
            meta: &meta,
            data: &data,
        })
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        write_atomic(&path, &json)?;
        drop(json);

        let ps = Arc::new(PersistedSnapshot { meta, data });
        *self.current_snapshot.write() = Some(ps.clone());
        Ok(ps)
    }
}

fn load_snapshot(
    path: &Path,
    applier: &dyn StateMachineApply,
) -> anyhow::Result<PersistedSnapshot> {
    let json = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("failed to read snapshot.json: {e}"))?;
    let ps: PersistedSnapshot = serde_json::from_str(&json)
        .map_err(|e| anyhow::anyhow!("failed to parse snapshot.json: {e}"))?;
    drop(json);
    applier
        .restore_from_snapshot(&ps.data)
        .map_err(|e| anyhow::anyhow!("failed to restore state from snapshot.json: {e}"))?;
    Ok(ps)
}

/// Move an unrestorable snapshot aside: with no cache, openraft rebuilds an
/// empty snapshot over `snapshot.json`, which would destroy the only copy.
fn quarantine_snapshot(path: &Path) {
    let aside = path.with_extension("json.unrestorable");
    match std::fs::rename(path, &aside) {
        Ok(()) => warn!("moved unrestorable snapshot to {aside:?}"),
        Err(e) => warn!("failed to move unrestorable snapshot aside: {e}"),
    }
}

/// Fill `<path>.tmp`, fsync, rename over `path`, then fsync the directory so a
/// reader only ever sees a complete file and the rename survives a crash.
fn write_atomic(path: &Path, data: &[u8]) -> Result<(), std::io::Error> {
    let mut tmp_os = path.as_os_str().to_os_string();
    tmp_os.push(".tmp");
    let tmp = PathBuf::from(tmp_os);

    if let Err(e) = write_tmp_and_rename(&tmp, path, data) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    // Propagate: an unpersisted rename means a crash can lose a write reported durable.
    if let Some(dir) = path.parent() {
        let d = std::fs::File::open(dir)
            .map_err(|e| std::io::Error::new(e.kind(), format!("{dir:?}: {e}")))?;
        d.sync_all()
            .map_err(|e| std::io::Error::new(e.kind(), format!("{dir:?}: {e}")))?;
    }
    Ok(())
}

fn write_tmp_and_rename(tmp: &Path, path: &Path, data: &[u8]) -> Result<(), std::io::Error> {
    use std::io::Write;

    let mut f = std::fs::File::create(tmp)
        .map_err(|e| std::io::Error::new(e.kind(), format!("{tmp:?}: {e}")))?;
    f.write_all(data)
        .map_err(|e| std::io::Error::new(e.kind(), format!("{tmp:?}: {e}")))?;
    f.sync_all()
        .map_err(|e| std::io::Error::new(e.kind(), format!("{tmp:?}: {e}")))?;
    drop(f);

    std::fs::rename(tmp, path).map_err(|e| std::io::Error::new(e.kind(), format!("{path:?}: {e}")))
}

/// Drop any leftover `*.tmp` in the raft dir (an interrupted [`write_atomic`]).
fn remove_stale_temp_files(raft_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(raft_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "tmp") {
            let _ = std::fs::remove_file(&path);
        }
    }
}

impl RaftLogReader<SpurTypeConfig> for Arc<SpurStore> {
    async fn try_get_log_entries<RB: std::ops::RangeBounds<u64> + Clone + std::fmt::Debug>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<SpurTypeConfig>>, StorageError<NodeId>> {
        let inner = self.inner.read();
        Ok(inner.log.range(range).map(|(_, e)| e.clone()).collect())
    }
}

impl RaftSnapshotBuilder<SpurTypeConfig> for Arc<SpurStore> {
    async fn build_snapshot(&mut self) -> Result<Snapshot<SpurTypeConfig>, StorageError<NodeId>> {
        // Capture state and meta together under `inner` (apply also takes it, so
        // they agree), then release it before the slow disk write.
        let (snapshot_data, meta) = {
            let inner = self.inner.read();

            let snapshot_data = self.applier.snapshot_state().map_err(|e| {
                StorageError::from_io_error(
                    openraft::ErrorSubject::Store,
                    openraft::ErrorVerb::Read,
                    std::io::Error::other(e),
                )
            })?;

            let snap_id = format!(
                "{}-{}",
                inner.last_applied.map(|l| l.index).unwrap_or(0),
                inner.applied_count
            );

            let meta = SnapshotMeta {
                last_log_id: inner.last_applied,
                last_membership: inner.last_membership.clone(),
                snapshot_id: snap_id,
            };
            (snapshot_data, meta)
        };

        let ps = self
            .persist_and_cache_snapshot(meta, snapshot_data)
            .map_err(|e| {
                StorageError::from_io_error(
                    openraft::ErrorSubject::Store,
                    openraft::ErrorVerb::Write,
                    e,
                )
            })?;

        // The trait requires the bytes even though openraft keeps only the meta.
        Ok(Snapshot {
            meta: ps.meta.clone(),
            snapshot: Box::new(Cursor::new(ps.data.clone())),
        })
    }
}

impl openraft::RaftStorage<SpurTypeConfig> for Arc<SpurStore> {
    type LogReader = Self;
    type SnapshotBuilder = Self;

    async fn get_log_state(&mut self) -> Result<LogState<SpurTypeConfig>, StorageError<NodeId>> {
        let inner = self.inner.read();
        let last = inner.log.iter().next_back().map(|(_, e)| e.log_id);
        Ok(LogState {
            last_purged_log_id: inner.last_purged,
            last_log_id: last,
        })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn save_vote(&mut self, vote: &Vote<NodeId>) -> Result<(), StorageError<NodeId>> {
        self.persist_vote(vote).map_err(|e| {
            StorageError::from_io_error(openraft::ErrorSubject::Vote, openraft::ErrorVerb::Write, e)
        })?;
        self.inner.write().vote = Some(*vote);
        Ok(())
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<NodeId>>, StorageError<NodeId>> {
        Ok(self.inner.read().vote)
    }

    async fn append_to_log<I: IntoIterator<Item = Entry<SpurTypeConfig>> + Send>(
        &mut self,
        entries: I,
    ) -> Result<(), StorageError<NodeId>> {
        let mut inner = self.inner.write();
        for entry in entries {
            self.persist_log_entry(&entry).map_err(|e| {
                StorageError::from_io_error(
                    openraft::ErrorSubject::Logs,
                    openraft::ErrorVerb::Write,
                    e,
                )
            })?;
            inner.log.insert(entry.log_id.index, entry);
        }
        Ok(())
    }

    async fn delete_conflict_logs_since(
        &mut self,
        log_id: LogId<NodeId>,
    ) -> Result<(), StorageError<NodeId>> {
        let mut inner = self.inner.write();
        let keys: Vec<_> = inner.log.range(log_id.index..).map(|(k, _)| *k).collect();
        for k in keys {
            inner.log.remove(&k);
            self.remove_log_entry(k);
        }
        Ok(())
    }

    async fn purge_logs_upto(&mut self, log_id: LogId<NodeId>) -> Result<(), StorageError<NodeId>> {
        self.persist_last_purged(&log_id).map_err(|e| {
            StorageError::from_io_error(openraft::ErrorSubject::Logs, openraft::ErrorVerb::Write, e)
        })?;
        let mut inner = self.inner.write();
        let keys: Vec<_> = inner.log.range(..=log_id.index).map(|(k, _)| *k).collect();
        for k in keys {
            inner.log.remove(&k);
            self.remove_log_entry(k);
        }
        inner.last_purged = Some(log_id);
        Ok(())
    }

    async fn last_applied_state(
        &mut self,
    ) -> Result<(Option<LogId<NodeId>>, StoredMembership<NodeId, BasicNode>), StorageError<NodeId>>
    {
        let inner = self.inner.read();
        Ok((inner.last_applied, inner.last_membership.clone()))
    }

    async fn apply_to_state_machine(
        &mut self,
        entries: &[Entry<SpurTypeConfig>],
    ) -> Result<Vec<ClientResponse>, StorageError<NodeId>> {
        let mut inner = self.inner.write();
        let mut results = Vec::new();

        for entry in entries {
            inner.last_applied = Some(entry.log_id);
            match &entry.payload {
                EntryPayload::Normal(op) => {
                    debug!(index = entry.log_id.index, "raft: applying WalOperation");
                    inner.applied_count += 1;
                    results.push(self.applier.apply_operation(op));
                }
                EntryPayload::Membership(mem) => {
                    inner.last_membership = StoredMembership::new(Some(entry.log_id), mem.clone());
                    results.push(ClientResponse::default());
                }
                EntryPayload::Blank => {
                    results.push(ClientResponse::default());
                }
            }
        }
        Ok(results)
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.clone()
    }

    async fn begin_receiving_snapshot(
        &mut self,
    ) -> Result<Box<Cursor<Vec<u8>>>, StorageError<NodeId>> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta<NodeId, BasicNode>,
        snapshot: Box<Cursor<Vec<u8>>>,
    ) -> Result<(), StorageError<NodeId>> {
        let data = snapshot.into_inner();

        // No lenient mode for a live leader snapshot: reject what we can't apply,
        // and restore before persisting so a bad one never reaches disk.
        {
            // One guard so a concurrent build never pairs restored state with the old index.
            let mut inner = self.inner.write();
            self.applier.restore_from_snapshot(&data).map_err(|e| {
                StorageError::from_io_error(
                    openraft::ErrorSubject::Store,
                    openraft::ErrorVerb::Write,
                    std::io::Error::other(e),
                )
            })?;
            inner.last_applied = meta.last_log_id;
            inner.last_membership = meta.last_membership.clone();
        }

        let ps = self
            .persist_and_cache_snapshot(meta.clone(), data)
            .map_err(|e| {
                StorageError::from_io_error(
                    openraft::ErrorSubject::Store,
                    openraft::ErrorVerb::Write,
                    e,
                )
            })?;
        // A stale-guard hit would leave the restored state ahead of the cache.
        debug_assert_eq!(ps.meta.last_log_id, meta.last_log_id);

        info!(last_applied = ?meta.last_log_id, "installed snapshot from leader");
        Ok(())
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<Snapshot<SpurTypeConfig>>, StorageError<NodeId>> {
        Ok(self.current_snapshot.read().as_ref().map(|ps| Snapshot {
            meta: ps.meta.clone(),
            snapshot: Box::new(Cursor::new(ps.data.clone())),
        }))
    }
}

/// Raft network factory — connects to peers on the dedicated Raft port.
pub struct SpurNetwork {
    peers: BTreeMap<NodeId, String>,
}

impl openraft::RaftNetworkFactory<SpurTypeConfig> for Arc<SpurNetwork> {
    type Network = SpurNetworkConnection;

    async fn new_client(&mut self, target: NodeId, _node: &BasicNode) -> Self::Network {
        let addr = self
            .peers
            .get(&target)
            .cloned()
            .unwrap_or_else(|| format!("unknown-{}", target));
        SpurNetworkConnection {
            target,
            addr,
            client: None,
        }
    }
}

/// Connection to a single Raft peer.
pub struct SpurNetworkConnection {
    #[allow(dead_code)]
    target: NodeId,
    addr: String,
    client: Option<
        spur_proto::raft_proto::raft_internal_client::RaftInternalClient<tonic::transport::Channel>,
    >,
}

impl SpurNetworkConnection {
    async fn get_client(
        &mut self,
    ) -> Result<
        &mut spur_proto::raft_proto::raft_internal_client::RaftInternalClient<
            tonic::transport::Channel,
        >,
        openraft::error::RPCError<NodeId, BasicNode, openraft::error::RaftError<NodeId>>,
    > {
        if self.client.is_none() {
            let url = if self.addr.starts_with("http") {
                self.addr.clone()
            } else {
                format!("http://{}", self.addr)
            };
            let channel = tonic::transport::Channel::from_shared(url)
                .map_err(|e| {
                    openraft::error::RPCError::Unreachable(openraft::error::Unreachable::new(
                        &std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()),
                    ))
                })?
                .connect_timeout(std::time::Duration::from_secs(2))
                .timeout(std::time::Duration::from_secs(5))
                .connect()
                .await
                .map_err(|e| {
                    openraft::error::RPCError::Unreachable(openraft::error::Unreachable::new(
                        &std::io::Error::new(std::io::ErrorKind::ConnectionRefused, e.to_string()),
                    ))
                })?;
            self.client = Some(raft_client(channel));
        }
        Ok(self.client.as_mut().unwrap())
    }
}

impl openraft::RaftNetwork<SpurTypeConfig> for SpurNetworkConnection {
    async fn append_entries(
        &mut self,
        rpc: openraft::raft::AppendEntriesRequest<SpurTypeConfig>,
        _option: openraft::network::RPCOption,
    ) -> Result<
        openraft::raft::AppendEntriesResponse<NodeId>,
        openraft::error::RPCError<NodeId, BasicNode, openraft::error::RaftError<NodeId>>,
    > {
        let payload = serde_json::to_vec(&rpc).map_err(|e| {
            openraft::error::RPCError::Unreachable(openraft::error::Unreachable::new(
                &std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()),
            ))
        })?;

        if payload.len() > RAFT_MAX_MESSAGE_SIZE {
            let hint = (rpc.entries.len() as u64 / 2).max(1);
            return Err(openraft::error::RPCError::PayloadTooLarge(
                openraft::error::PayloadTooLarge::new_entries_hint(hint),
            ));
        }

        let client = self.get_client().await?;
        let resp = client
            .append_entries(spur_proto::raft_proto::RaftRequest { payload })
            .await
            .map_err(|e| {
                self.client = None;
                openraft::error::RPCError::Unreachable(openraft::error::Unreachable::new(
                    &std::io::Error::new(std::io::ErrorKind::ConnectionRefused, e.to_string()),
                ))
            })?;

        serde_json::from_slice(&resp.into_inner().payload).map_err(|e| {
            openraft::error::RPCError::Unreachable(openraft::error::Unreachable::new(
                &std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()),
            ))
        })
    }

    async fn install_snapshot(
        &mut self,
        rpc: openraft::raft::InstallSnapshotRequest<SpurTypeConfig>,
        _option: openraft::network::RPCOption,
    ) -> Result<
        openraft::raft::InstallSnapshotResponse<NodeId>,
        openraft::error::RPCError<
            NodeId,
            BasicNode,
            openraft::error::RaftError<NodeId, openraft::error::InstallSnapshotError>,
        >,
    > {
        let payload = serde_json::to_vec(&rpc).map_err(|e| {
            openraft::error::RPCError::Unreachable(openraft::error::Unreachable::new(
                &std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()),
            ))
        })?;

        let client = self.get_client().await.map_err(|e| match e {
            openraft::error::RPCError::Unreachable(u) => openraft::error::RPCError::Unreachable(u),
            _ => openraft::error::RPCError::Unreachable(openraft::error::Unreachable::new(
                &std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "connection failed"),
            )),
        })?;

        let resp = client
            .install_snapshot(spur_proto::raft_proto::RaftRequest { payload })
            .await
            .map_err(|e| {
                self.client = None;
                openraft::error::RPCError::Unreachable(openraft::error::Unreachable::new(
                    &std::io::Error::new(std::io::ErrorKind::ConnectionRefused, e.to_string()),
                ))
            })?;

        serde_json::from_slice(&resp.into_inner().payload).map_err(|e| {
            openraft::error::RPCError::Unreachable(openraft::error::Unreachable::new(
                &std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()),
            ))
        })
    }

    async fn vote(
        &mut self,
        rpc: openraft::raft::VoteRequest<NodeId>,
        _option: openraft::network::RPCOption,
    ) -> Result<
        openraft::raft::VoteResponse<NodeId>,
        openraft::error::RPCError<NodeId, BasicNode, openraft::error::RaftError<NodeId>>,
    > {
        let payload = serde_json::to_vec(&rpc).map_err(|e| {
            openraft::error::RPCError::Unreachable(openraft::error::Unreachable::new(
                &std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()),
            ))
        })?;

        let client = self.get_client().await?;
        let resp = client
            .vote(spur_proto::raft_proto::RaftRequest { payload })
            .await
            .map_err(|e| {
                self.client = None;
                openraft::error::RPCError::Unreachable(openraft::error::Unreachable::new(
                    &std::io::Error::new(std::io::ErrorKind::ConnectionRefused, e.to_string()),
                ))
            })?;

        serde_json::from_slice(&resp.into_inner().payload).map_err(|e| {
            openraft::error::RPCError::Unreachable(openraft::error::Unreachable::new(
                &std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()),
            ))
        })
    }
}

/// Handle to the running Raft node — exposes leadership queries.
pub struct RaftHandle {
    pub raft: SpurRaft,
    pub node_id: NodeId,
    pub peers: BTreeMap<NodeId, String>,
}

impl RaftHandle {
    /// Cheap, locally-cached leadership check. Can be stale for the length of
    /// an election timeout after a partition — callers that need a
    /// consensus-backed answer (e.g. before writes that bypass Raft, like
    /// accounting reconciliation) must use `ensure_leader` instead.
    pub fn is_leader(&self) -> bool {
        let rx = self.raft.metrics();
        live_leader_from_rx(&rx) == Some(self.node_id)
    }

    /// Consensus-backed leadership check: confirms leadership by exchanging
    /// heartbeats with a quorum of followers, so a partitioned former leader
    /// gets `false` rather than a stale cached `true`. Costs a network
    /// round-trip; use for periodic/batch operations, not on every request.
    pub async fn ensure_leader(&self) -> bool {
        self.raft.ensure_linearizable().await.is_ok()
    }

    pub fn current_leader(&self) -> Option<NodeId> {
        let rx = self.raft.metrics();
        live_leader_from_rx(&rx)
    }

    /// Resolves with the [`Fatal`] that stopped the Raft core.
    pub async fn wait_fatal(&self) -> Fatal<NodeId> {
        let mut rx = self.raft.metrics();
        wait_fatal_on(&mut rx).await
    }

    /// Publishes when leadership has been held since, tagged with its term.
    /// Watch-driven, so a blip between two health ticks is still observed.
    pub fn spawn_leadership_watcher(&self) -> tokio::sync::watch::Receiver<Option<LeaderSince>> {
        let metrics_rx = self.raft.metrics();
        let node_id = self.node_id;
        let (tx, rx) = tokio::sync::watch::channel(None);
        tokio::spawn(run_leadership_watcher(metrics_rx, tx, node_id));
        rx
    }

    /// The Raft term this node currently holds a vote at, for confirming a
    /// published [`LeaderSince`] still describes the live term.
    pub fn current_term(&self) -> u64 {
        self.raft.metrics().borrow().current_term
    }
}

/// The leader in `metrics`, or `None` once the core has stopped: openraft leaves
/// `current_leader` pointing at the dead node after a fatal error.
fn live_leader(metrics: &openraft::RaftMetrics<NodeId, openraft::BasicNode>) -> Option<NodeId> {
    if metrics.running_state.is_err() || metrics.state == openraft::ServerState::Shutdown {
        return None;
    }
    metrics.current_leader
}

/// [`live_leader`], also failing closed on a dropped sender: a core panic never
/// sets `running_state = Err`, so the closed channel is the only stop signal.
fn live_leader_from_rx(
    rx: &tokio::sync::watch::Receiver<openraft::RaftMetrics<NodeId, openraft::BasicNode>>,
) -> Option<NodeId> {
    if rx.has_changed().is_err() {
        return None;
    }
    live_leader(&rx.borrow())
}

/// Returns the recorded `Fatal`, or `Panicked` if the sender closed while
/// `running_state` was still `Ok`.
async fn wait_fatal_on(
    rx: &mut tokio::sync::watch::Receiver<openraft::RaftMetrics<NodeId, openraft::BasicNode>>,
) -> Fatal<NodeId> {
    loop {
        if let Err(e) = rx.borrow().running_state.clone() {
            return e;
        }
        if rx.changed().await.is_err() {
            return match rx.borrow().running_state.clone() {
                Err(e) => e,
                Ok(()) => Fatal::Panicked,
            };
        }
    }
}

/// When this node won leadership, and the term it won it in. The term lets a
/// reader reject a value the watcher has not yet refreshed past a transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaderSince {
    pub at: std::time::Instant,
    pub term: u64,
}

/// Drives the leadership watch channel. Losing the raft metrics sender publishes
/// `None` first, so a reader fails closed rather than acting on a value that can
/// no longer change; the receiver-gone exits have nobody left to tell.
async fn run_leadership_watcher(
    mut metrics_rx: tokio::sync::watch::Receiver<
        openraft::RaftMetrics<NodeId, openraft::BasicNode>,
    >,
    tx: tokio::sync::watch::Sender<Option<LeaderSince>>,
    node_id: NodeId,
) {
    let mut published = None;
    loop {
        let (current_leader, current_term) = {
            let m = metrics_rx.borrow_and_update();
            (live_leader(&m), m.current_term)
        };
        let next = observe_leadership(
            current_leader,
            current_term,
            node_id,
            published,
            std::time::Instant::now(),
        );
        if next != published {
            published = next;
            if tx.send(published).is_err() {
                return;
            }
        }
        tokio::select! {
            _ = tx.closed() => return,
            changed = metrics_rx.changed() => {
                if changed.is_err() {
                    break;
                }
            }
        }
    }
    tracing::error!("raft leadership watcher stopped; suppressing node health checks");
    let _ = tx.send(None);
}

/// Restamps whenever this node leads under a term it was not already credited
/// with, so a coalesced lose-then-regain is still caught by its term bump.
fn observe_leadership(
    current_leader: Option<NodeId>,
    current_term: u64,
    node_id: NodeId,
    prior: Option<LeaderSince>,
    now: std::time::Instant,
) -> Option<LeaderSince> {
    if current_leader != Some(node_id) {
        return None;
    }
    match prior {
        Some(p) if p.term == current_term => Some(p),
        _ => Some(LeaderSince {
            at: now,
            term: current_term,
        }),
    }
}

/// The system hostname as a UTF-8 string.
pub fn system_hostname() -> anyhow::Result<String> {
    let name = hostname::get().map_err(|e| anyhow::anyhow!("failed to read hostname: {e}"))?;
    Ok(name.to_string_lossy().into_owned())
}

/// How a node's Raft id was determined, for startup diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeIdSource {
    Explicit,
    PeersPosition,
    HostnameOrdinal,
}

impl std::fmt::Display for NodeIdSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            NodeIdSource::Explicit => "explicit controller.node_id",
            NodeIdSource::PeersPosition => "position in controller.peers",
            NodeIdSource::HostnameOrdinal => "hostname ordinal",
        };
        f.write_str(s)
    }
}

/// Resolve this node's Raft id (multi-node; `peers` non-empty). Precedence:
/// explicit -> position in `peers` -> hostname ordinal. Validated to be in
/// `1..=peers.len()`, else fails fast (an out-of-range id splits brain).
pub fn resolve_node_id(
    explicit: Option<u64>,
    hostname: &str,
    peers: &[String],
) -> anyhow::Result<(u64, NodeIdSource)> {
    let n = peers.len() as u64;

    if let Some(id) = explicit {
        if !(1..=n).contains(&id) {
            anyhow::bail!(
                "controller.node_id = {id} is out of range for {n} configured \
                 controller.peers (must be 1..={n})"
            );
        }
        return Ok((id, NodeIdSource::Explicit));
    }

    if let Some(id) = node_id_from_peers(hostname, peers)? {
        return Ok((id, NodeIdSource::PeersPosition));
    }

    // Ordinal fallback only fits IP-only peers (no hostname to match). With
    // hostname peers a no-match is a misconfig: fail fast, don't guess an id.
    if !all_peers_ip_only(peers) {
        anyhow::bail!(
            "this host ({hostname:?}) matched no entry in controller.peers; fix the \
             hostname to match its peers entry or set controller.node_id in spur.conf"
        );
    }

    // Accept the ordinal only if in range; out-of-range is a misconfig.
    if let Some(id) = node_id_from_hostname(hostname) {
        if (1..=n).contains(&id) {
            return Ok((id, NodeIdSource::HostnameOrdinal));
        }
        anyhow::bail!(
            "hostname {hostname:?} yields Raft node_id {id} via ordinal fallback, \
             out of range for {n} configured controller.peers; fix the hostname \
             to match its controller.peers entry or set controller.node_id"
        );
    }

    anyhow::bail!(
        "controller.peers are IP-only and hostname {hostname:?} has no ordinal to \
         derive a node_id from; set controller.node_id in spur.conf"
    )
}

/// True when every peer host is an IP literal (no hostname to match against).
fn all_peers_ip_only(peers: &[String]) -> bool {
    peers
        .iter()
        .all(|entry| peer_host(entry).parse::<std::net::IpAddr>().is_ok())
}

/// Parse a node_id from a hostname string. The ordinal after the last '-'
/// is treated as a 0-based index and converted to a 1-based node_id.
pub fn node_id_from_hostname(hostname: &str) -> Option<u64> {
    let ordinal: u64 = hostname.rsplit('-').next()?.parse().ok()?;
    Some(ordinal + 1)
}

/// Node id (index + 1) of the `peers` entry matching `hostname`, on the full
/// name or first DNS label (so `spurctld-0` matches `spurctld-0.ns.svc...`).
/// `Ok(None)` if none match; errors if several do (guessing risks split-brain).
pub fn node_id_from_peers(hostname: &str, peers: &[String]) -> anyhow::Result<Option<u64>> {
    let matches: Vec<u64> = peers
        .iter()
        .enumerate()
        .filter(|(_, entry)| host_matches(hostname, peer_host(entry)))
        .map(|(i, _)| i as u64 + 1)
        .collect();
    match matches.as_slice() {
        [] => Ok(None),
        [id] => Ok(Some(*id)),
        ids => anyhow::bail!(
            "hostname {hostname:?} matches multiple controller.peers entries \
             (node ids {ids:?}); set controller.node_id explicitly"
        ),
    }
}

/// Extract the host part from a `host:port` peer entry, handling bracketed IPv6
/// (`[::1]:6821`) and bare hosts.
fn peer_host(entry: &str) -> &str {
    let entry = entry.trim();
    if let Some(rest) = entry.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    // One ':' is "host:port"; more colons are an unbracketed IPv6 literal (no port).
    match entry.rsplit_once(':') {
        Some((host, _)) if !host.contains(':') => host,
        _ => entry,
    }
}

/// True if `hostname` matches `peer_host` in full or on the first DNS label.
fn host_matches(hostname: &str, peer_host: &str) -> bool {
    if hostname.eq_ignore_ascii_case(peer_host) {
        return true;
    }
    first_label(hostname).eq_ignore_ascii_case(first_label(peer_host))
}

/// The segment of a hostname before the first '.' (its first DNS label).
fn first_label(host: &str) -> &str {
    host.split('.').next().unwrap_or(host)
}

/// Guard against a shifted node identity across restarts: the id is re-derived
/// each boot but persisted Raft state belongs to the old id, so a reordered or
/// renamed peer would orphan committed entries. Persist first, then verify.
fn persist_or_verify_node_id(state_dir: &Path, node_id: NodeId) -> anyhow::Result<()> {
    let raft_dir = state_dir.join("raft");
    std::fs::create_dir_all(&raft_dir)?;
    let path = raft_dir.join("node_id");

    match std::fs::read_to_string(&path) {
        Ok(contents) => {
            let prev: NodeId = contents.trim().parse().map_err(|_| {
                anyhow::anyhow!(
                    "failed to parse persisted node_id {:?} at {}",
                    contents.trim(),
                    path.display()
                )
            })?;
            if prev != node_id {
                anyhow::bail!(
                    "resolved Raft node_id {node_id} differs from persisted id {prev} at {}; \
                     the controller.peers ordering or this host's name likely changed. \
                     Running under a new id would orphan committed Raft state. Restore the \
                     previous identity, or wipe the raft state dir to rejoin as a fresh member.",
                    path.display()
                );
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let tmp = raft_dir.join("node_id.tmp");
            std::fs::write(&tmp, node_id.to_string())?;
            std::fs::rename(&tmp, &path)?;
            Ok(())
        }
        Err(e) => Err(anyhow::anyhow!("failed to read {}: {e}", path.display())),
    }
}

/// Build a 1-indexed peer map from the config peers list.
pub fn build_peer_map(peers: &[String]) -> BTreeMap<NodeId, String> {
    peers
        .iter()
        .enumerate()
        .map(|(i, addr)| (i as u64 + 1, addr.clone()))
        .collect()
}

/// Test-only convenience wrapper around `start_raft_with_recovery_mode` with
/// strict WAL/snapshot recovery (see `SpurStore::new`). Production startup
/// (main.rs) calls `start_raft_with_recovery_mode` directly since it needs to
/// thread through the `--allow-partial-wal-recovery` CLI flag.
#[cfg(test)]
pub async fn start_raft(
    node_id: NodeId,
    peers: &[String],
    state_dir: &Path,
    applier: Arc<dyn StateMachineApply>,
) -> anyhow::Result<RaftHandle> {
    start_raft_with_recovery_mode(node_id, peers, state_dir, applier, true).await
}

pub async fn start_raft_with_recovery_mode(
    node_id: NodeId,
    peers: &[String],
    state_dir: &Path,
    applier: Arc<dyn StateMachineApply>,
    strict_wal_recovery: bool,
) -> anyhow::Result<RaftHandle> {
    let config = Config {
        heartbeat_interval: 500,
        election_timeout_min: 1500,
        election_timeout_max: 3000,
        snapshot_max_chunk_size: RAFT_SNAPSHOT_MAX_CHUNK_SIZE,
        ..Default::default()
    };
    let config = Arc::new(config.validate().map_err(|e| anyhow::anyhow!("{e}"))?);

    persist_or_verify_node_id(state_dir, node_id)?;

    let store = Arc::new(SpurStore::new_with_recovery_mode(
        state_dir,
        applier,
        strict_wal_recovery,
    )?);
    let peer_map = build_peer_map(peers);
    let network = Arc::new(SpurNetwork {
        peers: peer_map.clone(),
    });

    let (log_store, state_machine) = openraft::storage::Adaptor::new(store.clone());
    let raft = Raft::new(node_id, config, network, log_store, state_machine).await?;

    // Symmetric bootstrap only applies on first-ever startup; skip it once we
    // have prior state, else openraft logs an ERROR rejecting it every restart.
    let already_initialized = {
        let inner = store.inner.read();
        inner.vote.is_some() || !inner.log.is_empty() || inner.last_applied.is_some()
    };
    if already_initialized {
        debug!("raft store has prior state; skipping redundant initialize()");
    } else {
        let members: BTreeMap<NodeId, BasicNode> = peer_map
            .iter()
            .map(|(id, addr)| (*id, BasicNode::new(addr.clone())))
            .collect();
        if let Err(e) = raft.initialize(members).await {
            debug!("raft initialize: {e} (already initialized)");
        }
    }

    info!(node_id, peers = ?peer_map, "raft node started");
    Ok(RaftHandle {
        raft,
        node_id,
        peers: peer_map,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    struct NoopApplier;
    impl StateMachineApply for NoopApplier {
        fn apply_operation(&self, _op: &WalOperation) -> ClientResponse {
            ClientResponse::default()
        }
        fn snapshot_state(&self) -> Result<Vec<u8>, anyhow::Error> {
            Ok(Vec::new())
        }
        fn restore_from_snapshot(&self, _data: &[u8]) -> Result<(), anyhow::Error> {
            Ok(())
        }
    }

    fn noop_applier() -> Arc<dyn StateMachineApply> {
        Arc::new(NoopApplier)
    }

    fn snap_meta(index: u64, id: &str) -> SnapshotMeta<NodeId, BasicNode> {
        SnapshotMeta {
            last_log_id: Some(LogId {
                leader_id: openraft::LeaderId {
                    term: 1,
                    node_id: 1,
                },
                index,
            }),
            last_membership: StoredMembership::default(),
            snapshot_id: id.into(),
        }
    }

    fn snapshot_path(dir: &TempDir) -> PathBuf {
        dir.path().join("raft").join("snapshot.json")
    }

    #[test]
    fn peer_map_empty() {
        let m = build_peer_map(&[]);
        assert!(m.is_empty());
    }

    #[test]
    fn peer_map_single() {
        let m = build_peer_map(&["host1:6821".into()]);
        assert_eq!(m.len(), 1);
        assert_eq!(m[&1], "host1:6821");
    }

    #[test]
    fn peer_map_three_nodes() {
        let m = build_peer_map(&["a:6821".into(), "b:6821".into(), "c:6821".into()]);
        assert_eq!(m.len(), 3);
        assert_eq!(m[&1], "a:6821");
        assert_eq!(m[&2], "b:6821");
        assert_eq!(m[&3], "c:6821");
    }

    #[test]
    fn hostname_spurctld_0() {
        assert_eq!(node_id_from_hostname("spurctld-0"), Some(1));
    }

    #[test]
    fn hostname_spurctld_2() {
        assert_eq!(node_id_from_hostname("spurctld-2"), Some(3));
    }

    #[test]
    fn hostname_custom_prefix() {
        assert_eq!(node_id_from_hostname("my-cluster-ctrl-5"), Some(6));
    }

    #[test]
    fn hostname_no_dash() {
        assert_eq!(node_id_from_hostname("localhost"), None);
    }

    #[test]
    fn hostname_non_numeric_suffix() {
        assert_eq!(node_id_from_hostname("ctrl-abc"), None);
    }

    #[test]
    fn peers_position_mapping() {
        let peers = vec![
            "hostA:6821".to_string(),
            "hostB:6821".to_string(),
            "hostC:6821".to_string(),
        ];
        assert_eq!(node_id_from_peers("hostA", &peers).unwrap(), Some(1));
        assert_eq!(node_id_from_peers("hostB", &peers).unwrap(), Some(2));
        assert_eq!(node_id_from_peers("hostC", &peers).unwrap(), Some(3));
    }

    #[test]
    fn peers_short_hostname_matches_fqdn_entry() {
        let peers = vec![
            "spurctld-0.ns.svc.cluster.local:6821".to_string(),
            "spurctld-1.ns.svc.cluster.local:6821".to_string(),
        ];
        assert_eq!(node_id_from_peers("spurctld-0", &peers).unwrap(), Some(1));
        assert_eq!(node_id_from_peers("spurctld-1", &peers).unwrap(), Some(2));
    }

    #[test]
    fn peers_fqdn_hostname_matches_short_entry() {
        let peers = vec!["node1:6821".to_string(), "node2:6821".to_string()];
        assert_eq!(
            node_id_from_peers("node2.example.com", &peers).unwrap(),
            Some(2)
        );
    }

    #[test]
    fn peers_no_match_is_none() {
        let peers = vec!["hostA:6821".to_string(), "hostB:6821".to_string()];
        assert_eq!(node_id_from_peers("hostZ", &peers).unwrap(), None);
    }

    #[test]
    fn peers_ambiguous_match_errors() {
        // Same first DNS label in two different domains: both reduce to "n1".
        let peers = vec!["n1.dc-a:6821".to_string(), "n1.dc-b:6821".to_string()];
        assert!(node_id_from_peers("n1", &peers).is_err());
    }

    #[test]
    fn peers_matches_bracketed_ipv6_entry() {
        let peers = vec!["[::1]:6821".to_string(), "hostB:6821".to_string()];
        assert_eq!(peer_host("[::1]:6821"), "::1");
        assert_eq!(node_id_from_peers("hostB", &peers).unwrap(), Some(2));
    }

    #[test]
    fn peer_host_parsing() {
        assert_eq!(peer_host("host:6821"), "host");
        assert_eq!(peer_host("host"), "host");
        assert_eq!(peer_host("host.example.com:6821"), "host.example.com");
        assert_eq!(peer_host("[2001:db8::1]:6821"), "2001:db8::1");
    }

    fn three_peers() -> Vec<String> {
        vec![
            "hostA:6821".to_string(),
            "hostB:6821".to_string(),
            "hostC:6821".to_string(),
        ]
    }

    #[test]
    fn resolve_explicit_wins() {
        let (id, source) = resolve_node_id(Some(2), "hostA", &three_peers()).unwrap();
        assert_eq!(id, 2);
        assert_eq!(source, NodeIdSource::Explicit);
    }

    #[test]
    fn resolve_explicit_out_of_range_errors() {
        assert!(resolve_node_id(Some(9), "hostA", &three_peers()).is_err());
        assert!(resolve_node_id(Some(0), "hostA", &three_peers()).is_err());
    }

    #[test]
    fn resolve_peers_position() {
        let (id, source) = resolve_node_id(None, "hostC", &three_peers()).unwrap();
        assert_eq!(id, 3);
        assert_eq!(source, NodeIdSource::PeersPosition);
    }

    #[test]
    fn resolve_hostname_ordinal_fallback_for_ip_peers() {
        // IP-only peers can't match a hostname; the ordinal fallback applies.
        let peers = vec![
            "10.0.0.1:6821".to_string(),
            "10.0.0.2:6821".to_string(),
            "10.0.0.3:6821".to_string(),
        ];
        let (id, source) = resolve_node_id(None, "spurctld-1", &peers).unwrap();
        assert_eq!(id, 2);
        assert_eq!(source, NodeIdSource::HostnameOrdinal);
    }

    #[test]
    fn resolve_ordinal_out_of_range_errors() {
        // spurctld-5 -> ordinal id 6, but only 3 IP-only peers exist.
        let peers = vec![
            "10.0.0.1:6821".to_string(),
            "10.0.0.2:6821".to_string(),
            "10.0.0.3:6821".to_string(),
        ];
        assert!(resolve_node_id(None, "spurctld-5", &peers).is_err());
    }

    #[test]
    fn resolve_no_match_errors() {
        assert!(resolve_node_id(None, "unknownhost", &three_peers()).is_err());
    }

    #[test]
    fn resolve_hostname_peers_no_match_does_not_fall_back_to_ordinal() {
        // spurctld-1 has a valid in-range ordinal (2), but hostname peers with
        // no match must fail fast, not fall back to it.
        let err = resolve_node_id(None, "spurctld-1", &three_peers()).unwrap_err();
        assert!(
            err.to_string().contains("matched no entry"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_ip_only_peers_without_ordinal_errors() {
        let peers = vec!["10.0.0.1:6821".to_string(), "10.0.0.2:6821".to_string()];
        assert!(resolve_node_id(None, "plainhost", &peers).is_err());
    }

    #[test]
    fn node_id_persists_on_first_boot() {
        let dir = TempDir::new().unwrap();
        persist_or_verify_node_id(dir.path(), 2).unwrap();
        let persisted = std::fs::read_to_string(dir.path().join("raft").join("node_id")).unwrap();
        assert_eq!(persisted.trim(), "2");
    }

    #[test]
    fn node_id_same_across_restart_is_ok() {
        let dir = TempDir::new().unwrap();
        persist_or_verify_node_id(dir.path(), 3).unwrap();
        persist_or_verify_node_id(dir.path(), 3).unwrap();
    }

    #[test]
    fn node_id_shift_across_restart_fails_fast() {
        let dir = TempDir::new().unwrap();
        persist_or_verify_node_id(dir.path(), 1).unwrap();
        let err = persist_or_verify_node_id(dir.path(), 2).unwrap_err();
        assert!(
            err.to_string().contains("differs from persisted id"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_ambiguous_match_errors() {
        let peers = vec!["n1.dc-a:6821".to_string(), "n1.dc-b:6821".to_string()];
        assert!(resolve_node_id(None, "n1", &peers).is_err());
    }

    #[test]
    fn store_persists_vote() {
        let dir = TempDir::new().unwrap();
        let store = SpurStore::new(dir.path(), noop_applier()).unwrap();

        let vote = Vote {
            leader_id: openraft::LeaderId {
                term: 5,
                node_id: 2,
            },
            committed: true,
        };
        store.persist_vote(&vote).unwrap();

        let store2 = SpurStore::new(dir.path(), noop_applier()).unwrap();
        let inner = store2.inner.read();
        let recovered = inner.vote.as_ref().unwrap();
        assert_eq!(recovered.leader_id.term, 5);
        assert_eq!(recovered.leader_id.node_id, 2);
        assert!(recovered.committed);
    }

    #[test]
    fn store_persists_log_entries() {
        let dir = TempDir::new().unwrap();
        let store = SpurStore::new(dir.path(), noop_applier()).unwrap();

        let entry = Entry {
            log_id: LogId {
                leader_id: openraft::LeaderId {
                    term: 1,
                    node_id: 1,
                },
                index: 42,
            },
            payload: EntryPayload::Blank,
        };
        store.persist_log_entry(&entry).unwrap();

        let store2 = SpurStore::new(dir.path(), noop_applier()).unwrap();
        let inner = store2.inner.read();
        assert!(inner.log.contains_key(&42));
        assert_eq!(inner.log[&42].log_id.index, 42);
    }

    #[test]
    fn store_removes_log_entries() {
        let dir = TempDir::new().unwrap();
        let store = SpurStore::new(dir.path(), noop_applier()).unwrap();

        for idx in 0..5 {
            let entry = Entry {
                log_id: LogId {
                    leader_id: openraft::LeaderId {
                        term: 1,
                        node_id: 1,
                    },
                    index: idx,
                },
                payload: EntryPayload::Blank,
            };
            store.persist_log_entry(&entry).unwrap();
            store.inner.write().log.insert(idx, entry);
        }

        store.remove_log_entry(2);
        store.remove_log_entry(3);

        let store2 = SpurStore::new(dir.path(), noop_applier()).unwrap();
        let inner = store2.inner.read();
        assert_eq!(inner.log.len(), 3); // 0, 1, 4
        assert!(inner.log.contains_key(&0));
        assert!(inner.log.contains_key(&1));
        assert!(!inner.log.contains_key(&2));
        assert!(!inner.log.contains_key(&3));
        assert!(inner.log.contains_key(&4));
    }

    #[test]
    fn store_persists_snapshot() {
        let dir = TempDir::new().unwrap();
        let store = SpurStore::new(dir.path(), noop_applier()).unwrap();

        let meta = SnapshotMeta {
            last_log_id: Some(LogId {
                leader_id: openraft::LeaderId {
                    term: 3,
                    node_id: 1,
                },
                index: 100,
            }),
            last_membership: StoredMembership::default(),
            snapshot_id: "snap-100".into(),
        };
        let data = b"test snapshot data".to_vec();
        store
            .persist_and_cache_snapshot(meta, data.clone())
            .unwrap();

        // Served from the in-memory cache, never re-read from disk.
        let cached = store.current_snapshot.read().clone().unwrap();
        assert_eq!(cached.meta.snapshot_id, "snap-100");
        assert_eq!(cached.data, data);

        // And it survives a restart: a fresh store recovers it from disk.
        let reopened = SpurStore::new(dir.path(), noop_applier()).unwrap();
        let recovered = reopened.current_snapshot.read().clone().unwrap();
        assert_eq!(recovered.meta.snapshot_id, "snap-100");
        assert_eq!(recovered.data, data);
    }

    #[tokio::test]
    async fn get_current_snapshot_unaffected_by_torn_file_on_disk() {
        use openraft::RaftStorage;
        let dir = TempDir::new().unwrap();
        let mut store = Arc::new(SpurStore::new(dir.path(), noop_applier()).unwrap());

        let meta = SnapshotMeta {
            last_log_id: Some(LogId {
                leader_id: openraft::LeaderId {
                    term: 3,
                    node_id: 1,
                },
                index: 100,
            }),
            last_membership: StoredMembership::default(),
            snapshot_id: "snap-100".into(),
        };
        let data = b"intact snapshot".to_vec();
        store
            .persist_and_cache_snapshot(meta, data.clone())
            .unwrap();

        // A half-written snapshot.json must not matter: reads come from memory.
        std::fs::write(dir.path().join("raft").join("snapshot.json"), b"{tor").unwrap();

        let snap = store
            .get_current_snapshot()
            .await
            .unwrap()
            .expect("snapshot served from memory");
        assert_eq!(snap.meta.snapshot_id, "snap-100");
        assert_eq!(snap.snapshot.into_inner(), data);
    }

    #[test]
    fn persist_refuses_to_replace_newer_snapshot_with_older() {
        let dir = TempDir::new().unwrap();
        let store = SpurStore::new(dir.path(), noop_applier()).unwrap();

        store
            .persist_and_cache_snapshot(snap_meta(200, "new"), b"new".to_vec())
            .unwrap();
        // An older build finishing late must not clobber the newer snapshot.
        let kept = store
            .persist_and_cache_snapshot(snap_meta(100, "old"), b"old".to_vec())
            .unwrap();
        assert_eq!(kept.meta.snapshot_id, "new");

        let cached = store.current_snapshot.read().clone().unwrap();
        assert_eq!(cached.meta.snapshot_id, "new");
        assert_eq!(cached.data, b"new");

        let reopened = SpurStore::new(dir.path(), noop_applier()).unwrap();
        let on_disk = reopened.current_snapshot.read().clone().unwrap();
        assert_eq!(on_disk.meta.snapshot_id, "new");
        assert_eq!(on_disk.data, b"new");
    }

    #[tokio::test]
    async fn build_snapshot_keeps_newer_cached_snapshot_over_older_state() {
        use openraft::RaftSnapshotBuilder;
        let dir = TempDir::new().unwrap();
        let mut store = Arc::new(SpurStore::new(dir.path(), noop_applier()).unwrap());

        store
            .persist_and_cache_snapshot(snap_meta(200, "new"), b"new".to_vec())
            .unwrap();
        store.inner.write().last_applied = snap_meta(100, "").last_log_id;

        let built = store.build_snapshot().await.unwrap();
        assert_eq!(built.meta.snapshot_id, "new");
        assert_eq!(built.meta.last_log_id.unwrap().index, 200);

        let reopened = SpurStore::new(dir.path(), noop_applier()).unwrap();
        let on_disk = reopened.current_snapshot.read().clone().unwrap();
        assert_eq!(on_disk.meta.snapshot_id, "new");
        assert_eq!(on_disk.data, b"new");
    }

    #[tokio::test]
    async fn install_snapshot_success_is_served_applied_and_recovered() {
        use openraft::RaftStorage;
        let dir = TempDir::new().unwrap();
        let mut store = Arc::new(SpurStore::new(dir.path(), noop_applier()).unwrap());
        let meta = snap_meta(10, "snap-10");

        store
            .install_snapshot(&meta, Box::new(Cursor::new(b"state".to_vec())))
            .await
            .unwrap();

        let served = store.get_current_snapshot().await.unwrap().unwrap();
        assert_eq!(served.meta, meta);
        assert_eq!(served.snapshot.into_inner(), b"state");
        let (applied, membership) = store.last_applied_state().await.unwrap();
        assert_eq!(applied, meta.last_log_id);
        assert_eq!(membership, meta.last_membership);

        let mut reopened = Arc::new(SpurStore::new(dir.path(), noop_applier()).unwrap());
        let recovered = reopened.get_current_snapshot().await.unwrap().unwrap();
        assert_eq!(recovered.meta, meta);
        assert_eq!(recovered.snapshot.into_inner(), b"state");
        assert_eq!(
            reopened.last_applied_state().await.unwrap().0,
            meta.last_log_id
        );
    }

    #[test]
    fn startup_drops_garbage_temp_and_recovers_snapshot() {
        let dir = TempDir::new().unwrap();
        let store = SpurStore::new(dir.path(), noop_applier()).unwrap();
        store
            .persist_and_cache_snapshot(snap_meta(5, "snap-5"), b"state".to_vec())
            .unwrap();
        let tmp = dir.path().join("raft").join("snapshot.json.tmp");
        std::fs::write(&tmp, b"{garbage").unwrap();

        let reopened = SpurStore::new(dir.path(), noop_applier()).unwrap();
        assert!(!tmp.exists());
        let recovered = reopened.current_snapshot.read().clone().unwrap();
        assert_eq!(recovered.meta.snapshot_id, "snap-5");
    }

    #[test]
    fn write_atomic_replaces_target_and_leaves_no_temp() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("f.json");
        write_atomic(&path, b"first").unwrap();
        write_atomic(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");

        let mut tmp = path.as_os_str().to_os_string();
        tmp.push(".tmp");
        assert!(!std::path::Path::new(&tmp).exists());
    }

    #[test]
    fn write_atomic_removes_temp_when_rename_fails() {
        let dir = TempDir::new().unwrap();
        // Renaming a file over a non-empty directory fails after the temp is written.
        let path = dir.path().join("f.json");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("child"), b"x").unwrap();

        assert!(write_atomic(&path, b"data").is_err());
        assert!(!dir.path().join("f.json.tmp").exists());
    }

    // Byte-identical JSON keeps snapshot.json readable across binaries.
    #[test]
    fn persisted_snapshot_ref_json_matches_owned() {
        let meta = SnapshotMeta {
            last_log_id: Some(LogId {
                leader_id: openraft::LeaderId {
                    term: 4,
                    node_id: 2,
                },
                index: 42,
            }),
            last_membership: StoredMembership::default(),
            snapshot_id: "snap-42".into(),
        };
        let data = vec![0u8, 1, 2, 250, 255];

        let owned = PersistedSnapshot {
            meta: meta.clone(),
            data: data.clone(),
        };
        let borrowed = PersistedSnapshotRef {
            meta: &meta,
            data: &data,
        };

        let owned_json = serde_json::to_vec(&owned).unwrap();
        let borrowed_json = serde_json::to_vec(&borrowed).unwrap();
        assert_eq!(owned_json, borrowed_json);

        // And the owned struct deserializes back from the borrowed output.
        let round: PersistedSnapshot = serde_json::from_slice(&borrowed_json).unwrap();
        assert_eq!(round.meta.snapshot_id, "snap-42");
        assert_eq!(round.data, data);
    }

    #[test]
    fn store_empty_dir_recovery() {
        let dir = TempDir::new().unwrap();
        let store = SpurStore::new(dir.path(), noop_applier()).unwrap();

        let inner = store.inner.read();
        assert!(inner.vote.is_none());
        assert!(inner.log.is_empty());
        assert!(inner.last_applied.is_none());
    }

    #[tokio::test]
    async fn store_persists_last_purged_across_restart() {
        use openraft::RaftStorage;
        let dir = TempDir::new().unwrap();
        let log_id = LogId {
            leader_id: openraft::LeaderId {
                term: 7,
                node_id: 1,
            },
            index: 9999,
        };

        // Exercise the real public purge path, not the internal helper.
        {
            let mut store = Arc::new(SpurStore::new(dir.path(), noop_applier()).unwrap());
            store.purge_logs_upto(log_id).await.unwrap();
        }

        // Simulate restart: reconstruct from the same dir and verify via get_log_state.
        let mut store2 = Arc::new(SpurStore::new(dir.path(), noop_applier()).unwrap());
        let state = store2.get_log_state().await.unwrap();
        assert_eq!(state.last_purged_log_id, Some(log_id));
    }

    #[test]
    fn store_no_purged_file_recovers_as_none() {
        let dir = TempDir::new().unwrap();
        let store = SpurStore::new(dir.path(), noop_applier()).unwrap();
        let inner = store.inner.read();
        assert!(inner.last_purged.is_none());
    }

    #[test]
    fn live_leader_reports_leader_while_running() {
        let mut m = openraft::RaftMetrics::<NodeId, openraft::BasicNode>::new_initial(1);
        m.state = openraft::ServerState::Leader;
        m.current_leader = Some(1);
        assert_eq!(super::live_leader(&m), Some(1));
    }

    #[test]
    fn live_leader_fails_closed_on_shutdown_state() {
        let mut m = openraft::RaftMetrics::<NodeId, openraft::BasicNode>::new_initial(1);
        m.current_leader = Some(1);
        m.state = openraft::ServerState::Shutdown;
        assert_eq!(super::live_leader(&m), None);
    }

    #[test]
    fn live_leader_fails_closed_on_fatal_running_state() {
        let mut m = openraft::RaftMetrics::<NodeId, openraft::BasicNode>::new_initial(1);
        m.state = openraft::ServerState::Leader;
        m.current_leader = Some(1);
        m.running_state = Err(Fatal::Stopped);
        assert_eq!(super::live_leader(&m), None);
    }

    #[test]
    fn live_leader_from_rx_reports_leader_while_sender_alive() {
        let mut m = openraft::RaftMetrics::<NodeId, openraft::BasicNode>::new_initial(1);
        m.state = openraft::ServerState::Leader;
        m.current_leader = Some(1);
        let (_tx, rx) = tokio::sync::watch::channel(m);
        assert_eq!(super::live_leader_from_rx(&rx), Some(1));
    }

    // A panic only drops the sender; the last snapshot still shows a running leader.
    #[test]
    fn live_leader_from_rx_fails_closed_when_sender_dropped() {
        let mut m = openraft::RaftMetrics::<NodeId, openraft::BasicNode>::new_initial(1);
        m.state = openraft::ServerState::Leader;
        m.current_leader = Some(1);
        let (tx, rx) = tokio::sync::watch::channel(m);
        drop(tx);
        assert_eq!(super::live_leader_from_rx(&rx), None);
    }

    #[tokio::test]
    async fn wait_fatal_returns_the_recorded_error() {
        let (tx, mut rx) = tokio::sync::watch::channel(openraft::RaftMetrics::<
            NodeId,
            openraft::BasicNode,
        >::new_initial(1));
        let io = std::io::Error::other("disk full");
        let err = StorageError::from_io_error(
            openraft::ErrorSubject::Logs,
            openraft::ErrorVerb::Write,
            io,
        );
        tx.send_modify(|m| m.running_state = Err(Fatal::StorageError(err)));
        assert!(matches!(
            super::wait_fatal_on(&mut rx).await,
            Fatal::StorageError(_)
        ));
    }

    // Graceful shutdown records `Err(Stopped)` before dropping the sender.
    #[tokio::test]
    async fn wait_fatal_reports_stopped_on_graceful_shutdown() {
        let (tx, mut rx) = tokio::sync::watch::channel(openraft::RaftMetrics::<
            NodeId,
            openraft::BasicNode,
        >::new_initial(1));
        tx.send_modify(|m| m.running_state = Err(Fatal::Stopped));
        drop(tx);
        assert!(matches!(
            super::wait_fatal_on(&mut rx).await,
            Fatal::Stopped
        ));
    }

    // A panic drops the sender while `running_state` is still `Ok`.
    #[tokio::test]
    async fn wait_fatal_reports_panicked_when_sender_closes_without_error() {
        let (tx, mut rx) = tokio::sync::watch::channel(openraft::RaftMetrics::<
            NodeId,
            openraft::BasicNode,
        >::new_initial(1));
        drop(tx);
        assert!(matches!(
            super::wait_fatal_on(&mut rx).await,
            Fatal::Panicked
        ));
    }

    // The error arrives after the wait begins and must wake the waiter.
    #[tokio::test]
    async fn wait_fatal_wakes_on_a_later_error() {
        let (tx, mut rx) = tokio::sync::watch::channel(openraft::RaftMetrics::<
            NodeId,
            openraft::BasicNode,
        >::new_initial(1));
        let waiter = tokio::spawn(async move { super::wait_fatal_on(&mut rx).await });
        tokio::task::yield_now().await;
        tx.send_modify(|m| m.running_state = Err(Fatal::Stopped));
        assert!(matches!(waiter.await.unwrap(), Fatal::Stopped));
    }

    #[test]
    fn observe_leadership_sets_since_on_election_win() {
        let now = std::time::Instant::now();
        let got = super::observe_leadership(Some(1), 3, 1, None, now);
        assert_eq!(got, Some(super::LeaderSince { at: now, term: 3 }));
    }

    #[test]
    fn observe_leadership_keeps_since_steady_while_leader_and_term_unchanged() {
        let t0 = std::time::Instant::now();
        let prior = super::LeaderSince { at: t0, term: 3 };
        let t1 = t0 + std::time::Duration::from_secs(5);
        let got = super::observe_leadership(Some(1), 3, 1, Some(prior), t1);
        assert_eq!(
            got,
            Some(prior),
            "unchanged term must not restart the window"
        );
    }

    #[test]
    fn observe_leadership_resets_on_term_bump_while_still_leader() {
        // A watch channel coalescing a lose-then-regain: the leader value reads
        // unchanged, but the term proves an election happened in between.
        let t0 = std::time::Instant::now();
        let prior = super::LeaderSince { at: t0, term: 3 };
        let t1 = t0 + std::time::Duration::from_secs(20);
        let got = super::observe_leadership(Some(1), 4, 1, Some(prior), t1);
        assert_eq!(got, Some(super::LeaderSince { at: t1, term: 4 }));
    }

    #[test]
    fn observe_leadership_clears_since_when_not_leader() {
        let t0 = std::time::Instant::now();
        let prior = super::LeaderSince { at: t0, term: 3 };
        assert_eq!(
            super::observe_leadership(Some(2), 3, 1, Some(prior), t0),
            None
        );
    }

    fn metrics_at(leader: Option<NodeId>, term: u64) -> openraft::RaftMetrics<NodeId, BasicNode> {
        let mut m = openraft::RaftMetrics::new_initial(1);
        m.current_leader = leader;
        m.current_term = term;
        m
    }

    /// Drives the real watcher loop, not just its pure decision fn: this is the
    /// wiring that previously sampled on a timer and missed sub-tick blips.
    #[tokio::test]
    async fn leadership_watcher_restamps_on_a_coalesced_lose_then_regain() {
        let (metrics_tx, metrics_rx) = tokio::sync::watch::channel(metrics_at(Some(1), 3));
        let (tx, mut rx) = tokio::sync::watch::channel(None);
        let task = tokio::spawn(super::run_leadership_watcher(metrics_rx, tx, 1));

        rx.changed().await.unwrap();
        let first = rx.borrow_and_update().expect("leader at term 3");
        assert_eq!(first.term, 3);

        // One send collapsing a lost-and-regained leadership: `current_leader`
        // reads the same on both sides, so only the term exposes the blip.
        metrics_tx.send(metrics_at(Some(1), 4)).unwrap();
        rx.changed().await.unwrap();
        let second = rx.borrow_and_update().expect("leader again at term 4");
        assert_eq!(second.term, 4);
        assert!(second.at > first.at, "the grace window must be re-armed");

        task.abort();
    }

    #[tokio::test]
    async fn leadership_watcher_publishes_none_on_losing_leadership() {
        let (metrics_tx, metrics_rx) = tokio::sync::watch::channel(metrics_at(Some(1), 3));
        let (tx, mut rx) = tokio::sync::watch::channel(None);
        let task = tokio::spawn(super::run_leadership_watcher(metrics_rx, tx, 1));

        rx.changed().await.unwrap();
        assert!(rx.borrow_and_update().is_some());

        metrics_tx.send(metrics_at(Some(2), 4)).unwrap();
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow_and_update(), None);

        task.abort();
    }

    /// A watcher that stops must leave the health path suppressed, not acting on
    /// a stale `leader_since` that can no longer be refreshed.
    #[tokio::test]
    async fn leadership_watcher_publishes_none_when_the_metrics_channel_closes() {
        let (metrics_tx, metrics_rx) = tokio::sync::watch::channel(metrics_at(Some(1), 3));
        let (tx, mut rx) = tokio::sync::watch::channel(None);
        tokio::spawn(super::run_leadership_watcher(metrics_rx, tx, 1));

        rx.changed().await.unwrap();
        assert!(rx.borrow_and_update().is_some());

        drop(metrics_tx);
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow_and_update(), None);
    }

    /// Dropping the receiver must stop the task even though leadership never
    /// changes again, which is the only thing that would otherwise wake it.
    #[tokio::test]
    async fn leadership_watcher_exits_when_its_receiver_is_dropped() {
        let (_metrics_tx, metrics_rx) = tokio::sync::watch::channel(metrics_at(Some(1), 3));
        let (tx, mut rx) = tokio::sync::watch::channel(None);
        let task = tokio::spawn(super::run_leadership_watcher(metrics_rx, tx, 1));

        rx.changed().await.unwrap();
        drop(rx);
        task.await.unwrap();
    }

    #[test]
    fn store_corrupt_purged_file_hard_fails() {
        let dir = TempDir::new().unwrap();
        // Create the store once so the raft/ subdir exists, then corrupt purged.json.
        SpurStore::new(dir.path(), noop_applier()).unwrap();
        std::fs::write(
            dir.path().join("raft").join("purged.json"),
            "not valid json",
        )
        .unwrap();

        assert!(SpurStore::new(dir.path(), noop_applier()).is_err());
    }

    /// Mirrors a real restart with a schema-incompatible binary: a WAL entry
    /// whose payload variant this build's `WalOperation` enum doesn't know
    /// about (e.g. written by a newer/older `spurctld`).
    fn write_raw_log_entry(dir: &std::path::Path, index: u64, json: &str) {
        std::fs::write(
            dir.join("raft")
                .join("log")
                .join(format!("{index:020}.json")),
            json,
        )
        .unwrap();
    }

    const UNKNOWN_VARIANT_ENTRY: &str = r#"{"log_id":{"leader_id":{"term":1,"node_id":1},"index":0},"payload":{"Normal":{"SomeFutureVariant":{}}}}"#;

    #[test]
    fn store_unknown_wal_variant_hard_fails_by_default() {
        let dir = TempDir::new().unwrap();
        SpurStore::new(dir.path(), noop_applier()).unwrap();
        write_raw_log_entry(dir.path(), 0, UNKNOWN_VARIANT_ENTRY);

        assert!(SpurStore::new(dir.path(), noop_applier()).is_err());
    }

    #[test]
    fn store_unknown_wal_variant_lenient_mode_skips_and_continues() {
        let dir = TempDir::new().unwrap();
        SpurStore::new(dir.path(), noop_applier()).unwrap();
        write_raw_log_entry(dir.path(), 0, UNKNOWN_VARIANT_ENTRY);

        let store = SpurStore::new_with_recovery_mode(dir.path(), noop_applier(), false).unwrap();
        assert!(store.inner.read().log.is_empty());
    }

    #[test]
    fn store_corrupt_snapshot_hard_fails_by_default() {
        let dir = TempDir::new().unwrap();
        SpurStore::new(dir.path(), noop_applier()).unwrap();
        std::fs::write(
            dir.path().join("raft").join("snapshot.json"),
            "not valid json",
        )
        .unwrap();

        assert!(SpurStore::new(dir.path(), noop_applier()).is_err());
    }

    #[test]
    fn store_corrupt_snapshot_lenient_mode_skips_and_continues() {
        let dir = TempDir::new().unwrap();
        SpurStore::new(dir.path(), noop_applier()).unwrap();
        std::fs::write(
            dir.path().join("raft").join("snapshot.json"),
            "not valid json",
        )
        .unwrap();

        assert!(SpurStore::new_with_recovery_mode(dir.path(), noop_applier(), false).is_ok());
        assert_eq!(
            std::fs::read(snapshot_path(&dir).with_extension("json.unrestorable")).unwrap(),
            b"not valid json"
        );
        assert!(!snapshot_path(&dir).exists());
    }

    #[test]
    fn store_corrupt_vote_file_hard_fails_by_default() {
        let dir = TempDir::new().unwrap();
        SpurStore::new(dir.path(), noop_applier()).unwrap();
        std::fs::write(dir.path().join("raft").join("vote.json"), "not valid json").unwrap();

        assert!(SpurStore::new(dir.path(), noop_applier()).is_err());
    }

    #[test]
    fn store_corrupt_vote_file_lenient_mode_skips_and_continues() {
        let dir = TempDir::new().unwrap();
        SpurStore::new(dir.path(), noop_applier()).unwrap();
        std::fs::write(dir.path().join("raft").join("vote.json"), "not valid json").unwrap();

        let store = SpurStore::new_with_recovery_mode(dir.path(), noop_applier(), false).unwrap();
        assert!(store.inner.read().vote.is_none());
    }

    struct FailingRestoreApplier;
    impl StateMachineApply for FailingRestoreApplier {
        fn apply_operation(&self, _op: &WalOperation) -> ClientResponse {
            ClientResponse::default()
        }
        fn snapshot_state(&self) -> Result<Vec<u8>, anyhow::Error> {
            Ok(Vec::new())
        }
        fn restore_from_snapshot(&self, _data: &[u8]) -> Result<(), anyhow::Error> {
            Err(anyhow::anyhow!("applier rejected snapshot payload"))
        }
    }

    #[test]
    fn store_snapshot_restore_failure_hard_fails_by_default() {
        let dir = TempDir::new().unwrap();
        let store = SpurStore::new(dir.path(), noop_applier()).unwrap();
        let meta = SnapshotMeta {
            last_log_id: Some(LogId {
                leader_id: openraft::LeaderId {
                    term: 1,
                    node_id: 1,
                },
                index: 5,
            }),
            last_membership: StoredMembership::default(),
            snapshot_id: "snap-5".into(),
        };
        store
            .persist_and_cache_snapshot(meta, b"opaque state".to_vec())
            .unwrap();

        assert!(SpurStore::new(dir.path(), Arc::new(FailingRestoreApplier)).is_err());
    }

    #[test]
    fn store_snapshot_restore_failure_lenient_mode_skips_and_continues() {
        let dir = TempDir::new().unwrap();
        let store = SpurStore::new(dir.path(), noop_applier()).unwrap();
        let meta = SnapshotMeta {
            last_log_id: Some(LogId {
                leader_id: openraft::LeaderId {
                    term: 1,
                    node_id: 1,
                },
                index: 5,
            }),
            last_membership: StoredMembership::default(),
            snapshot_id: "snap-5".into(),
        };
        store
            .persist_and_cache_snapshot(meta, b"opaque state".to_vec())
            .unwrap();

        let original = std::fs::read(snapshot_path(&dir)).unwrap();

        let store =
            SpurStore::new_with_recovery_mode(dir.path(), Arc::new(FailingRestoreApplier), false)
                .unwrap();
        // A rejected restore must not advance last_applied/last_membership —
        // otherwise Raft believes the state machine is caught up to the
        // snapshot when it never actually restored anything.
        let inner = store.inner.read();
        assert!(inner.last_applied.is_none());
        assert_eq!(inner.last_membership, StoredMembership::default());

        // The unrestorable bytes are kept aside, not left for a rebuild to overwrite.
        assert!(store.current_snapshot.read().is_none());
        assert!(!snapshot_path(&dir).exists());
        assert_eq!(
            std::fs::read(snapshot_path(&dir).with_extension("json.unrestorable")).unwrap(),
            original
        );
    }

    struct FailingSnapshotApplier;
    impl StateMachineApply for FailingSnapshotApplier {
        fn apply_operation(&self, _op: &WalOperation) -> ClientResponse {
            ClientResponse::default()
        }
        fn snapshot_state(&self) -> Result<Vec<u8>, anyhow::Error> {
            Err(anyhow::anyhow!("snapshot write failed"))
        }
        fn restore_from_snapshot(&self, _data: &[u8]) -> Result<(), anyhow::Error> {
            Ok(())
        }
    }

    async fn start_single_node_leader(
        dir: &Path,
        applier: Arc<dyn StateMachineApply>,
    ) -> RaftHandle {
        let handle = start_raft(1, &["[::1]:0".into()], dir, applier)
            .await
            .unwrap();
        handle
            .raft
            .wait(Some(std::time::Duration::from_secs(5)))
            .metrics(|m| m.current_leader == Some(1), "leader elected")
            .await
            .expect("single-node raft did not self-elect within 5s");
        handle
    }

    #[tokio::test]
    async fn stopped_core_is_not_leader_and_wait_fatal_reports_stopped() {
        let dir = TempDir::new().unwrap();
        let handle = start_single_node_leader(dir.path(), noop_applier()).await;
        assert!(handle.is_leader());

        handle.raft.shutdown().await.unwrap();

        assert!(!handle.is_leader());
        assert_eq!(handle.current_leader(), None);
        assert!(matches!(handle.wait_fatal().await, Fatal::Stopped));
    }

    #[tokio::test]
    async fn storage_fatal_stops_leadership_and_wait_fatal_reports_it() {
        let dir = TempDir::new().unwrap();
        let handle = start_single_node_leader(dir.path(), Arc::new(FailingSnapshotApplier)).await;
        assert!(handle.is_leader());

        handle.raft.trigger().snapshot().await.unwrap();

        assert!(matches!(handle.wait_fatal().await, Fatal::StorageError(_)));
        assert!(!handle.is_leader());
        assert_eq!(handle.current_leader(), None);
    }

    #[tokio::test]
    async fn start_raft_skips_redundant_initialize_on_restart() {
        use std::time::Duration;

        let dir = TempDir::new().unwrap();

        let handle = start_raft(1, &["[::1]:0".into()], dir.path(), noop_applier())
            .await
            .unwrap();
        handle
            .raft
            .wait(Some(Duration::from_secs(5)))
            .metrics(|m| m.current_leader == Some(1), "leader elected")
            .await
            .expect("single-node raft did not self-elect within 5s");
        handle.raft.shutdown().await.unwrap();

        // Restart against the same state_dir: prior vote/log on disk means
        // already_initialized is true, so initialize() must be skipped, yet
        // the node must still become leader and be usable.
        let handle = start_raft(1, &["[::1]:0".into()], dir.path(), noop_applier())
            .await
            .unwrap();
        handle
            .raft
            .wait(Some(Duration::from_secs(5)))
            .metrics(|m| m.current_leader == Some(1), "leader elected")
            .await
            .expect("restarted single-node raft did not resume leadership within 5s");
    }

    /// The HA follower-catch-up path: a leader pushes a snapshot via the Raft
    /// `install_snapshot` RPC. If this node's applier can't apply it (e.g. a
    /// schema-incompatible build in a multi-controller cluster), the RPC must
    /// fail rather than mark the follower caught up on state it doesn't have.
    #[tokio::test]
    async fn install_snapshot_rejects_when_applier_cannot_restore() {
        use openraft::RaftStorage;

        let dir = TempDir::new().unwrap();
        let mut store =
            Arc::new(SpurStore::new(dir.path(), Arc::new(FailingRestoreApplier)).unwrap());

        let meta = SnapshotMeta {
            last_log_id: Some(LogId {
                leader_id: openraft::LeaderId {
                    term: 1,
                    node_id: 2,
                },
                index: 10,
            }),
            last_membership: StoredMembership::default(),
            snapshot_id: "snap-10".into(),
        };

        let result = store
            .install_snapshot(&meta, Box::new(Cursor::new(b"opaque state".to_vec())))
            .await;

        assert!(result.is_err());
        // A rejected snapshot must never reach disk: persisting it first would
        // make the next (strict-by-default) startup hard-fail on it forever.
        assert!(!dir.path().join("raft").join("snapshot.json").exists());
    }

    #[tokio::test]
    async fn append_entries_rejects_oversized_batch_with_payload_too_large() {
        use openraft::network::RPCOption;
        use openraft::RaftNetwork;
        use spur_core::job::JobSpec;

        let mut conn = SpurNetworkConnection {
            target: 99,
            addr: "http://127.0.0.1:1".into(),
            client: None,
        };

        let big_spec = JobSpec {
            script: Some("x".repeat(2 * 1024 * 1024)),
            ..Default::default()
        };
        let entry = Entry::<SpurTypeConfig> {
            log_id: LogId {
                leader_id: openraft::LeaderId {
                    term: 1,
                    node_id: 1,
                },
                index: 1,
            },
            payload: EntryPayload::Normal(WalOperation::JobSubmit {
                job_id: 1,
                spec: Box::new(big_spec),
            }),
        };

        let rpc = openraft::raft::AppendEntriesRequest::<SpurTypeConfig> {
            vote: Vote::new(1, 1),
            prev_log_id: None,
            entries: vec![entry; 20],
            leader_commit: None,
        };

        let option = RPCOption::new(std::time::Duration::from_secs(5));
        let result = conn.append_entries(rpc, option).await;

        let err = result.expect_err("oversized batch should be rejected");
        match err {
            openraft::error::RPCError::PayloadTooLarge(too_large) => {
                assert_eq!(too_large.entries_hint(), 10);
            }
            other => panic!("expected PayloadTooLarge, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn append_entries_accepts_small_batch() {
        use openraft::network::RPCOption;
        use openraft::RaftNetwork;

        let mut conn = SpurNetworkConnection {
            target: 99,
            addr: "http://127.0.0.1:1".into(),
            client: None,
        };

        let entry = Entry::<SpurTypeConfig> {
            log_id: LogId {
                leader_id: openraft::LeaderId {
                    term: 1,
                    node_id: 1,
                },
                index: 1,
            },
            payload: EntryPayload::Blank,
        };

        let rpc = openraft::raft::AppendEntriesRequest::<SpurTypeConfig> {
            vote: Vote::new(1, 1),
            prev_log_id: None,
            entries: vec![entry],
            leader_commit: None,
        };

        let option = RPCOption::new(std::time::Duration::from_secs(5));
        let result = conn.append_entries(rpc, option).await;

        // Small batch passes the size guard but fails to connect (bogus address).
        // The error should be Unreachable (connection failure), NOT PayloadTooLarge.
        let err = result.expect_err("should fail to connect to bogus address");
        assert!(
            matches!(err, openraft::error::RPCError::Unreachable(_)),
            "expected Unreachable, got: {err:?}"
        );
    }
}
