# spurctld — Spur Controller Daemon

## Overview

`spurctld` is the central controller daemon for Spur, an AI-native HPC job scheduler written in Rust that serves as a Slurm drop-in replacement. It runs as a single binary (`spurctld`) on one or more controller nodes, owns all cluster state (jobs, nodes, partitions, reservations, tokens), and replicates that state via Raft consensus. Every mutation flows through the Raft WAL as a `WalOperation`; reads are served from in-memory state materialized by replaying the log. The controller exposes a gRPC API (port 6817), a Slurm-compatible REST API (port 6820), a Raft-internal gRPC port (6821), and an OpenMetrics HTTP endpoint (port 6822).

## Directory Structure

```
src/
  main.rs                  CLI args, process startup, wires Raft/scheduler/k8s/health/metrics/REST/gRPC
  server.rs                gRPC SlurmController impl — all RPC handlers, leader forwarding, PassthroughCodec
  cluster.rs               ClusterManager — the 31k-line state machine: jobs, nodes, partitions, reservations, apply()
  raft.rs                  openraft integration: SpurStore (disk-backed WAL/snapshot), StateMachineApply trait
  raft_server.rs           Dedicated gRPC server for Raft-internal RPCs (AppendEntries, Vote, InstallSnapshot)
  scheduler_loop.rs        Leader-gated scheduling loop: BackfillScheduler, time-limit/completing/inactive enforcers
  cluster_k8s.rs           k0s cluster reconcile loop: role assignment, WireGuard mesh, join-token lifecycle
  hooks.rs                 Sandboxed Lua job_submit hooks (mlua, text-only, memory/instruction-capped)
  auth_middleware.rs        Tower layer: single auth verification point for all gRPC+accounting RPCs
  agent_client.rs          Authenticated controller->agent connections (JWT or native ControllerRpc tokens)
  native_keys.rs           Process-wide Ed25519 signing keys: peer forwarding, node identity, execution credentials
  pmix_dispatch.rs         Multi-node PMIx prepare/release fan-out helpers
  dispatch_tracker.rs      Leader-local in-flight launch tracker (RAII drop guard)
  association_cache.rs     Cached user/account associations from PostgreSQL (default QOS, memberships, limits)
  fairshare_cache.rs       Cached fairshare factors from PostgreSQL
  limits_cache.rs          Cached QOS definitions and GrpWall usage from PostgreSQL
  rpc_stats.rs             Per-operation RPC timing accumulator
  sched_stats.rs           Scheduler cycle and lifecycle statistics accumulator
  rpc_middleware.rs        Tower layer: RPC path/timing recording
  metrics_proto.rs         Protobuf-to-snapshot converters for OpenMetrics export
  metrics_server.rs        Axum HTTP server exporting /metrics/* endpoints (OpenMetrics 1.0)

  accounting/
    mod.rs                 Bring-up orchestration: connect-with-retry, activate caches/loops
    db.rs                  PostgreSQL schema (sqlx migrations), CRUD for jobs/users/accounts/QOS/usage
    grpc.rs                SlurmAccounting gRPC service impl
    fairshare.rs           Fairshare factor computation (half-life decay)
    reconcile.rs           Leader-gated job reconciliation loop (db vs in-memory drift)
    notifier.rs            Async accounting write dispatch (job start/end records)
    txn.rs                 Audit transaction record types and helpers

  audit/
    mod.rs                 AuditSlot for handler->layer annotation, InBand for handler-committed rows
    layer.rs               Tower audit layer: records mutating RPCs to the accounting txn table
    registry.rs            RPC method -> (action, entity, scope) classification for audit

  rest/
    mod.rs                 Axum router: /api/v1 and /slurm/v0.0.42 route trees, rest_auth middleware
    handlers.rs            REST endpoint handlers (ping, jobs, nodes, partitions, submit, cancel)
    types.rs               Serde types for REST request/response envelopes
    convert.rs             Job/Node/Partition -> JSON converters for REST
```

## Key Types & Interfaces

### ClusterManager (`cluster.rs`)

The core state machine. Thread-safe via `parking_lot::RwLock`. Both the scheduler loop and the gRPC server hold `Arc<ClusterManager>`.

