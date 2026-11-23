// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! spurstepd's per-step checkpoint socket: accepts one event per connection
//! from the job's own workload, attributes the sender, and folds the event
//! into the step's `checkpoint.json` record. Never allowed to fail, block,
//! or delay the step it watches — every error here is logged and swallowed
//! by the caller.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use nix::sys::socket::{AddressFamily, Backlog, SockFlag, SockType, UnixAddr};
use nix::unistd::{Gid, Uid};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Semaphore};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Duration, Instant};

use spur_core::checkpoint::{
    parse_event, CheckpointRecord, DropReason, Verification, MAX_EVENT_BYTES,
};
use spur_core::step::StepId;

pub const RECORD_FILE: &str = "checkpoint.json";
pub const MAX_IN_FLIGHT: usize = 128;
pub const RATE_BURST: u32 = 64;
pub const RATE_PER_SEC: u32 = 16;
pub const READ_DEADLINE: Duration = Duration::from_millis(100);
pub const PERSIST_INTERVAL: Duration = Duration::from_secs(5);

/// A dropped-event reason is silenced for this long after it has already
/// logged once for a given listener.
const DROP_LOG_INTERVAL: Duration = Duration::from_secs(60);

/// The listen backlog floor the Global Constraints require.
const LISTEN_BACKLOG: i32 = 1024;

/// Delay before retrying `accept()` after a transient error, so a persistent
/// one (e.g. the fd table staying full) does not spin the accept loop.
const ACCEPT_RETRY_BACKOFF: Duration = Duration::from_millis(50);

/// How long `finish()` waits for in-flight connections to apply their event
/// before forcing them closed, so a stuck client cannot delay `StepdCompleted`.
const CONNECTION_DRAIN_TIMEOUT: Duration =
    Duration::from_millis(READ_DEADLINE.as_millis() as u64 * 2);

/// A few KiB — enough to flush what a well-behaved (but oversize) sender left
/// unread, without looping unbounded if a client keeps writing after its ack.
const DRAIN_CAP_BYTES: usize = 8192;

pub type CgroupLookup = Arc<dyn Fn(i32) -> io::Result<String> + Send + Sync>;

/// Reads the unified-hierarchy (`0::`) line of `/proc/<pid>/cgroup`.
pub fn proc_cgroup_lookup() -> CgroupLookup {
    Arc::new(|pid: i32| {
        let content = fs::read_to_string(format!("/proc/{pid}/cgroup"))?;
        parse_unified_cgroup(&content)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no unified (0::) cgroup line"))
    })
}

/// The path after a `/proc/<pid>/cgroup` listing's `0::` line. `None` on a
/// purely legacy (cgroup v1-only) hierarchy, which carries no unified line.
fn parse_unified_cgroup(content: &str) -> Option<String> {
    content
        .lines()
        .find_map(|line| line.strip_prefix("0::").map(str::to_string))
}

pub struct ListenerSpec {
    pub socket: PathBuf,
    pub session_dir: PathBuf,
    pub job_id: u32,
    pub step_id: StepId,
    pub uid: u32,
    pub gid: u32,
    pub dir_owner_uid: u32,
    pub expected_cgroup: String,
    pub cgroup_lookup: CgroupLookup,
}

/// What every connection handler reads and may mutate: the record itself and
/// this step's shared token bucket, so a rate limit spans every connection.
#[derive(Debug)]
struct ListenerState {
    record: CheckpointRecord,
    dirty: bool,
    tokens: TokenBucket,
}

/// Fixed per-listener context, cloned (via `Arc`) into every connection task.
struct ConnCtx {
    job_id: u32,
    step_id: StepId,
    uid: u32,
    expected_cgroup: String,
    cgroup_lookup: CgroupLookup,
    state: Arc<Mutex<ListenerState>>,
    drop_log: Mutex<DropLog>,
}

#[derive(Debug)]
pub struct CheckpointListener {
    accept_handle: JoinHandle<()>,
    persist_handle: JoinHandle<()>,
    connections: Arc<Mutex<JoinSet<()>>>,
    state: Arc<Mutex<ListenerState>>,
    socket: PathBuf,
    session_dir: PathBuf,
}

