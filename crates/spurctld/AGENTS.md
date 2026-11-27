# AGENTS.md -- spurctld

Raft, snapshot, forwarding (`PassthroughCodec`), and WAL recovery details live in
`docs/knowledge/consensus/raft.md`. Auth tiers are in `docs/knowledge/security/auth-model.md`.
Accounting schema, migrations, and bring-up are in `docs/knowledge/accounting/accounting.md`.

## Invariants

- **`cluster.rs` apply is the durable write path.** Durable replicated state changes only via `ClusterManager::propose()` then `apply_operation()`. Leader-local or transient fields (heartbeat stats, pending reasons, scheduler marks, WG pubkeys, BB staging state, GPU `stable_id` mappings, job output paths) are written directly and must tolerate loss on failover. Direct-write functions: `advance_bb_staging_for()`, `complete_bb_stage_in()`, `reconcile_node_gpu_allocations()` (the WAL keeps the legacy GPU ids), `set_job_output_paths()`, plus `update_job` and `register_agent_node` (`Node.source`) below. Do not add new direct mutations of `jobs`, `nodes`, `partitions`, `reservations`, `steps`, `tokens`, or `k0s`; they diverge from the WAL.
- `ClusterManager::update_job` writes durable spec fields (`time_limit`, `partition`, `comment`, `account`, `qos`) directly, not through Raft. They persist only in the leader's own snapshots, so a failover loses them.
- `PartitionDelete` records the name in snapshot-persisted `deleted_partition_names` (cleared by `PartitionCreate`). `restore_from_snapshot` filters the `spur.conf` baseline with it only for legacy snapshots whose `partitions` is `None`.
- `NodeInventoryUpdate` removes unlisted nodes all-or-nothing: `inventory_removals` refuses before any removal if an unlisted node holds a non-finalized job, is named by a reservation, or has a k0s role or membership. Only `ClusterManager::reconfigure` proposes it.
- **Evicted jobs' steps close inside the apply.** The `NodeStateChange(Down)` and `NodeRemove` arms call `complete_job_steps()` for every job `evict_jobs_on_node()` finalized, so every replica closes them identically instead of relying on a leader-only sweep.
- **Core layout is replay-deterministic.** `derive_core_layout()` feeds the snapshot-persisted `Node.reserved_picks` into `CoreLayout::compute` and stores the result back, so every replica computes the same sticky picks. An unsatisfiable layout keeps the last good one (dropping it would free reserved cores under running jobs) and records `layout_error`. `Node.layout_blocked_cpus` is leader-local, stored by `record_layout_heartbeat()` only when the agent holds the expected fingerprint.
- **WAL entry size.** A `JobSubmit` is one Raft entry retained in every snapshot, so `check_submission_size()` caps it at `MAX_JOB_SPEC_SIZE`, measured with a `ByteCounter` sink to avoid a second allocation.
- **k0s phase is replicated, its clock is not.** Phase transitions go through `ClusterManager::set_k0s_phase()` so a mid-provision leadership change is safe. The provisioning clock, `DriftTracker`, and `Correction` state are leader-local and reset on leadership change.
- **`DispatchInFlight` is an RAII guard** so a panic or early return cannot permanently exempt a node from `abort_orphaned_placements`.
- Dispatch cooldowns (`node_dispatch_cooldowns`) are leader-local and never persisted.
- `node_registration_locks` serializes (re-)registrations per node so a stale label diff cannot overwrite a fresh one.

## Leader forwarding

- Read RPCs are forwarded too, unless the request is already forwarded (prevents bounce loops).
- `Ping` is answered locally, but its `capabilities` are the leader's list intersected with this controller's, failing closed to none if no leader answers. This stops a client passing a capability check on a follower during a leader-last upgrade while the leader drops fields it does not know.
- The capability probe is bounded by `LEADER_CAPABILITY_PROBE_TIMEOUT_SECS` including channel acquisition: `get_leader_channel()` holds the `cached_channel` mutex across `Endpoint::connect()`, so an unbounded dial would jam every forwarded RPC. The leader `Endpoint` `connect_timeout` reuses that constant and must not exceed it.

## Config reload vs restart

- `scontrol reconfigure` swaps `ClusterManager.config`, but these are captured once and restart-only: gRPC listen address, Raft peer list, scheduler tick interval, `max_jobs_per_cycle`, JWT signing key, database pool.
- `[topology]` is rebuilt from `cluster.config()` in every `PassView::read`, so `scontrol reconfigure` updates it on the leader, unlike `interval_secs` / `max_jobs_per_cycle`.
- A `[[nodes]]` change to spec cores or `restricted_cores_per_gpu` is rejected by `reconfigure` (`core_layout::restart_only_node_changes`) so leader and followers cannot recompute isolation unevenly before a failover.
- `admission.node_names` semantics are in `docs/knowledge/deployment/operations.md`. `reconfigure()` waits for earlier Raft writes before proposing `NodeInventoryUpdate`. Tests: `cluster/tests/node_inventory.rs`.

## Scheduler

