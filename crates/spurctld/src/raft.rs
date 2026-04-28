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

use openraft::storage::{LogState, RaftLogReader, Snapshot, SnapshotMeta};
use openraft::{
    BasicNode, Config, Entry, EntryPayload, LogId, Raft, RaftSnapshotBuilder, StorageError,
    StoredMembership, Vote,
};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use spur_core::wal::WalOperation;

pub type NodeId = u64;

openraft::declare_raft_types!(
    pub SpurTypeConfig:
        D = WalOperation,
        R = ClientResponse,
        Node = BasicNode,
);

pub type SpurRaft = Raft<SpurTypeConfig>;

/// Response returned after a Raft write is committed.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClientResponse;

/// Trait for applying committed Raft entries to the cluster state.
/// Implemented by ClusterManager to avoid a circular dependency with SpurStore.
pub trait StateMachineApply: Send + Sync {
    fn apply_operation(&self, op: &WalOperation);
    fn snapshot_state(&self) -> Result<Vec<u8>, anyhow::Error>;
    fn restore_from_snapshot(&self, data: &[u8]);
}

/// Disk-backed Raft storage.
/// Layout: `{state_dir}/raft/{vote.json, log/*.json, snapshot.json}`
pub struct SpurStore {
    inner: RwLock<StoreInner>,
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

#[derive(Debug, Serialize, Deserialize)]
struct PersistedSnapshot {
    meta: SnapshotMeta<NodeId, BasicNode>,
    data: Vec<u8>,
}

impl SpurStore {
    pub fn new(state_dir: &Path, applier: Arc<dyn StateMachineApply>) -> anyhow::Result<Self> {
        let raft_dir = state_dir.join("raft");
        let log_dir = raft_dir.join("log");
        std::fs::create_dir_all(&log_dir)?;

        let mut inner = StoreInner::default();

        let vote_path = raft_dir.join("vote.json");
        if vote_path.exists() {
            match std::fs::read_to_string(&vote_path) {
                Ok(data) => match serde_json::from_str(&data) {
                    Ok(v) => inner.vote = Some(v),
                    Err(e) => warn!("failed to parse vote.json: {e}"),
                },
                Err(e) => warn!("failed to read vote.json: {e}"),
            }
        }

        if let Ok(entries) = std::fs::read_dir(&log_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map_or(false, |e| e == "json") {
                    match std::fs::read_to_string(&path) {
                        Ok(data) => match serde_json::from_str::<Entry<SpurTypeConfig>>(&data) {
                            Ok(e) => {
                                inner.log.insert(e.log_id.index, e);
                            }
                            Err(e) => warn!("failed to parse log entry {:?}: {e}", path),
                        },
                        Err(e) => warn!("failed to read log entry {:?}: {e}", path),
                    }
                }
            }
        }

        let snap_path = raft_dir.join("snapshot.json");
        if snap_path.exists() {
            match std::fs::read_to_string(&snap_path) {
                Ok(data) => match serde_json::from_str::<PersistedSnapshot>(&data) {
                    Ok(ps) => {
                        inner.last_applied = ps.meta.last_log_id;
                        inner.last_membership = ps.meta.last_membership.clone();
                        applier.restore_from_snapshot(&ps.data);
                    }
                    Err(e) => warn!("failed to parse snapshot.json: {e}"),
                },
                Err(e) => warn!("failed to read snapshot.json: {e}"),
            }
        }

        info!(
            log_entries = inner.log.len(),
            vote = ?inner.vote,
            "raft store recovered from disk"
        );

        Ok(Self {
            inner: RwLock::new(inner),
            raft_dir,
            applier,
        })
    }