impl CheckpointListener {
    /// Validates the socket's parent, binds, chowns/chmods the socket to the
    /// job, and spawns the accept loop and the debounced persist ticker.
    pub fn start(spec: ListenerSpec) -> io::Result<Self> {
        let parent = spec.socket.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "checkpoint socket has no parent directory",
            )
        })?;
        crate::checkpoint_dir::validate_socket_parent(parent, spec.dir_owner_uid)?;

        // A socket left behind by a prior attempt in this same session slot;
        // bind would otherwise fail with AddrInUse.
        match fs::remove_file(&spec.socket) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }

        let fd = nix::sys::socket::socket(
            AddressFamily::Unix,
            SockType::Stream,
            SockFlag::empty(),
            None::<nix::sys::socket::SockProtocol>,
        )?;
        let addr = UnixAddr::new(&spec.socket)?;
        nix::sys::socket::bind(fd.as_raw_fd(), &addr)?;
        nix::sys::socket::listen(&fd, Backlog::new(LISTEN_BACKLOG)?)?;
        nix::unistd::chown(
            &spec.socket,
            Some(Uid::from_raw(spec.uid)),
            Some(Gid::from_raw(spec.gid)),
        )?;
        fs::set_permissions(&spec.socket, fs::Permissions::from_mode(0o600))?;

        let std_listener: std::os::unix::net::UnixListener = fd.into();
        std_listener.set_nonblocking(true)?;
        let listener = UnixListener::from_std(std_listener)?;

        let state = Arc::new(Mutex::new(ListenerState {
            record: CheckpointRecord::default(),
            dirty: false,
            tokens: TokenBucket::new(Instant::now()),
        }));
        let ctx = Arc::new(ConnCtx {
            job_id: spec.job_id,
            step_id: spec.step_id,
            uid: spec.uid,
            expected_cgroup: spec.expected_cgroup,
            cgroup_lookup: spec.cgroup_lookup,
            state: state.clone(),
            drop_log: Mutex::new(DropLog::default()),
        });
        let connections: Arc<Mutex<JoinSet<()>>> = Arc::new(Mutex::new(JoinSet::new()));
        let semaphore = Arc::new(Semaphore::new(MAX_IN_FLIGHT));

        let accept_handle = {
            let connections = connections.clone();
            let ctx = ctx.clone();
            tokio::spawn(async move {
                loop {
                    let (stream, _addr) = match listener.accept().await {
                        Ok(pair) => pair,
                        Err(error) => {
                            // Transient (e.g. EMFILE/ENFILE/ECONNABORTED): the
                            // listener itself is still fine, so keep serving
                            // the step rather than fail-closed on one bad accept.
                            tracing::warn!(%error, "checkpoint listener accept failed; retrying");
                            tokio::time::sleep(ACCEPT_RETRY_BACKOFF).await;
                            continue;
                        }
                    };
                    match semaphore.clone().try_acquire_owned() {
                        Ok(permit) => {
                            let ctx = ctx.clone();
                            let mut connections = connections.lock().await;
                            // Finished tasks are only joined in finish() otherwise,
                            // so a long-running step would otherwise accumulate one
                            // JoinSet entry per connection for its whole lifetime.
                            while connections.try_join_next().is_some() {}
                            connections.spawn(async move {
                                handle_connection(stream, ctx).await;
                                drop(permit);
                            });
                        }
                        Err(_) => {
                            // Over capacity: close unread rather than make the
                            // sender wait on a read that competes with others.
                            record_drop(&ctx, DropReason::Busy).await;
                            drop(stream);
                        }
                    }
                }
            })
        };

        let persist_handle = {
            let state = state.clone();
            let session_dir = spec.session_dir.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(PERSIST_INTERVAL).await;
                    let mut guard = state.lock().await;
                    if !guard.dirty {
                        continue;
                    }
                    match serde_json::to_vec_pretty(&guard.record) {
                        Ok(json) => {
                            if let Err(error) =
                                crate::stepd::publish_private(&session_dir, RECORD_FILE, &json)
                            {
                                tracing::warn!(%error, "failed to persist checkpoint.json");
                            } else {
                                guard.dirty = false;
                            }
                        }
                        Err(error) => {
                            tracing::warn!(%error, "failed to encode the checkpoint record")
                        }
                    }
                }
            })
        };

        Ok(Self {
            accept_handle,
            persist_handle,
            connections,
            state,
            socket: spec.socket,
            session_dir: spec.session_dir,
        })
    }

    /// Stops accepting, lets in-flight connections finish applying their
    /// event, then writes the final (`is_final = true`) record and removes
    /// the socket.
    pub async fn finish(self) {
        self.accept_handle.abort();
        let _ = self.accept_handle.await;
        self.persist_handle.abort();
        let _ = self.persist_handle.await;

        {
            let mut connections = self.connections.lock().await;
            if tokio::time::timeout(CONNECTION_DRAIN_TIMEOUT, drain_all(&mut connections))
                .await
                .is_err()
            {
                // A connection outlived its own read deadline somehow; force
                // it rather than let a stuck client delay StepdCompleted.
                connections.abort_all();
                let _ = tokio::time::timeout(CONNECTION_DRAIN_TIMEOUT, drain_all(&mut connections))
                    .await;
            }
        }

        let mut guard = self.state.lock().await;
        guard.record.is_final = true;
        match serde_json::to_vec_pretty(&guard.record) {
            Ok(json) => {
                if let Err(error) =
                    crate::stepd::publish_private(&self.session_dir, RECORD_FILE, &json)
                {
                    tracing::warn!(%error, "failed to persist the final checkpoint record");
                }
            }
            Err(error) => tracing::warn!(%error, "failed to encode the final checkpoint record"),
        }
        drop(guard);
        let _ = fs::remove_file(&self.socket);
    }
}