| Field | Type | Purpose |
|---|---|---|
| `config` | `RwLock<Arc<SlurmConfig>>` | Live config, swapped atomically by `reconfigure()` |
| `jobs` | `RwLock<HashMap<JobId, Job>>` | All jobs (pending through terminal) |
| `nodes` | `RwLock<HashMap<String, Node>>` | Registered compute nodes |
| `partitions` | `RwLock<Vec<Partition>>` | Partition definitions |
| `reservations` | `RwLock<Vec<Reservation>>` | Active reservations |
| `steps` | `RwLock<HashMap<(JobId, u32), JobStep>>` | Job steps |
| `tokens` | `RwLock<HashMap<String, AdmissionToken>>` | Node admission tokens |
| `k0s` | `RwLock<K0sClusterState>` | Managed k0s cluster state |
| `raft` | `RwLock<Option<SpurRaft>>` | Raft handle, set after startup |
| `accounting` | `RwLock<Option<AccountingNotifier>>` | Async accounting writer |
| `scheduler_notify` | `Arc<Notify>` | Event-driven scheduler wake |
| `dispatch_tracker` | `Arc<DispatchTracker>` | In-flight launches |

Implements `StateMachineApply`: every committed `WalOperation` calls `apply_operation()`, which pattern-matches the variant and mutates in-memory state. This is the only write path — no other code mutates `jobs`/`nodes`/etc directly.

### ControllerService (`server.rs`)

Implements `SlurmController` (gRPC). Cloneable — all fields are `Arc` or immutable.

| Field | Purpose |
|---|---|
| `cluster` | Shared `ClusterManager` |
| `raft` | Raft handle for leadership checks |
| `leader_proxy` | Cached channel to the current leader |
| `client_addrs` | Node ID -> client API address map |
| `jwt_key` | Node-token signing key (restart-only, not reconfigurable) |

### SpurStore (`raft.rs`)

Disk-backed Raft storage. Layout: `{state_dir}/raft/{vote.json, log/*.json, snapshot.json, purged.json}`.

All log entries carry `WalOperation` as the data payload. Snapshots serialize the full cluster state (all jobs, nodes, partitions, reservations, tokens, k0s) as a single JSON blob.

### Key Error Types (`cluster.rs`)

- `SubmitError` — `InvalidArgument | Unavailable | Internal`
- `CancelError` — `NotFound | AlreadyTerminal | NotOwner | Internal`
- `ReservationError` — `InvalidArgument | NotFound | AlreadyExists | Raft`
- `PartitionError` — same shape as `ReservationError`
- `RegisterNodeError` — `PermissionDenied | Internal`

## gRPC Services

The controller serves two gRPC services on port 6817:

### SlurmController

**Jobs:** `SubmitJob`, `GetJobs`, `GetJob`, `CancelJob`, `CompleteJob`, `SuspendJob`, `ResumeJob`, `UpdateJob`, `RequeueJob`, `JobKeepalive`, `ExecInJob`, `RunStep`

**Job steps:** `GetJobSteps`, `CreateJobStep`, `CompleteJobStep`

**Nodes:** `GetNodes`, `GetNode`, `UpdateNode`, `DrainNode`, `DeregisterNode`, `DeregisterAgent`, `RegisterAgent`, `Heartbeat`, `ReportJobStatus`, `ReportStepdRecovery`

**Partitions:** `GetPartitions`, `CreatePartition`, `UpdatePartition`, `DeletePartition`

**Reservations:** `CreateReservation`, `UpdateReservation`, `DeleteReservation`, `ListReservations`

**K8s/Cluster:** `ClusterUp`, `ClusterDown`, `ClusterStatus`, `ClusterKubeconfig`, `ClusterAddNodes`, `ClusterRemoveNodes`

**Auth tokens:** `CreateToken`, `ListTokens`, `RevokeToken`

**Diagnostics:** `Ping`, `GetJobMetrics`, `GetNodeMetrics`, `GetRpcStats`, `GetSchedStats`, `GetAssocMgrInfo`, `ResetDiagStats`, `Reconfigure`

### SlurmAccounting (`accounting/grpc.rs`)

Runs on the same port. Database-backed user/account/QOS/association management, fairshare factor queries, and usage reporting.

## Raft Consensus

### Always-on design

Raft runs even in single-node mode (self-elects instantly, same pattern as Apache Kudu). No code path bypasses Raft for writes.

### WalOperation

