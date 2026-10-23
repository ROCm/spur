# Raft Consensus in Spur

## Overview

Spur uses Raft for durable, replicated cluster state.  Raft is **always-on**:
even a single-node deployment runs a 1-member Raft cluster that self-elects
instantly.  All state mutations flow through the Raft log as `WalOperation`
entries proposed via `ClusterManager::propose()` and applied through the
`StateMachineApply` trait.

Built on the **openraft** crate (`openraft::Raft`).  The type config is
declared in `spurctld/src/raft.rs`:

```rust
openraft::declare_raft_types!(
    pub SpurTypeConfig:
        D = WalOperation,   // application data
        R = ClientResponse, // per-entry apply result
        Node = BasicNode,
);
```

Raft timing defaults (hardcoded in `start_raft_with_recovery_mode`):

| Parameter | Value |
|---|---|
| `heartbeat_interval` | 500 ms |
| `election_timeout_min` | 1500 ms |
| `election_timeout_max` | 3000 ms |
| `snapshot_max_chunk_size` | 1 MiB |

---

## WAL Operations

`WalOperation` (defined in `spur-core/src/wal.rs`) is an externally-tagged
serde enum.  Every cluster state change is one variant.

### Job lifecycle

| Variant | Purpose |
|---|---|
| `JobSubmit` | Insert a new job with its full `JobSpec` |
| `JobStateChange` | Transition a job between states (Pending, Running, etc.); carries optional `pending_reason`, `begin_time` (backoff hold), `expected_run_attempt` (stale-activation guard) |
| `JobStart` | Record node allocation at dispatch; carries `per_node_alloc`, `run_attempt`, `idle_fill` stamp, `srun_step_dispatch` flag |
| `JobComplete` | Finalize a job with exit code and terminal `JobState` |
| `JobNodeComplete` | Per-node completion report with exit code, signal, and `run_attempt` guard |
| `JobTimeLimitSignaled` | Wall-clock watchdog fired; durable so grace period survives failover |
| `JobCancelSignaled` | Cancel signal recorded; moves run to Completing |
| `JobStepComplete` | Step-level exit code for `DerivedExitCode` |
| `JobStepCreate` | Persist a step at creation so `run_step` survives restart |
| `JobPriorityChange` | Reprioritize; also carries `reset_requeue_count`, `clear_reservation`, `run_attempt` |
| `JobDispatchBackoff` | Hold a job that failed dispatch before leaving Pending |
| `JobPreemptRequeue` | Atomic preempt + requeue (free allocation, end prior run, re-pend with hold) |
| `JobPreemptCancel` | Preempt then cancel in one entry |
| `JobUserRequeue` | Admin requeue (`scontrol requeue`) |
| `JobSuspend` / `JobResume` | Suspend/resume with leader-stamped instant |
| `JobEvict` | Evict a single job to NodeFail |
| `JobLaunchFailureDetail` | Record why a requeued job returned to Pending |
| `JobExcludeUpdate` | Update a job's exclude nodelist |
| `EvictTerminalJobs` | Bound controller memory by evicting aged-out terminal jobs |

### Node management

| Variant | Purpose |
|---|---|
| `NodeRegister` | Register a node (name, resources, address, labels, source) |
| `NodeUpdate` | Update a node's resources, address, version |
| `NodeStateChange` | Transition node state (Idle, Down, etc.) with reason |
| `NodeLabelsUpdate` | Set/remove node labels |
| `NodeRemove` | Deregister a node |

### Partitions, reservations, tokens

| Variant | Purpose |
|---|---|
| `PartitionCreate` / `PartitionUpdate` / `PartitionDelete` | CRUD for partitions |
| `ReservationCreate` / `ReservationUpdate` / `ReservationDelete` | CRUD for reservations |
| `TokenCreate` / `TokenRevoke` | Admission token management |

### k0s cluster

| Variant | Purpose |
|---|---|
| `NodeK0sAssign` | Assign a k0s role + mesh IP + pod CIDR |
| `K0sSetPhase` | Set cluster phase, control-plane nodes, member scope |
| `NodeK0sClear` / `NodeK0sSetError` | Clear or error a node's k0s state |
| `K0sMemberNodesAdd` / `K0sMemberNodesRemove` | Grow/shrink k0s member set |

### Serialization

All `WalOperation` values are serialized as JSON (via `serde_json`).  The Raft
log entries, vote, snapshot, and purge marker are all JSON files on disk.
The openraft network transport also uses JSON (`serde_json::to_vec` /
`from_slice`) inside opaque `bytes payload` protobuf messages.

---

## State Machine Apply

`ClusterManager` implements `StateMachineApply`:

```rust
pub trait StateMachineApply: Send + Sync {
    fn apply_operation(&self, op: &WalOperation) -> ClientResponse;
    fn snapshot_state(&self) -> Result<Vec<u8>, anyhow::Error>;
    fn restore_from_snapshot(&self, data: &[u8]) -> Result<(), anyhow::Error>;
}
```

`apply_operation` is called inside `SpurStore::apply_to_state_machine` under a
write lock on `StoreInner`.  It acquires write locks on `jobs` and `nodes`
(always jobs-before-nodes), then pattern-matches over every `WalOperation`
variant.  Key behaviors:

- **Leader-stamped timestamps**: `op.occurred_at()` provides the leader's
  instant so every replica (and replay) records the same time.  Falls back to
  `Utc::now()` for pre-upgrade entries that carry no stamp.
- **Idempotent transitions**: state changes use `job.apply_transition()` which
  returns `NoOp` for duplicate or stale applies.
- **`expected_run_attempt` guard**: a `JobStateChange` carrying this field is
  refused if the job's current epoch has advanced, preventing a delayed
  activation from firing on a superseded reservation.
- **`ClientResponse` feedback**: the apply result signals whether a job
  actually started (`job_started`), whether a state change was applied
  (`state_change_applied`), whether jobs were finalized (`jobs_finalized`),
  etc., so the proposing leader can act on the outcome.

---

## Leader Election

### Single-node self-election

With one peer, the node bootstraps a 1-member membership via
`raft.initialize()` and self-elects.  The election timeout (1.5--3 s) means
leadership is acquired within seconds of startup.

### Multi-node

`start_raft_with_recovery_mode` builds a `BTreeMap<NodeId, BasicNode>` from the
`controller.peers` list (1-indexed).  All peers are initialized into the
membership on first boot.  `raft.initialize()` is skipped on restart (detected
by the presence of a persisted vote, log entries, or snapshot).

Node ID resolution precedence:

1. Explicit `controller.node_id` in config
2. Position of this hostname in `controller.peers` (first-label match)
3. Hostname ordinal fallback (IP-only peers only)

A persisted `raft/node_id` file guards against identity shifts across restarts.
If the resolved ID differs from the persisted one, startup fails fast rather
than orphaning committed state.

### Leadership watcher

`RaftHandle::spawn_leadership_watcher()` drives a `watch::Receiver<Option<LeaderSince>>`
channel.  It tracks the term, so a coalesced lose-then-regain (same leader ID
but bumped term) correctly restamps the `LeaderSince` instant.  Losing the
metrics sender publishes `None` so consumers fail closed.

Two leadership checks are available:

- `is_leader()` -- cheap, locally-cached, can be stale for one election timeout
- `ensure_leader()` -- consensus-backed (quorum heartbeat exchange), costs a
  network round-trip

---

## Snapshots

### Serialization

`ClusterSnapshot` is a JSON-serialized struct containing all cluster state:
jobs, nodes, reservations, partitions, deleted partition names, steps, license
pool, admission tokens, burst buffer total, k0s state, and `next_job_id`
high-water mark.  Serialized via `serde_json::to_vec`.

### Chunked transport

Snapshots ship in chunks of `RAFT_SNAPSHOT_MAX_CHUNK_SIZE` (1 MiB raw).  Since
`serde_json` inflates `Vec<u8>` ~3.4x on the wire, the gRPC message limit is
set to `RAFT_MAX_MESSAGE_SIZE` (32 MiB).  Invariant enforced at compile time:
`4 * RAFT_SNAPSHOT_MAX_CHUNK_SIZE < RAFT_MAX_MESSAGE_SIZE`.

### Stale-snapshot guard

`persist_and_cache_snapshot` is serialized by `persist_lock` and compares
`last_log_id`: if the cached snapshot is newer, the older build is discarded.
This prevents a slow local build from clobbering a leader-installed snapshot.

### Disk format

Snapshot is persisted atomically as `{state_dir}/raft/snapshot.json`.  Reads
are always served from the in-memory cache, never re-read from disk, so a torn
file on disk does not affect serving.

---

## Leader Forwarding

Write RPCs that arrive at a follower are transparently forwarded to the leader.

### PassthroughCodec

`PassthroughCodec` (in `server.rs`) sends pre-encoded bytes without
re-serialization.  The encoder is a raw `BytesEncoder` (copies `Bytes` into
the buffer); the decoder is a standard `ProstDecoder`.

Why this matters: before forwarding, the follower encodes the protobuf message
once, computes a digest of those exact bytes, and signs the digest.  If tonic's
default codec re-encoded the message, the bytes on the wire could differ from
the signed bytes (protobuf encoding is not canonically unique), breaking
signature verification on the leader.

### Flow

1. Follower receives a write RPC and detects it is not the leader
2. Checks `is_already_forwarded` (header or extension) to prevent loops
3. Resolves the leader channel via `leader_proxy.get_leader_channel()`
4. Encodes the request body once via `prost::Message::encode`
5. Computes `request_digest` over those bytes and signs
6. Sends via `tonic::client::Grpc::unary` with `PassthroughCodec`

---

## Persistence

### Disk layout

```text
{state_dir}/raft/
  vote.json                     # current vote
  purged.json                   # last purged log id
  snapshot.json                 # full cluster snapshot
  node_id                       # persisted Raft node id
  log/
    00000000000000000042.json   # log entry at index 42
    ...
```

### Atomic writes

All critical files (`vote.json`, `snapshot.json`, `purged.json`) use
`write_atomic`: write to `*.tmp`, fsync, rename over target, fsync the
directory.  Stale `*.tmp` files from interrupted writes are cleaned on startup.