async fn drain_all(connections: &mut JoinSet<()>) {
    while connections.join_next().await.is_some() {}
}

/// Reads `checkpoint.json`; `Ok(None)` if it was never written.
pub fn load_record(session_dir: &Path) -> io::Result<Option<CheckpointRecord>> {
    match fs::read(session_dir.join(RECORD_FILE)) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

async fn handle_connection(mut stream: UnixStream, ctx: Arc<ConnCtx>) {
    let verification = verify_peer(&stream, &ctx);
    let read_result = read_one_event(&mut stream).await;
    // Always the same byte regardless of outcome: the ack must not leak why
    // (or whether) an event was accepted.
    let _ = stream.write_all(b"k").await;
    let _ = stream.shutdown().await;
    // An oversize sender can still have unread bytes sitting in the kernel
    // buffer; closing over those aborts the connection and can discard the
    // ack already written above instead of delivering it. This only drains
    // what is already buffered — it never waits for more to arrive.
    drain_unread(&stream);

    if let Some(reason) = finalize(&ctx, verification, read_result).await {
        log_drop(&ctx, reason).await;
    }
}

fn drain_unread(stream: &UnixStream) {
    let mut discard = [0u8; 512];
    let mut drained = 0usize;
    while drained < DRAIN_CAP_BYTES {
        match stream.try_read(&mut discard) {
            Ok(0) | Err(_) => break,
            Ok(n) => drained += n,
        }
    }
}

fn verify_peer(stream: &UnixStream, ctx: &ConnCtx) -> Result<Verification, DropReason> {
    let cred = stream.peer_cred().map_err(|_| DropReason::UidMismatch)?;
    if cred.uid() != ctx.uid {
        return Err(DropReason::UidMismatch);
    }
    let Some(pid) = cred.pid() else {
        // No pid reported on this platform: uid already matched and a live
        // listener means the step is still active.
        return Ok(Verification::Uid);
    };
    match (ctx.cgroup_lookup)(pid) {
        Ok(path) if path == ctx.expected_cgroup => Ok(Verification::Cgroup),
        Ok(_) => Err(DropReason::NotJobProcess),
        // The sender may have exited before its cgroup could be read.
        Err(_) => Ok(Verification::Uid),
    }
}

/// Reads at most `MAX_EVENT_BYTES + 1` bytes (enough to tell a newline-
/// terminated event from an oversize one) in as few syscalls as the kernel
/// buffer allows, under one deadline spanning every read. Anything after the
/// first `\n` in a single chunk is dropped — a sender putting more than one
/// event on a connection trades its rest for `Schema`/`JobMismatch`, not for
/// having it treated as a second event.
async fn read_one_event(stream: &mut UnixStream) -> Result<Vec<u8>, DropReason> {
    let deadline = Instant::now() + READ_DEADLINE;
    let mut buf = vec![0u8; MAX_EVENT_BYTES + 1];
    let mut filled = 0usize;
    while filled < buf.len() {
        match tokio::time::timeout_at(deadline, stream.read(&mut buf[filled..])).await {
            Err(_) => return Err(DropReason::ReadTimeout),
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                let chunk_start = filled;
                filled += n;
                if let Some(pos) = buf[chunk_start..filled].iter().position(|&b| b == b'\n') {
                    filled = chunk_start + pos + 1;
                    break;
                }
            }
            Ok(Err(_)) => break,
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

/// Applies one read event under a single lock, so the token bucket and the
/// record it guards cannot race a sibling connection's event.
async fn finalize(
    ctx: &ConnCtx,
    verification: Result<Verification, DropReason>,
    read_result: Result<Vec<u8>, DropReason>,
) -> Option<DropReason> {
    // Checked first: a spoofed uid or foreign cgroup must be reported as
    // such even when the sender also stalls, not masked by its read outcome.
    let verification = match verification {
        Err(reason) => return Some(record_drop(ctx, reason).await),
        Ok(verification) => verification,
    };
    let data = match read_result {
        Err(reason) => return Some(record_drop(ctx, reason).await),
        Ok(data) => data,
    };
    let event = match parse_event(&data) {
        Err(reason) => return Some(record_drop(ctx, reason).await),
        Ok(event) => event,
    };
    let step_mismatch = event.step_id.is_some_and(|step_id| step_id != ctx.step_id);
    if event.job_id != ctx.job_id || step_mismatch {
        return Some(record_drop(ctx, DropReason::JobMismatch).await);
    }

    let mut guard = ctx.state.lock().await;
    if !guard.tokens.try_consume(Instant::now()) {
        guard.record.record_drop(DropReason::RateLimited);
        guard.dirty = true;
        return Some(DropReason::RateLimited);
    }
    if let Err(reason) = guard.record.apply(&event, verification, Utc::now()) {
        guard.record.record_drop(reason);
        guard.dirty = true;
        return Some(reason);
    }
    guard.dirty = true;
    None
}

async fn record_drop(ctx: &ConnCtx, reason: DropReason) -> DropReason {
    let mut guard = ctx.state.lock().await;
    guard.record.record_drop(reason);
    guard.dirty = true;
    reason
}

async fn log_drop(ctx: &ConnCtx, reason: DropReason) {
    let now = Instant::now();
    let should_log = ctx.drop_log.lock().await.should_log(reason, now);
    if should_log {
        // Never the event contents — only identity and the reason it failed.
        tracing::warn!(
            job_id = ctx.job_id,
            step_id = ctx.step_id,
            uid = ctx.uid,
            reason = reason.as_str(),
            "dropped a checkpoint event"
        );
    }
}

#[derive(Default)]
struct DropLog {
    last_logged: HashMap<DropReason, Instant>,
}

impl DropLog {
    fn should_log(&mut self, reason: DropReason, now: Instant) -> bool {
        let silenced = self
            .last_logged
            .get(&reason)
            .is_some_and(|last| now.saturating_duration_since(*last) < DROP_LOG_INTERVAL);
        if silenced {
            return false;
        }
        self.last_logged.insert(reason, now);
        true
    }
}

/// Refills at `RATE_PER_SEC` tokens/sec up to `RATE_BURST`, keyed off
/// `tokio::time::Instant` so paused-time tests control it exactly.
#[derive(Debug)]
struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
}

impl TokenBucket {
    fn new(now: Instant) -> Self {
        Self {
            tokens: RATE_BURST as f64,
            last_refill: now,
        }
    }

    fn try_consume(&mut self, now: Instant) -> bool {
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        self.last_refill = now;
        self.tokens = (self.tokens + elapsed * RATE_PER_SEC as f64).min(RATE_BURST as f64);
        if self.tokens < 1.0 {
            return false;
        }
        self.tokens -= 1.0;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn current_ids() -> (u32, u32) {
        (
            nix::unistd::getuid().as_raw(),
            nix::unistd::getgid().as_raw(),
        )
    }

    struct Fixture {
        _state_dir: tempfile::TempDir,
        session_dir: PathBuf,
        socket: PathBuf,
    }

    fn fixture() -> Fixture {
        let state_dir = tempfile::tempdir().expect("tempdir");
        // Stands in for the <cluster> dir ancestor validate_socket_parent
        // also checks; give it a well-formed, non-writable mode of its own.
        fs::set_permissions(state_dir.path(), fs::Permissions::from_mode(0o755))
            .expect("chmod state dir");
        let checkpoint_dir = state_dir.path().join("checkpoint");
        fs::create_dir(&checkpoint_dir).expect("create checkpoint dir");
        fs::set_permissions(&checkpoint_dir, fs::Permissions::from_mode(0o711))
            .expect("chmod checkpoint dir");
        let session_dir = state_dir.path().join("session");
        fs::create_dir(&session_dir).expect("create session dir");
        let socket = checkpoint_dir.join("7.1.0.sock");
        Fixture {
            _state_dir: state_dir,
            session_dir,
            socket,
        }
    }

    fn spec(fx: &Fixture, cgroup_lookup: CgroupLookup) -> ListenerSpec {
        let (uid, gid) = current_ids();
        ListenerSpec {
            socket: fx.socket.clone(),
            session_dir: fx.session_dir.clone(),
            job_id: 7,
            step_id: 0,
            uid,
            gid,
            dir_owner_uid: uid,
            expected_cgroup: "/expected".to_string(),
            cgroup_lookup,
        }
    }

    fn always(path: &'static str) -> CgroupLookup {
        Arc::new(move |_pid: i32| Ok(path.to_string()))
    }

    fn always_err() -> CgroupLookup {
        Arc::new(|_pid: i32| Err(io::Error::new(io::ErrorKind::NotFound, "no cgroup")))
    }

    /// Gives already-woken background tasks (the persist ticker, the accept
    /// loop draining a batch of connections) enough scheduler turns to run
    /// to completion before a paused-time test inspects state that only
    /// they mutate.
    async fn settle() {
        for _ in 0..4096 {
            tokio::task::yield_now().await;
        }
    }

    async fn send_and_ack(socket: &Path, payload: &[u8]) {
        let mut stream = UnixStream::connect(socket).await.expect("connect");
        stream.write_all(payload).await.expect("write event");
        let mut ack = [0u8; 1];
        stream.read_exact(&mut ack).await.expect("read ack");
        assert_eq!(ack, [b'k']);
    }

    fn begin_json(job_id: u32, step_id: StepId, step: u64) -> Vec<u8> {
        format!(
            r#"{{"v":1,"kind":"checkpoint-begin","step":{step},"job_id":"{job_id}","step_id":"{step_id}"}}"#
        )
        .into_bytes()
        .into_iter()
        .chain(std::iter::once(b'\n'))
        .collect()
    }

    fn end_json(job_id: u32, step_id: StepId, step: u64, ok: bool) -> Vec<u8> {
        let status = if ok { "ok" } else { "failed" };
        format!(
            r#"{{"v":1,"kind":"checkpoint-end","step":{step},"status":"{status}","job_id":"{job_id}","step_id":"{step_id}"}}"#
        )
        .into_bytes()
        .into_iter()
        .chain(std::iter::once(b'\n'))
        .collect()
    }

    /// Like `begin_json`, but omits `step_id` entirely -- what a plain
    /// `sbatch` batch step sends, since it has no `SPUR_STEP_ID`.
    fn begin_json_without_step_id(job_id: u32, step: u64) -> Vec<u8> {
        format!(r#"{{"v":1,"kind":"checkpoint-begin","step":{step},"job_id":"{job_id}"}}"#)
            .into_bytes()
            .into_iter()
            .chain(std::iter::once(b'\n'))
            .collect()
    }

    #[tokio::test]
    async fn accepted_begin_and_end_complete_a_checkpoint() {
        let fx = fixture();
        let listener = CheckpointListener::start(spec(&fx, always("/expected"))).expect("start");

        send_and_ack(&fx.socket, &begin_json(7, 0, 10)).await;
        send_and_ack(&fx.socket, &end_json(7, 0, 10, true)).await;
        listener.finish().await;

        let record = load_record(&fx.session_dir)
            .expect("load")
            .expect("a record was written");
        assert_eq!(record.completed, 1);
        assert!(record.is_final);
    }

    #[tokio::test]
    async fn client_always_receives_the_single_ack_byte() {
        let fx = fixture();
        let listener = CheckpointListener::start(spec(&fx, always("/expected"))).expect("start");

        for payload in [begin_json(7, 0, 1), b"not json\n".to_vec(), vec![b'x'; 600]] {
            let mut stream = UnixStream::connect(&fx.socket).await.expect("connect");
            stream.write_all(&payload).await.expect("write");
            let mut ack = Vec::new();
            stream.read_to_end(&mut ack).await.expect("read to end");
            assert_eq!(ack, b"k");
        }
        listener.finish().await;
    }

    #[tokio::test]
    async fn foreign_cgroup_drops_as_not_job_process_without_applying() {
        let fx = fixture();
        let listener = CheckpointListener::start(spec(&fx, always("/other"))).expect("start");

        send_and_ack(&fx.socket, &begin_json(7, 0, 1)).await;
        listener.finish().await;

        let record = load_record(&fx.session_dir)
            .expect("load")
            .expect("a record was written");
        assert_eq!(record.dropped.get(DropReason::NotJobProcess), 1);
        assert!(record.in_progress_step.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn verification_failure_outranks_a_concurrent_read_timeout() {
        let fx = fixture();
        // Foreign cgroup (NotJobProcess) *and* a client that never sends
        // anything (ReadTimeout): the spoof reason must win either way.
        let listener = CheckpointListener::start(spec(&fx, always("/other"))).expect("start");

        let stream = UnixStream::connect(&fx.socket).await.expect("connect");
        tokio::time::advance(READ_DEADLINE + Duration::from_millis(10)).await;
        drop(stream);

        listener.finish().await;
        let record = load_record(&fx.session_dir)
            .expect("load")
            .expect("a record was written");
        assert_eq!(record.dropped.get(DropReason::NotJobProcess), 1);
        assert_eq!(record.dropped.get(DropReason::ReadTimeout), 0);
    }

    #[tokio::test]
    async fn cgroup_lookup_error_applies_with_uid_verification() {
        let fx = fixture();
        let listener = CheckpointListener::start(spec(&fx, always_err())).expect("start");

        send_and_ack(&fx.socket, &begin_json(7, 0, 1)).await;
        listener.finish().await;

        let record = load_record(&fx.session_dir)
            .expect("load")
            .expect("a record was written");
        assert_eq!(record.uid_verified, 1);
        assert_eq!(record.in_progress_step, Some(1));
    }

    #[tokio::test]
    async fn mismatched_job_id_drops_as_job_mismatch() {
        let fx = fixture();
        let listener = CheckpointListener::start(spec(&fx, always("/expected"))).expect("start");

        send_and_ack(&fx.socket, &begin_json(999, 0, 1)).await;
        listener.finish().await;

        let record = load_record(&fx.session_dir)
            .expect("load")
            .expect("a record was written");
        assert_eq!(record.dropped.get(DropReason::JobMismatch), 1);
        assert_eq!(record.completed, 0);
        assert!(record.in_progress_step.is_none());
    }

    #[tokio::test]
    async fn an_event_without_step_id_is_applied() {
        let fx = fixture();
        let listener = CheckpointListener::start(spec(&fx, always("/expected"))).expect("start");

        send_and_ack(&fx.socket, &begin_json_without_step_id(7, 1)).await;
        listener.finish().await;

        let record = load_record(&fx.session_dir)
            .expect("load")
            .expect("a record was written");
        assert_eq!(record.dropped.total(), 0);
        assert_eq!(record.in_progress_step, Some(1));
    }

    #[tokio::test]
    async fn mismatched_step_id_drops_as_job_mismatch() {
        let fx = fixture();
        let listener = CheckpointListener::start(spec(&fx, always("/expected"))).expect("start");

        send_and_ack(&fx.socket, &begin_json(7, 1, 1)).await;
        listener.finish().await;

        let record = load_record(&fx.session_dir)
            .expect("load")
            .expect("a record was written");
        assert_eq!(record.dropped.get(DropReason::JobMismatch), 1);
        assert!(record.in_progress_step.is_none());
    }

    #[tokio::test]
    async fn malformed_event_drops_as_malformed_json() {
        let fx = fixture();
        let listener = CheckpointListener::start(spec(&fx, always("/expected"))).expect("start");

        send_and_ack(&fx.socket, b"not json\n").await;
        listener.finish().await;

        let record = load_record(&fx.session_dir)
            .expect("load")
            .expect("a record was written");
        assert_eq!(record.dropped.get(DropReason::MalformedJson), 1);
    }

    #[tokio::test]
    async fn oversize_event_drops_as_oversize() {
        let fx = fixture();
        let listener = CheckpointListener::start(spec(&fx, always("/expected"))).expect("start");

        send_and_ack(&fx.socket, &vec![b'{'; 600]).await;
        listener.finish().await;

        let record = load_record(&fx.session_dir)
            .expect("load")
            .expect("a record was written");
        assert_eq!(record.dropped.get(DropReason::Oversize), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn silent_client_drops_as_read_timeout() {
        let fx = fixture();
        let listener = CheckpointListener::start(spec(&fx, always("/expected"))).expect("start");

        let mut stream = UnixStream::connect(&fx.socket).await.expect("connect");
        tokio::time::advance(READ_DEADLINE + Duration::from_millis(10)).await;
        let mut ack = [0u8; 1];
        stream.read_exact(&mut ack).await.expect("read ack");
        assert_eq!(ack, [b'k']);
        drop(stream);

        listener.finish().await;
        let record = load_record(&fx.session_dir)
            .expect("load")
            .expect("a record was written");
        assert_eq!(record.dropped.get(DropReason::ReadTimeout), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn rapid_events_past_the_burst_are_rate_limited() {
        let fx = fixture();
        let listener = CheckpointListener::start(spec(&fx, always("/expected"))).expect("start");

        let extra = 5;
        for _ in 0..(RATE_BURST + extra) {
            send_and_ack(&fx.socket, &begin_json(7, 0, 10)).await;
        }
        listener.finish().await;

        let record = load_record(&fx.session_dir)
            .expect("load")
            .expect("a record was written");
        assert_eq!(
            record.dropped.get(DropReason::RateLimited),
            u64::from(extra)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_connection_over_capacity_is_closed_unread_as_busy() {
        let fx = fixture();
        let listener = CheckpointListener::start(spec(&fx, always("/expected"))).expect("start");

        // Held open, never sending anything, so each permit stays taken for
        // the rest of the test. No tokio::time::advance anywhere in this
        // test: advancing would let these hit their own READ_DEADLINE and
        // free their permits (recording read_timeout), which would make the
        // over-capacity snapshot below meaningless.
        let mut held = Vec::with_capacity(MAX_IN_FLIGHT);
        for _ in 0..MAX_IN_FLIGHT {
            held.push(UnixStream::connect(&fx.socket).await.expect("connect"));
        }
        let mut extra = UnixStream::connect(&fx.socket).await.expect("connect");

        // Let the accept loop actually drain the backlog in order and
        // acquire all MAX_IN_FLIGHT permits before the 129th is looked at.
        settle().await;

        // Busy: closed unread, with no ack ever written.
        let mut probe = [0u8; 1];
        let eof = extra
            .read(&mut probe)
            .await
            .expect("read after a busy close");
        assert_eq!(eof, 0, "a busy connection is closed without an ack");

        let record = listener.state.lock().await.record.clone();
        assert_eq!(record.dropped.get(DropReason::Busy), 1);
        assert_eq!(
            record.dropped.total(),
            1,
            "none of the held connections are counted"
        );

        drop(held);
    }

    #[tokio::test(start_paused = true)]
    async fn persist_ticker_debounces_and_catches_up_on_the_next_tick() {
        let fx = fixture();
        let listener = CheckpointListener::start(spec(&fx, always("/expected"))).expect("start");

        send_and_ack(&fx.socket, &begin_json(7, 0, 1)).await;
        send_and_ack(&fx.socket, &end_json(7, 0, 1, true)).await;
        assert!(
            load_record(&fx.session_dir).expect("load").is_none(),
            "nothing is persisted before the first tick"
        );

        tokio::time::advance(PERSIST_INTERVAL + Duration::from_millis(10)).await;
        settle().await;
        let first = load_record(&fx.session_dir)
            .expect("load")
            .expect("the first tick persists both events");
        assert_eq!(first.completed, 1);

        send_and_ack(&fx.socket, &begin_json(7, 0, 2)).await;
        tokio::time::advance(PERSIST_INTERVAL + Duration::from_millis(10)).await;
        settle().await;
        let second = load_record(&fx.session_dir)
            .expect("load")
            .expect("the second tick persists the new event");
        assert_eq!(second.in_progress_step, Some(2));

        listener.finish().await;
    }

    #[tokio::test]
    async fn start_refuses_a_loose_or_symlinked_parent() {
        let (uid, gid) = current_ids();
        let temp = tempfile::tempdir().expect("tempdir");

        let loose = temp.path().join("loose");
        fs::create_dir(&loose).expect("mkdir");
        fs::set_permissions(&loose, fs::Permissions::from_mode(0o755)).expect("chmod");
        let error = CheckpointListener::start(ListenerSpec {
            socket: loose.join("7.1.0.sock"),
            session_dir: temp.path().join("session"),
            job_id: 7,
            step_id: 0,
            uid,
            gid,
            dir_owner_uid: uid,
            expected_cgroup: "/expected".into(),
            cgroup_lookup: always("/expected"),
        })
        .expect_err("a 0755 parent must be refused");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);

        let elsewhere = tempfile::tempdir().expect("tempdir");
        let linked = temp.path().join("linked");
        std::os::unix::fs::symlink(elsewhere.path(), &linked).expect("symlink");
        let error = CheckpointListener::start(ListenerSpec {
            socket: linked.join("7.1.0.sock"),
            session_dir: temp.path().join("session"),
            job_id: 7,
            step_id: 0,
            uid,
            gid,
            dir_owner_uid: uid,
            expected_cgroup: "/expected".into(),
            cgroup_lookup: always("/expected"),
        })
        .expect_err("a symlinked parent must be refused");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn the_accept_loop_reaps_finished_connections_instead_of_only_finish() {
        let fx = fixture();
        let listener = CheckpointListener::start(spec(&fx, always("/expected"))).expect("start");

        let completed = 50;
        for _ in 0..completed {
            send_and_ack(&fx.socket, &begin_json(7, 0, 1)).await;
        }
        settle().await;

        // One more connection drives another reap pass in the accept loop,
        // which must have already cleared out the completed tasks above
        // rather than leaving one JoinSet entry per connection.
        send_and_ack(&fx.socket, &begin_json(7, 0, 2)).await;
        settle().await;

        let remaining = listener.connections.lock().await.len();
        assert!(
            remaining <= 1,
            "expected the accept loop to reap finished connections, found {remaining} \
             still in the JoinSet after {completed} completed ones"
        );

        listener.finish().await;
    }

    #[tokio::test]
    async fn socket_is_0600_while_live_and_removed_after_finish() {
        let fx = fixture();
        let listener = CheckpointListener::start(spec(&fx, always("/expected"))).expect("start");

        let mode = fs::metadata(&fx.socket).expect("stat").permissions().mode() & 0o7777;
        assert_eq!(mode, 0o600);

        listener.finish().await;
        assert!(!fx.socket.exists());
    }

    #[tokio::test(start_paused = true)]
    async fn finish_returns_promptly_despite_a_client_flooding_after_its_event() {
        let fx = fixture();
        let listener = CheckpointListener::start(spec(&fx, always("/expected"))).expect("start");

        let mut stream = UnixStream::connect(&fx.socket).await.expect("connect");
        stream
            .write_all(&begin_json(7, 0, 1))
            .await
            .expect("write event");

        // Fill the kernel send buffer well past DRAIN_CAP_BYTES before the
        // server gets a chance to read any of it (nothing here yields, so
        // the server cannot run until this loop stops itself on WouldBlock).
        // An unbounded drain would still be spinning on this; a bounded one
        // stops at its cap regardless.
        let chunk = [b'x'; 4096];
        loop {
            match stream.try_write(&chunk) {
                Ok(_) => continue,
                Err(ref error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }

        // Forces the connection to actually be accepted and its event read
        // before finish() runs — the ack arrives well before all the junk
        // behind it does, since reading it does not wait on draining that.
        let mut ack = [0u8; 1];
        stream.read_exact(&mut ack).await.expect("read ack");
        assert_eq!(ack, [b'k']);

        // No explicit time::advance: a byte-capped drain and the bounded
        // connection teardown in finish() both resolve on their own.
        listener.finish().await;

        let record = load_record(&fx.session_dir)
            .expect("load")
            .expect("a record was written");
        assert_eq!(record.in_progress_step, Some(1));
    }

    #[test]
    fn parse_unified_cgroup_reads_the_v2_line() {
        assert_eq!(parse_unified_cgroup("0::/a/b\n"), Some("/a/b".to_string()));
    }

    #[test]
    fn parse_unified_cgroup_finds_the_unified_line_in_a_hybrid_listing() {
        let hybrid = "12:pids:/a\n11:cpu,cpuacct:/b\n0::/unified/path\n";
        assert_eq!(
            parse_unified_cgroup(hybrid),
            Some("/unified/path".to_string())
        );
    }

    #[test]
    fn parse_unified_cgroup_is_none_without_a_unified_line() {
        let legacy_only = "12:pids:/a\n11:cpu,cpuacct:/b\n";
        assert_eq!(parse_unified_cgroup(legacy_only), None);
    }
}