- `schedule_with()` runs inside `catch_unwind`: a panic skips one cycle, not the loop.
- **Leadership grace periods:** the orphaned-placement sweep waits 2x `dispatch_timeout_secs`; the node health checker waits max(`heartbeat_timeout_secs`, `health_tick_secs`). `leader_since` is a watch channel so a lose-then-regain flip within one tick is detected.
- Topology has no `fabric` plugin value. A `FabricView` with block data or an `external.block_file` upgrades `tree` or `block` to `TopologyTree::from_fabric`.
- **Idle-fill quota is evaluated per team (legitimate + borrowed), not per run.** With 2+ borrowed runs in a QOS, a per-run check has each run measure against the same headroom, all read "became legitimate", and reclaim finds nothing while the team sits over its cap.
- Idle-fill candidates must have a `time_limit`: an unbounded borrowed job makes its node look busy forever through `busy_until`.
- Het, burst-buffer, and license jobs cannot idle-fill: re-running the BB gate mutates staging state, and the license contention map would let a candidate take a license an in-quota job just claimed.
- Script placement and per-node run semantics are in `docs/knowledge/execution/job-execution.md`. The dispatch back-off for a run landing on a node without `CAP_SCRIPT_PLACEMENT` uses spared retries (`take_spared_dispatch`), capped at `MAX_SPARED_DISPATCHES`.
- A `ReservedCpusHeld` launch refusal cools the node down like `ResourcesUnavailable`; when every failed node reported it, `backoff_pending_job_after_dispatch_failure(.., spare_requeue_budget: true)` holds the job without charging `max_batch_requeue`.
- **Victim search** (`VictimSearch`, `victim_order.rs`): the job-to-node credit is a ranking estimate, not a placement proof; `satisfiable_victim_set` must verify the final set by real placement. Rank invalidation must include nodes sharing a remaining job with a partly emptied node, and wide picks whose credited gain is capped when the shortfall shrinks. The tie-break hash uses fixed FNV-1a constants so it is stable across processes and toolchains. Eligibility and ranking: `docs/knowledge/scheduler/scheduling.md#victim-selection`.

## Auth and trust boundaries

- **Two different `require_admin` bars.** `server.rs` requires `administers_cluster()` (Administrator only; an Operator is denied). `accounting/grpc.rs` requires `operates_jobs()` (Operator or Administrator). `record_job_start` and `record_job_end` have no role check.
- `rest_auth` inserts only `Identity`; REST `dispatch()` re-wraps the request and inserts `Identity` + `Verified`. `Verified` drives the audit `verified` column, so only code that just verified a credential may insert it.
- REST mutations call the `SlurmController` trait method via `dispatch()` so validation, forwarding, authorization, and audit are shared with gRPC. Do not reimplement them in REST handlers.
- REST submit requires a valid `Authorization: Bearer` header: REST carries no uid, so an unauthenticated submit would run as uid 0.
- REST on a non-loopback address with `plugin = "spur"` and `mode = "required"` requires `rest_api.allow_non_loopback = true`.
- Lua hooks load with `ChunkMode::Text` only: bytecode is a sandbox escape vector. Changes to non-whitelisted job fields are dropped with a warning.
- `agent_client::connect()` mints a credential per connection (not cached), assuming one RPC per connect.
- Native signing keys are `None` unless `plugin = "spur"`, and then `sign_dispatch_credential()` / `sign_step_credential()` return empty strings.
- A handler that writes its own audit row in the same transaction must call `mark_recorded_in_band()` to suppress the audit layer's duplicate.

## Pitfalls

- `accounting_block()` fails closed: before the first successful cache load, jobs naming a QOS or account are held in `Pending` (`AccountingUnavailable`).
- REST `ntasks` absent or zero defaults to one task per requested node (`rest_effective_ntasks`), not 1, so multi-node REST requests are not collapsed.
- `rest/convert.rs:node_to_json` must use `Node::socket_core_thread()` / `Node::gpu_cpus()` (shared with `node_to_proto`) so gRPC and REST cannot disagree.
- `validate_per_gpu_options()` backstops spur-cli's own per-GPU flag checks for REST/gRPC clients that bypass the CLI.
- `CompleteJobStep` from a client is cancel-or-PTY only: `cancelled: false` for a non-PTY step is `InvalidArgument`, so a client never overwrites a running step's result.
- Stepd recovery: `claim_stepd_recovery_fence()` serializes concurrent reporters so two past-grace reports cannot both fence (requeue) the same run.
- Multi-node PMIx `ReleasePmix` rollback is best-effort with its own short timeout (`RELEASE_RPC_TIMEOUT`), independent of the prepare/launch deadlines.

## Testing

- `server.rs` and `rest/handlers.rs` tests build a `ClusterManager` with `new()` plus direct `apply_operation()` calls (no Raft, no election timing), then drive `ControllerService` or Axum `oneshot()`. Helpers: `test_support::no_leader_service()`, `test_slurm_config()`.
- Victim search tests compare heap selection with a full re-rank after every pick and count operations on large layouts. Do not use elapsed-time limits.