    fn persist_vote(&self, vote: &Vote<NodeId>) -> Result<(), std::io::Error> {
        let path = self.raft_dir.join("vote.json");
        let data = serde_json::to_vec(vote)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(&path, &data)
            .map_err(|e| std::io::Error::new(e.kind(), format!("{path:?}: {e}")))
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

    fn persist_snapshot(
        &self,
        meta: &SnapshotMeta<NodeId, BasicNode>,
        data: &[u8],
    ) -> Result<(), std::io::Error> {
        let ps = PersistedSnapshot {
            meta: meta.clone(),
            data: data.to_vec(),
        };
        let path = self.raft_dir.join("snapshot.json");
        let json = serde_json::to_vec(&ps)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(&path, &json)
            .map_err(|e| std::io::Error::new(e.kind(), format!("{path:?}: {e}")))
    }

    fn load_persisted_snapshot(&self) -> Option<PersistedSnapshot> {
        let path = self.raft_dir.join("snapshot.json");
        if !path.exists() {
            return None;
        }
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|data| serde_json::from_str(&data).ok())
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
        let inner = self.inner.read();

        let snapshot_data = self.applier.snapshot_state().map_err(|e| {
            StorageError::from_io_error(
                openraft::ErrorSubject::Store,
                openraft::ErrorVerb::Read,
                std::io::Error::new(std::io::ErrorKind::Other, e),
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

        self.persist_snapshot(&meta, &snapshot_data).map_err(|e| {
            StorageError::from_io_error(
                openraft::ErrorSubject::Store,
                openraft::ErrorVerb::Write,
                e,
            )
        })?;

        Ok(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(snapshot_data)),
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
        self.inner.write().vote = Some(vote.clone());
        Ok(())
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<NodeId>>, StorageError<NodeId>> {
        Ok(self.inner.read().vote.clone())
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
                    self.applier.apply_operation(op);
                }
                EntryPayload::Membership(mem) => {
                    inner.last_membership = StoredMembership::new(Some(entry.log_id), mem.clone());
                }
                EntryPayload::Blank => {}
            }
            results.push(ClientResponse);
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
        self.persist_snapshot(meta, &data).map_err(|e| {
            StorageError::from_io_error(
                openraft::ErrorSubject::Store,
                openraft::ErrorVerb::Write,
                e,
            )
        })?;

        self.applier.restore_from_snapshot(&data);

        let mut inner = self.inner.write();
        inner.last_applied = meta.last_log_id;
        inner.last_membership = meta.last_membership.clone();
        info!(last_applied = ?meta.last_log_id, "installed snapshot from leader");
        Ok(())
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<Snapshot<SpurTypeConfig>>, StorageError<NodeId>> {
        if let Some(ps) = self.load_persisted_snapshot() {
            Ok(Some(Snapshot {
                meta: ps.meta,
                snapshot: Box::new(Cursor::new(ps.data)),
            }))
        } else {
            Ok(None)
        }
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
            self.client = Some(
                spur_proto::raft_proto::raft_internal_client::RaftInternalClient::new(channel),
            );
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
    pub fn is_leader(&self) -> bool {
        let metrics = self.raft.metrics().borrow().clone();
        metrics.current_leader == Some(self.node_id)
    }

    pub fn current_leader(&self) -> Option<NodeId> {
        self.raft.metrics().borrow().current_leader
    }
}

/// Auto-detect node_id from hostname ordinal (e.g., "spurctld-2" → 3).
pub fn detect_node_id_from_hostname() -> Option<u64> {
    let hostname = hostname::get().ok()?;
    node_id_from_hostname(&hostname.to_string_lossy())
}

/// Parse a node_id from a hostname string. The ordinal after the last '-'
/// is treated as a 0-based index and converted to a 1-based node_id.
pub fn node_id_from_hostname(hostname: &str) -> Option<u64> {
    let ordinal: u64 = hostname.rsplit('-').next()?.parse().ok()?;
    Some(ordinal + 1)
}

/// Build a 1-indexed peer map from the config peers list.
pub fn build_peer_map(peers: &[String]) -> BTreeMap<NodeId, String> {
    peers
        .iter()
        .enumerate()
        .map(|(i, addr)| (i as u64 + 1, addr.clone()))
        .collect()
}

pub async fn start_raft(
    node_id: NodeId,
    peers: &[String],
    state_dir: &Path,
    applier: Arc<dyn StateMachineApply>,
) -> anyhow::Result<RaftHandle> {
    let config = Config {
        heartbeat_interval: 500,
        election_timeout_min: 1500,
        election_timeout_max: 3000,
        ..Default::default()
    };
    let config = Arc::new(config.validate().map_err(|e| anyhow::anyhow!("{e}"))?);

    let store = Arc::new(SpurStore::new(state_dir, applier)?);
    let peer_map = build_peer_map(peers);
    let network = Arc::new(SpurNetwork {
        peers: peer_map.clone(),
    });

    let (log_store, state_machine) = openraft::storage::Adaptor::new(store.clone());
    let raft = Raft::new(node_id, config, network, log_store, state_machine).await?;

    // Symmetric bootstrap: every node calls initialize with the full
    // membership. Openraft guarantees that when all nodes use the same
    // membership, the voting protocol picks exactly one leader. On
    // subsequent restarts initialize() returns "already initialized"
    // (benign) and normal Raft elections take over.
    let members: BTreeMap<NodeId, BasicNode> = peer_map
        .iter()
        .map(|(id, addr)| (*id, BasicNode::new(addr.clone())))
        .collect();
    if let Err(e) = raft.initialize(members).await {
        debug!("raft initialize: {e} (already initialized)");
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
        fn apply_operation(&self, _op: &WalOperation) {}
        fn snapshot_state(&self) -> Result<Vec<u8>, anyhow::Error> {
            Ok(Vec::new())
        }
        fn restore_from_snapshot(&self, _data: &[u8]) {}
    }

    fn noop_applier() -> Arc<dyn StateMachineApply> {
        Arc::new(NoopApplier)
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
        let data = b"test snapshot data";
        store.persist_snapshot(&meta, data).unwrap();

        let loaded = store.load_persisted_snapshot().unwrap();
        assert_eq!(loaded.meta.snapshot_id, "snap-100");
        assert_eq!(loaded.data, data);
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
}