Every cluster mutation is proposed as a `WalOperation` enum variant. ~39 variants:

**Job lifecycle:** `JobSubmit`, `JobStart`, `JobComplete`, `JobNodeComplete`, `JobCancelSignaled`, `JobPreemptCancel`, `JobPreemptRequeue`, `JobSuspend`, `JobResume`, `JobUserRequeue`, `JobDispatchBackoff`, `JobTimeLimitSignaled`, `JobStateChange`, `JobPriorityChange`, `JobStepCreate`, `JobStepComplete`, `JobEvict`, `JobLaunchFailureDetail`, `JobExcludeUpdate`, `EvictTerminalJobs`

**Node management:** `NodeRegister`, `NodeUpdate`, `NodeStateChange`, `NodeLabelsUpdate`, `NodeRemove`, `NodeK0sAssign`, `NodeK0sClear`, `NodeK0sSetError`

**Partitions:** `PartitionCreate`, `PartitionUpdate`, `PartitionDelete`

**Reservations:** `ReservationCreate`, `ReservationUpdate`, `ReservationDelete`

**Tokens:** `TokenCreate`, `TokenRevoke`

**K8s:** `K0sSetPhase`, `K0sMemberNodesAdd`, `K0sMemberNodesRemove`

`ClusterManager::propose()` writes to the Raft log; `apply_operation()` materializes the committed entry into in-memory state. The `ClientResponse` returned from apply carries side-effects (e.g. `jobs_finalized` for accounting writes, `job_started` flag for duplicate-detection).

### Leader forwarding

Followers forward write RPCs to the leader via a `LeaderProxy` that caches a channel to the current leader. The `PassthroughCodec` sends the exact bytes signed by the follower, since prost map fields have no canonical byte order and re-encoding would break the digest. The forwarded identity is carried in an `x-spur-identity` header, verified by `auth_middleware` against a SHA-256 body digest.

Read RPCs are forwarded too (consistency with leader state), unless the request is itself already forwarded (prevents bounce loops).

### Snapshots

