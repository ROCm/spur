# Raft Consensus in Spur

Spur replicates cluster state with openraft (`spurctld/src/raft.rs`). HA
configuration (`controller.peers`, `node_id`, `raft_listen_addr`) is documented
in `docs/admin-guide/configuration.rst`; controller count and quorum sizing in
`docs/deployment/ansible.rst`.

## Write path contract

- Raft is always on. A single-node deployment runs a 1-member Raft cluster, and
  the Raft log plus snapshot is the sole durable store for cluster state
  (accounting is in PostgreSQL). Never bypass
  `ClusterManager::propose()` "for single-node performance".
- Durable state changes only as a `WalOperation` proposed by the leader and
  applied on every replica (and on replay) via `StateMachineApply::apply_operation`.
  Apply must be deterministic: ops that date something a user reads back carry
  a leader-stamped `at` (see `stamp_occurred_at` in `crates/spur-core/AGENTS.md`).
  Apply still uses the local clock for leader-local fields such as
  `last_heartbeat` on `NodeRegister`. Which fields are written
  outside the WAL is in `crates/spurctld/AGENTS.md`.
- Apply takes `jobs` before `nodes`. Any other path locking both must use the
  same order or it deadlocks.
- A `JobStateChange` carrying `expected_run_attempt` is refused once the job's
  run epoch has advanced, so a delayed activation cannot fire on a superseded
  reservation.
- Log entries, vote, snapshot, and purge marker are JSON on disk, and the
  openraft transport carries JSON inside opaque `bytes payload` protobuf
  messages. Schema rules below therefore cover both disk replay and the wire.

## Leadership

- `is_leader()` is a cheap local check. A leader that lost quorum keeps
  reporting `true` until it sees a higher term. Use `ensure_leader()` (a quorum
  round-trip) before acting on leadership for writes that bypass Raft.
- `spawn_leadership_watcher()` tracks the term, so a coalesced lose-then-regain
  (same leader id, bumped term) still restamps `LeaderSince`. If the metrics
  sender goes away it publishes `None` so consumers fail closed.
- Raft runs on its own gRPC server (`serve_raft`, `raft_listen_addr`) so client
  request load never delays heartbeats.

## Leader forwarding

Write RPCs that reach a follower are forwarded to the leader. The follower
encodes the protobuf body once, computes `request_digest` over those bytes, and
signs it into the forwarded envelope. `PassthroughCodec` sends the pre-encoded
bytes unchanged. A typed codec would re-encode, and protobuf encoding is not
canonical, so the leader's signature check would fail.

A request that is already forwarded (`is_already_forwarded`) and reaches a
non-leader gets a not-leader status instead of being bounced again.

## Snapshots

- Snapshot chunks are `RAFT_SNAPSHOT_MAX_CHUNK_SIZE` raw bytes, and `serde_json`
  inflates `Vec<u8>` about 3.4x. Invariant:
  `4 * RAFT_SNAPSHOT_MAX_CHUNK_SIZE < RAFT_MAX_MESSAGE_SIZE`. It is documented
  but not machine-checked; only `MAX_GRPC_REQUEST_SIZE < RAFT_MAX_MESSAGE_SIZE`
  has a compile-time assert (in `raft_server.rs`), so the client-facing cap
  never restricts Raft traffic.
- Oversized `AppendEntries`: the network layer checks the serialized size
  against `RAFT_MAX_MESSAGE_SIZE` and returns `PayloadTooLarge` with a halved
  entries hint, so openraft retries with a smaller batch.
- `install_snapshot` restores into the state machine before persisting, so a
  schema-incompatible leader snapshot never reaches a follower's disk.
- `persist_and_cache_snapshot` is serialized by `persist_lock` and discards a
  snapshot older than the cached one, so a slow local build cannot clobber a
  leader-installed snapshot.
- Snapshots are served from the in-memory cache, never re-read from disk, so a
  torn `snapshot.json` does not affect serving.

## Persistence and recovery

- `vote.json`, `snapshot.json`, and `purged.json` use `write_atomic` (tmp,
  fsync, rename, fsync dir); stale `*.tmp` files are removed on startup. Log
  entries use plain `std::fs::write` without fsync, so a power loss can leave an
  unreadable entry, which strict recovery treats as a hard error.
- Strict recovery (default): any unreadable vote, log entry, or snapshot fails
  startup. The fix is to roll back to a compatible binary.
- `--allow-partial-wal-recovery` skips bad records instead. Each skipped record
  is lost committed state (nodes, jobs, reservations); `skipped_records` is
  logged at ERROR. Only for deliberate forensic recovery after assessing the loss.
- In that mode an unrestorable snapshot is moved to `snapshot.json.unrestorable`
  so openraft does not overwrite the only copy with an empty rebuild. Seeing
  that file means the snapshot came from an incompatible binary. In strict mode
  the snapshot stays in place and startup fails.
- `purged.json` always hard-fails, in either mode: a corrupt purge marker would
  silently un-purge history.
- `raft/node_id` persists the resolved Raft id. If reordering `controller.peers`
  or a hostname change shifts the derived id, startup fails ("differs from
  persisted id") rather than orphaning committed state. Restore the identity or
  wipe the raft state dir.
- `raft.initialize()` runs only on first boot (no vote, log, or snapshot yet).

## Breaking-change sensitivity

The Raft log and snapshot are the cluster's durable memory; schema changes
break replay. Root `AGENTS.md` (Breaking Changes) covers the general rule.

1. Every new field on a type reachable from `WalOperation` must deserialize when
   absent: `#[serde(default)]` or a plain `Option<T>` (an `Option` with
   `deserialize_with` still needs `default`). Otherwise strict recovery crashes
   replaying pre-upgrade entries.
2. `WalOperation` is append-only. Never rename or remove a variant: the
   externally tagged JSON (`{"JobSubmit": {...}}`) makes the variant name the
   wire format. Add new variants at the end (serde does not care about order;
   this is for readability and merge hygiene). For how a new variant interacts
   with a mixed-version rolling upgrade, see `docs/deployment/upgrading.rst`.
3. `ClusterSnapshot` fields need the same treatment; restore runs on every
   startup.
4. Frozen-payload tests (e.g. `job_submit_v0_5_1_payload_still_deserializes`)
   embed JSON from a prior release. A failure means the new field needs a
   default, not a regenerated fixture.
5. Never change what an existing variant does when applied. Controllers on
   different releases apply the same entries during a rolling upgrade, so they
   would diverge. New behavior gets a new variant (`JobCopyExited` beside
   `JobNodeComplete`), which an older controller cannot parse: it stops
   replicating instead, and the upgrade runs leader last.
6. A snapshot must not hand an older controller state it would misapply. It
   ignores fields it does not know, so state only a new variant produces would
   reach it silently through a snapshot. While any active job holds such state
   (`holds_per_node_run_state`: `Job.first_failed_node` or
   `Job.retained_nodes`), `ClusterSnapshot` writes the jobs under `fenced_jobs`
   and omits `jobs`, which older controllers require, so they refuse the
   snapshot and stay stalled as they do on the entry. Restore reads either key.