Log entries (`log/*.json`) are written directly (non-atomic) since they are
append-only and individually recoverable.

### Recovery modes

- **Strict** (default, `--allow-partial-wal-recovery` absent): any
  unreadable/undeserializable vote, log entry, or snapshot is a hard error.
  The operator must roll back to a compatible binary.
- **Lenient** (`strict = false`): logs a warning and skips the record.
  Dangerous -- committed state is lost silently.  Only for deliberate forensic
  recovery.

`purged.json` always hard-fails regardless of mode (a corrupt purge marker
would silently un-purge history).

Unrestorable snapshots are quarantined to `snapshot.json.unrestorable` so
openraft's rebuild doesn't overwrite the only copy.

---

## Breaking Change Sensitivity

The Raft log and snapshot are the cluster's durable memory.  Schema changes
break replay.

### Rules

1. **Every new field on a `WalOperation` variant MUST have `#[serde(default)]`**.
   Without it, a controller on the new binary cannot replay entries written by
   an older binary (the field is absent), and strict recovery crashes on startup.

2. **Never rename or remove a variant**.  The externally-tagged JSON
   (`{"JobSubmit": {...}}`) ties variant names to the wire format.  Renaming
   one makes every historical entry with the old name unreadable.

3. **Append new variants at the end**.  Although serde's externally-tagged
   format is not position-sensitive, keeping them ordered avoids merge conflicts
   and keeps the enum readable.

4. **Frozen-payload tests guard the wire format**.  Tests like
   `job_submit_v0_5_1_payload_still_deserializes` embed a literal JSON string
   from a prior release and assert it still deserializes.  A failure means the
   new field needs a default, not a regenerated fixture.

5. **`ClusterSnapshot` fields also need `#[serde(default)]`**.  The snapshot
   is JSON and restored on startup; a missing field must not crash restore.

---

## HA Configuration

Multi-controller HA requires three or more `spurctld` instances with identically
ordered `controller.peers` lists.

### Config (`spur.conf`)

```toml
[controller]
peers = ["node1:6821", "node2:6821", "node3:6821"]
# Optional: explicit node id (else derived from hostname / peers position)
# node_id = 1
raft_listen_addr = "[::]:6821"
```

### Ports

| Port | Purpose |
|---|---|
| 6817 | Client-facing gRPC (SlurmController + SlurmAccounting) |
| 6820 | REST API (Slurm-compat HTTP, disabled by default) |
| 6821 | Raft-internal gRPC (heartbeats, log replication, snapshots) |

The Raft port runs a **separate** gRPC server (`serve_raft` in
`raft_server.rs`) so heartbeats are never blocked by client request load.
Message-size limits on both server and client are raised to
`RAFT_MAX_MESSAGE_SIZE` (32 MiB) to accommodate snapshot chunks.

A compile-time assertion enforces `MAX_GRPC_REQUEST_SIZE < RAFT_MAX_MESSAGE_SIZE`
so the client-facing cap never accidentally restricts Raft-internal traffic.

---

## Common Pitfalls

1. **Forgetting `#[serde(default)]` on a new WAL field.**  Breaks replay of
   every pre-upgrade entry with the old schema.  The frozen-payload tests catch
   this, but only if they exist for the affected variant.

2. **Re-encoding breaks signatures.**  Never replace `PassthroughCodec` with a
   typed codec on the forwarding path.  Protobuf encoding is not canonical; the
   re-encoded bytes can differ from what was signed.

3. **Lock order: jobs before nodes.**  `apply_operation` acquires `jobs.write()`
   then `nodes.write()`.  Reversing this in any other code path deadlocks.

4. **Oversized `AppendEntries`.**  The network layer pre-checks the serialized
   size against `RAFT_MAX_MESSAGE_SIZE` and returns `PayloadTooLarge` with a
   halved entries hint so openraft retries with a smaller batch.

5. **Node ID shift across restart.**  If `controller.peers` is reordered or the
   hostname changes, the derived ID may differ from the persisted one.  Startup
   fails with "differs from persisted id."  Fix the identity or wipe state.

6. **Lenient recovery is not free.**  Skipping records loses committed state
   (nodes, jobs, reservations).  The `skipped_records` counter logs at ERROR.
   Never enable it without assessing the loss.

7. **Snapshot quarantine.**  An unrestorable snapshot is moved to
   `.unrestorable` so openraft doesn't overwrite it with an empty rebuild.
   If you see this file, the snapshot was written by an incompatible binary.

8. **Single-node still runs Raft.**  Do not bypass the propose path for
   "optimization" in single-node mode -- the WAL is the sole durable store.

9. **`install_snapshot` validates before persisting.**  The applier's
   `restore_from_snapshot` runs before the snapshot reaches disk, so a
   schema-incompatible snapshot from a leader never gets persisted on a
   follower that can't apply it.

10. **`occurred_at` exhaustive match.**  Both `occurred_at()` and
    `stamp_occurred_at()` use exhaustive matches.  Adding a new variant that
    dates user-visible state must be a compile error in the catch-all arm, not
    a silent `None`.