`snapshot_state()` serializes the full cluster state to JSON. `restore_from_snapshot()` replaces all in-memory state. Snapshots ship in chunks of `RAFT_SNAPSHOT_MAX_CHUNK_SIZE` (1 MiB raw). The Raft gRPC message limit is 32 MiB (above tonic's 4 MiB default) to accommodate the JSON inflation of chunk payloads.

### Recovery

`--allow-partial-wal-recovery` skips unreadable WAL/vote/snapshot records instead of refusing to start. This is silent data loss — the default (`strict = true`) halts so the operator can roll back to a compatible binary.

## Scheduler Loop

`scheduler_loop::run()` is spawned from `main.rs`, leader-gated.

**Event-driven wake:** `tokio::select!` between `scheduler_notify.notified()` (job submitted) and a periodic tick (`scheduler.interval_secs`). Eliminates polling delay for new submissions.

**Per-cycle flow:**
1. Cancel unsatisfiable dependency jobs
2. Drive burst-buffer stage-in
3. Sweep orphaned placements (held back 2x dispatch_timeout after leadership change)
4. Purge expired reservations, enforce reservation end times, evict expired terminal jobs
5. Run node health checks
6. Classify pending jobs (QOS/account limits, burst-buffer, licenses, idle-fill candidates)
7. Build `ClusterState` (nodes off dispatch cooldown, partitions, reservations, topology, busy_until)
8. Run `BackfillScheduler::schedule()` in `catch_unwind` (a panic skips the cycle, not the loop)
9. Dispatch assignments: propose `JobStart` via Raft, fan out `LaunchJob` RPCs to agents
10. Record cycle stats

**Companion tasks (spawned alongside):**
- `enforce_time_limits` — SIGTERM then SIGKILL with a 30s grace (`GRACE_PERIOD_SECS`)
- `enforce_completing_timeout` — reaps jobs stuck in `Completing` past a deadline
- `manage_power` — node power state management
- `enforce_inactive_limits` — reaps interactive allocations with no keepalive activity

**Topology:** built once at startup from config (`tree` or `block` plugin). NOT picked up by `scontrol reconfigure` — requires a restart. Two plugins:
- `tree` — `TopologyTree::from_switches(&topo_config.switches)` — explicit switch hierarchy
- `block` — `TopologyTree::from_blocks(&node_names, block_size)` — implicit equal-sized blocks (default 18 nodes)

**Idle-fill candidates:** Jobs refused solely by their QOS group-node cap are appended below every in-quota job at placement time. They carry `idle_fill = true` and are offered exactly the capacity left after every in-quota job has started or reserved what it needs. Candidates must have a time_limit (mandatory — an unbounded borrowed job would make its node look busy forever through `busy_until`).

## Cluster Manager (cluster.rs)

At 31k lines, this is the largest file. It contains:

- **`apply_operation()`** — the state machine apply: pattern-matches every `WalOperation` variant
- **Job lifecycle** — submit validation (size check, submission limits, reservation/partition/QOS checks, hook execution), start (resource allocation, placement), complete (per-node reports, all-done detection), cancel, suspend/resume, requeue, preemption, priority changes
- **Node management** — register (with per-node serialization lock), heartbeat (GPU stable_id reconciliation), health checks (exclusive whole-node check jobs, force-drain for starvation, fail-drain on check failure), deregistration
- **Partition CRUD** — create, update (state/limits/preempt-mode/node-set), delete (with WAL suppression of config-seeded names)
- **Reservation CRUD** — create, update, delete, expiration purging, end-time enforcement
- **Token management** — create/list/revoke admission tokens
- **Idle-fill / borrow** — jobs over their QOS group-node cap may borrow spare capacity, with team-level quota checks and a borrow ceiling
- **Accounting integration** — `accounting_block()` holds jobs naming a QOS/account until the cache loads; `drain_accounting()` waits for pending writes at shutdown
- **Config snapshot** — `config()` returns a cheap `Arc<SlurmConfig>` clone; `reconfigure()` atomically swaps the inner Arc
- **Health checks** — submits exclusive whole-node jobs with reserved name `_spur-health.{idx}.{node}`, force-drains nodes to make room for starved checks, marks failed-check nodes with sticky drain
- **Node GPU reconciliation** — `heartbeat()` reconciles GPU `stable_id` lists; a mismatch triggers a `NodeUpdate` WAL entry

The `propose()` method wraps `raft.client_write()` and returns `ClientResponse`. All Raft proposals go through this single point.

**Launch failure requeue:** On a dispatch failure, the job is requeued with exponential backoff (`launch_backoff_secs`): base * 2^requeue_count, capped at `MAX_LAUNCH_BACKOFF_SECS`. The shift is clamped at 16 to avoid overflow.

## Accounting

### PostgreSQL integration (`accounting/db.rs`)

sqlx-driven migrations create tables for users, accounts, associations, QOS definitions, jobs (start/end), resource usage, TRES usage, and audit transactions. The schema mirrors Slurm's accounting model:

| Table | Key | Purpose |
|---|---|---|
| `jobs` | `job_id (BIGINT)` | Job history: state, exit code, node list, timings, preemption info |
| `accounts` | `name` | Account hierarchy (parent_account), fairshare weight, grp_tres |
| `users` | `(name, account)` | User-account membership, admin_level, default_account |
| `associations` | `(user_name, account, partition_name)` | Per-association limits: max_running_jobs, max_submit_jobs, grp_submit_jobs, TRES limits, max_wall |
| `qos` | `name` | QOS definitions: priority, preempt mode, per-user/account/group limits, idle_fill_preemptable |
| `usage` | `(user_name, account, period_start)` | Periodic CPU/GPU-seconds and job counts for fairshare |
| `tres_usage` | `(job_id, tres_type)` | Per-job TRES allocation and usage |
| `txn` | auto-id | Audit transaction log |

Migrations are serialized across controllers via a PostgreSQL advisory lock (`pg_advisory_xact_lock`).

### Bring-up (`accounting/mod.rs`)

`start()` returns immediately with an `AccountingService` marked unavailable. A background task retries `connect_and_migrate()` with capped exponential backoff (1s to 30s). On success, `activate()` installs the pool, sets the job notifier, and spawns cache-refresh and reconciliation loops. A controller that boots with the database down converges on its own.

### Caches

Four caches refresh periodically from the database:
- **FairshareCache** — `(user, account) -> f64` factor, half-life decay
- **QosCache** — `name -> Qos` definitions with limits
- **AssociationCache** — default QOS/account, memberships, per-association limits, admin levels, allowed QOS lists
- **GrpWallCache** — per-QOS consumed wall-minutes within a sliding window

All use the same pattern: `RwLock<Snapshot>` refreshed by a background loop that retains stale data on error. An unloaded cache (before the first successful fetch) holds scheduling with `accounting_block()`.

### Reconciliation (`accounting/reconcile.rs`)

Leader-gated loop that detects drift between in-memory job state and the accounting database (jobs started but not recorded, jobs completed but not finalized).

### Notifier (`accounting/notifier.rs`)

Async dispatch of job start/end records. `ClusterManager` calls `notifier.job_started()` / `notifier.job_ended()` and the notifier writes to PostgreSQL off the hot path. `drain_accounting()` waits up to 5 seconds at shutdown.

## REST API

Axum-based HTTP server on port 6820, nested under `/api/v1` and `/slurm/v0.0.42` (Slurm compatibility).

**Routes:** `/ping`, `/jobs`, `/job/{job_id}` (GET, DELETE), `/job/submit` (POST), `/nodes`, `/node/{name}`, `/partitions`

**Auth:** `rest_auth` middleware mirrors the gRPC `auth_middleware` policy. `/ping` is exempt. A forged token is always rejected even under permissive mode.

**Mutations route through the gRPC handler:** REST submit and cancel call `dispatch()`, which creates a `tonic::Request` with the caller's identity and calls the `SlurmController` trait method directly. This shares validation, leader forwarding, authorization, and audit with the gRPC path.

**REST submit requires authentication:** Unlike `sbatch`/`srun`, REST carries no uid, so an unauthenticated submission would run as uid 0. The handler rejects submissions without a valid `Authorization: Bearer` header.

**REST ntasks default:** An absent or zero `ntasks` defaults to one task per requested node (Slurm's default), not 1. This prevents a multi-node REST request from being silently collapsed.

**Non-loopback safety:** REST on a non-loopback address with `[auth] plugin = "spur"` and `mode = "required"` requires `rest_api.allow_non_loopback = true`, failing fast at startup otherwise.

## Hooks (`hooks.rs`)

Lua job_submit hooks run in an embedded mlua VM, sandboxed:
- Only `TABLE`, `STRING`, `MATH`, `UTF8`, `COROUTINE` stdlib loaded
- `dofile`, `loadfile`, `load`, `loadstring`, `collectgarbage` removed
- 64 MiB memory limit, 100M instruction budget
- `ChunkMode::Text` only (bytecode refused — sandbox escape vector)

The script defines `slurm_job_submit(job_desc, submit_uid)` and returns `slurm.SUCCESS` (0) or `slurm.ERROR` (-1). Whitelisted fields (`partition`, `qos`, `comment`, `gres`, `time_limit`, `priority`, `begin_time`, `account`, `constraint`, `reservation`, `hold`) may be modified; non-whitelisted changes are silently dropped with a warning.

`validate_submit_hooks()` runs at startup and on `reconfigure`, failing fast on missing/non-executable/non-compiling scripts.

## Auth & Audit

### auth_middleware.rs

A Tower layer wrapping all gRPC services on the port (including the accounting service — its `add_user` takes an `admin_level` and has no authorization of its own). Three modes:
- **Required:** every call must present a valid Bearer JWT; rejection is a hard `Unauthenticated`
- **Permissive:** anonymous calls are accepted (warned); a present-but-invalid token is always rejected
- **Disabled:** no verification at all

On success, inserts a verified `Identity` into request extensions. Nothing else in the pipeline may insert an `Identity`, so a handler that finds one knows it was verified here.

Forwarded requests from followers carry an `x-spur-identity` header, verified against the peer's Ed25519 key set and a SHA-256 body digest. The body digest is checked against the received wire bytes (not a re-encoded message) because prost map fields have no canonical byte order.

### audit/ subdirectory

**AuditLayer** (Tower): intercepts mutating RPCs, records a `TxnRecord` to the accounting database. Handlers add target/details via `annotate()`. A handler that writes its own audit row in the same transaction calls `mark_recorded_in_band()` to suppress the layer's duplicate.

**Registry:** classifies every RPC method as `Read` or `Mutating(action, entity, scope)`. Scope controls whether only the leader records (`LeaderOnly`) or every node that handles it (`Local`).

## K8s Integration

`cluster_k8s.rs` drives a SPUR-managed k0s cluster. Leader-gated reconcile loop (30s tick).

**Phases:** `Down -> Provisioning -> Ready` (or `Degraded` on timeout).

**Per-tick reconcile:**
1. Build WireGuard mesh membership from live node inventory
2. Push `ApplyMesh` to every meshed node (level-triggered, idempotent)
3. Assign roles (controller/worker) and allocate mesh IPs + pod CIDRs to unassigned nodes
4. Converge: `StartClusterComponent` RPC to each node, poll `GetClusterComponentStatus`
5. Detect drift (role/status disagreement) and issue corrections

Phase transitions go through `ClusterManager::set_k0s_phase()` (WAL-replicated), so a leadership change mid-provision is safe. The provisioning clock is leader-local: a leadership flip resets it, so the timeout re-arms on the new leader.

**Networking config:**
- `ClusterNetworking` struct carries WireGuard mesh CIDR, pod CIDR, service CIDR, CNI mode (kuberouter or calico), MTU
- Calico CNI can route natively over the WireGuard mesh (`mesh_native()`); kube-router tunnels over the underlay
- Mesh membership is pushed every tick (level-triggered, idempotent), so node-local drift (reboot, wg restart), failed pushes, and controller failover all self-heal

**Drift detection:** The reconcile loop detects drift between expected k0s role/status and actual node state, issuing corrections. Tracked by `DriftTracker` and `Correction` structs (leader-local, reset on leadership loss).

## Agent Communication (`agent_client.rs`)

Every controller-to-agent RPC goes through `agent_client::connect()`, which authenticates the controller to the agent. Two modes:
- **Native plugin (`spur`):** Pings the agent (no credential) to learn its advertised audience and boot epoch, then mints a `ControllerRpc` token for that audience
- **JWT plugin:** Mints a short-lived JWT with subject `_spur_controller`, signed with `[auth] jwt_key`

Credentials are minted per connection (not cached). The design assumes short-lived connections (one RPC per connect). Channel tunables (connect timeout, keepalive interval/timeout) are reconfigurable via `set_channel_tuning()`.

## Native Keys (`native_keys.rs`)

Process-wide Ed25519 signing key sets, installed once at startup:

| Key set | Path env | Purpose |
|---|---|---|
| Peer (forwarding) | `SPUR_CONTROLLER_SIGNING_JWKS` | Sign forwarded identity envelopes between controllers |
| Execution credential | `SPUR_CRED_SIGNING_JWKS` | Sign job/step execution credentials for the agent |
| Node identity | `SPUR_NODE_SIGNING_JWKS` | Sign node identity tokens for agent registration |
| Controller RPC | (derived from peer keys) | Sign controller-to-agent RPC credentials |

All four are `None` when `[auth] plugin` is not `spur`. `sign_dispatch_credential()` and `sign_step_credential()` produce empty strings when no keys are loaded.

## Metrics (`metrics_server.rs`, `metrics_proto.rs`)

OpenMetrics 1.0 HTTP export on port 6822 (configurable). Endpoints:

`/metrics` (alias `/metrics/jobs`), `/metrics/nodes`, `/metrics/partitions`, `/metrics/rpc`, `/metrics/scheduler`, `/metrics/k8s`, `/metrics/jobs-users-accts`, `/metrics/auth`, `/metrics/audit`

`RpcStatsCollector` and `SchedStatsCollector` accumulate in-memory counters. Cluster state snapshots are derived from `ClusterManager` on each scrape.

## Dispatch Tracker (`dispatch_tracker.rs`)

Leader-local record of launches currently on the wire. `DispatchInFlight` is an RAII guard — the launch is released on drop, so a panic or early return cannot permanently exempt a node from orphaned-placement reconciliation. Used by `abort_orphaned_placements` to distinguish a genuinely-abandoned reservation from one still being dispatched.

## PMIx Dispatch (`pmix_dispatch.rs`)

Multi-node PMIx launch is a two-phase process: `PreparePmix` (PMIx server_start on each node) then `LaunchJob`. Each phase gets its own dispatch deadline. K8s virtual agents are rejected at submit for multi-node PMIx (not supported). Rollback (`ReleasePmix`) has a short independent timeout (5s) since it is best-effort.

## Common Pitfalls

**Config reload vs restart:** `scontrol reconfigure` re-reads `spur.conf` and atomically swaps `ClusterManager.config`. But several values are captured once at startup and are restart-only: the gRPC listen address, the Raft peer list, the scheduler tick interval, the topology tree, the JWT signing key, and the database pool.

**WAL size limit:** A single `WalOperation::JobSubmit` becomes one Raft log entry retained in every snapshot. `MAX_JOB_SPEC_SIZE` (4 MiB serialized) caps it. The check measures serialized size without allocating.

**Raft message sizes:** The Raft-internal gRPC port uses 32 MiB message limits (vs tonic's 4 MiB default). Snapshot chunks are 1 MiB raw but inflate ~3.4x in JSON. Invariant: `4 * RAFT_SNAPSHOT_MAX_CHUNK_SIZE < RAFT_MAX_MESSAGE_SIZE`.

**PassthroughCodec for forwarding:** Followers must send the exact bytes they signed. Re-encoding with prost would break the digest because `map` fields iterate in random order. `PassthroughCodec` sends pre-encoded bytes.

**Accounting caches fail-closed:** Before the first successful database fetch, `accounting_block()` holds every job that names a QOS or account in `Pending` with reason `AssocGrpJobsLimit`. This prevents running jobs against unvalidated limits. The hold clears automatically once the cache loads.

**Scheduler panic recovery:** `schedule()` runs inside `catch_unwind`. A panic skips one cycle and logs the error; the loop continues on the next tick.

**Leadership grace periods:** After a leadership change, several watchdogs wait before acting:
- Orphaned-placement sweep: 2x `dispatch_timeout_secs`
- Node health checker: max(`heartbeat_timeout_secs`, `health_tick_secs`)
- The scheduler tracks `leader_since` via a watch channel to detect rapid lose-then-regain flips within a single tick

**Dispatch cooldown:** After a resources-unavailable reject, a node is held out of dispatch for a cooldown period (`node_dispatch_cooldowns`). Leader-local, never persisted.

**Node registration serialization:** Per-node mutex (`node_registration_locks`) ensures concurrent (re-)registrations for the same node serialize, preventing a stale label diff from being applied over a fresh one.

**`cluster.rs` apply is the only write path:** No code may mutate `jobs`, `nodes`, `partitions`, `reservations`, `steps`, `tokens`, or `k0s` except through `apply_operation()`. Any other mutation would diverge from the WAL and break replication.

**Testing patterns:** `server.rs` and `rest/handlers.rs` have extensive test suites that construct a `ClusterManager` with `new()` + direct `apply_operation()` calls (no Raft), then drive handlers through `ControllerService` or Axum `oneshot()`. This tests the handler logic without election timing. The `test_support` module in `server.rs` provides `no_leader_service()` and `test_slurm_config()` helpers.

**Idle-fill / borrow semantics:** Jobs over their QOS group-node cap may borrow spare capacity, but this is evaluated at the team level (total legitimate + borrowed), not per-run. A per-run check is wrong once a QOS has 2+ borrowed runs: each measures itself against the same headroom, all read "became legitimate", and the reclaimable set comes back empty while the team physically sits over its cap.

**Het jobs, burst-buffer, and license jobs cannot idle-fill:** They are structurally excluded because re-running their admission gates is either unsound (BB gate mutates staging state) or would allow a candidate to take a resource an in-quota job just claimed (license contention map).

**`entrypoint.sh` confusion:** The root `entrypoint.sh` is the CI dev-container helper. The runtime container entrypoint is `docker/entrypoint.sh`. This is a spur-wide convention inherited from the parent project.

**Job submission size check:** `check_submission_size()` uses a `ByteCounter` write sink to measure serialized size without allocating the output. The Raft log entry is re-serialized by openraft on propose, so this avoids a double allocation.

**Stepd recovery:** When a step supervisor detects a partial cohort (some nodes missing their supervisor), it reports to the controller. The controller probes all expected nodes, waits a grace period (15s), then fences (requeues) the job. `claim_stepd_recovery_fence()` serializes concurrent reporters under a lock so two past-grace reports cannot both fence the same run.

**Migration advisory lock:** Database migrations use `pg_advisory_xact_lock(0x53505552, 1)` to serialize across concurrent controllers, since migrations now rewrite data and build unique indexes.
