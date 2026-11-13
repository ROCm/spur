# AGENTS.md -- spur-core

The foundational shared-types crate for the Spur scheduler. Every other crate in
the workspace depends on it: `spurctld` (controller), `spurd` (agent),
`spur-cli` (squeue, srun, sbatch, sacctmgr, scontrol, sinfo), `spur-sched`
(scheduler), `spur-k8s` (operator), `spur-metrics`, `spur-stepd` (step daemon),
`spurauthd` (auth daemon), and `spur-tests`. No runtime logic lives here --
only data models, configuration, authentication primitives, and small pure
helpers.

Treat this crate as a public API surface: a field rename, a removed enum
variant, or a missing `#[serde(default)]` can crash a running cluster on
upgrade replay.

---

## 1. Directory structure

```
src/
  lib.rs                 -- pub mod declarations (50 modules)
  config.rs              -- SlurmConfig (TOML), all config sections (~5000 lines)
  job.rs                 -- JobSpec, Job, JobState, PendingReason, state machine
  node.rs                -- Node, NodeState, NodeEvent, NodeSource, NodeOverlay
  resource.rs            -- ResourceSet, ResourceAllocations, GpuResource, parse_gres
  gpu_request.rs         -- GpuRequest, GpuDemand, resolve_gpu_demand, distribute_total
  gpu_binding.rs         -- GpuBinding, BoundGpu, per-node GPU selection validated at dispatch/step
  gpu_identity.rs        -- stable_id-to-silicon mapping, LayoutDiff, safe GPU renames
  partition.rs           -- Partition, PartitionState, PreemptType, PreemptMode
  reservation.rs         -- Reservation, ReservationFlags, overlap/access checks
  accounting.rs          -- Account, Qos, QosLimits, AccountLimits, TresRecord, TresType
  account_limits.rs      -- Association-level limit enforcement (mirrors qos.rs)
  qos.rs                 -- check_qos_limits, check_qos_submit_limits, idle-fill eligibility
  step.rs                -- JobStep, StepState, TaskDistribution, step IDs (batch/extern)
  auth.rs                -- Identity, BearerAuth, JWT generate/verify, BearerOutcome
  hooks.rs               -- Prolog/epilog/job_submit hooks, SubmitHookOutcome, whitelist
  wal.rs                 -- WalOperation enum (Raft log entries), all state mutations
  hostlist.rs            -- Slurm-style hostlist expand/compress (node[001-004])
  dependency.rs          -- Job dependency expression parsing (afterok:N, singleton)
  array.rs               -- Job array spec parsing (0-99%10)
  block_file.rs          -- Fabric block-file parsing (INI-like), DefinedBlock, BlockFile
  burst_buffer.rs        -- BbStageState, burst-buffer capacity reservation
  constraints.rs         -- GPU constraint expressions (gpu.type==MI300X), Predicate, Constraints
  core_layout.rs         -- Controller CPU partitioning (spec-reserved, GPU-reserved, general pools)
  cpu_topology.rs        -- CpuTopology from sysfs: sockets, NUMA, cores, hyperthread, cpulist notation
  device_topology.rs     -- DeviceTopology proto conversion (PCI BDF, NUMA, VRAM, fabric observation)
  topology.rs            -- TopologyTree, Switch, SwitchConfig, locality-aware placement
  fabric.rs              -- FabricView builder: groups GPUs into fabric blocks by domain, tracks health
  fabric_binding.rs      -- FabricBinding, per-node fabric block selection validated at dispatch/step
  admission.rs           -- AdmissionToken, node admission control
  privilege.rs           -- sudo/wheel privilege checks (shared CLI + controller)
  rbac.rs                -- Role-based access (Administrator/Operator/User)
  end_reason.rs          -- CancelProvenance, NodeFailure, Actor, LastRequeue
  process.rs             -- shell_exit_code helper (exit status or 128 + signal)
  time_spec.rs           -- --begin / --deadline time expression parsing
  spur_env.rs            -- SpurEnv builder (SPUR_* + SLURM_* twin environment vars)
  mpi.rs                 -- PMIx-related types and constants
  k0s.rs                 -- K0sRole, K0sPhase, K0sClusterState, k0s config YAML generation
  peer.rs                -- Peer address normalization (dual-stack IPv4-mapped rendering)
  version.rs             -- Build version info
  quota_names.rs         -- K8s quota namespace/SA naming (account_namespace, sanitize_dns_label)
  audit_metrics.rs       -- Counters for the OpenMetrics audit surface
  task_launch.rs         -- Multi-task/multi-node step launch helpers (batch + srun dispatch)
  native_cred.rs         -- Native per-RPC credential structure (HMAC-signed)
  native_jwks.rs         -- JWKS key set loading and rotation
  native_mint.rs         -- Credential minting (sign a UserRpcCredential)
  native_peer.rs         -- Controller-peer forwarded identity envelopes (Raft follower-to-leader)
  native_replay.rs       -- Nonce-based replay cache
  native_service.rs      -- Controller-to-agent service credentials and node identity
  native_exec.rs         -- Exec-time credential for job/step launch
  native_metrics.rs      -- Counters for native auth verify/reject
```

---

## 2. Configuration (`config.rs`)

`SlurmConfig` is the top-level TOML structure (`spur.conf`). Every section uses
`#[serde(default)]` so a minimal config works. Major sections:

| Section          | Struct               | Purpose |
|------------------|----------------------|---------|
| `[controller]`   | `ControllerConfig`   | gRPC/Raft listen addrs, peers, heartbeat/dispatch timeouts, state_dir |
| `[scheduler]`    | `SchedulerConfig`    | Backfill interval, fairshare halflife, preempt_type, idle-fill knobs |
| `[accounting]`   | `AccountingConfig`   | PostgreSQL URL, fairshare refresh, QOS enforcement flags |
| `[auth]`         | `AuthConfig`         | Plugin (jwt/spur/none), AuthMode, jwt_key, cluster_admins, private_data |
| `[network]`      | `NetworkConfig`      | WireGuard mesh toggle, CIDR, agent port |
| `[kubernetes]`   | `KubernetesConfig`   | External k8s integration (SpurJob CRDs) |
| `[cluster]`      | `ClusterConfig`      | Native k0s lifecycle (spur k8s up/down), CNI, storage provisioner |
| `[isolation]`    | `IsolationConfig`    | setuid, namespaces, seccomp, landlock |
| `[cgroup]`       | `CgroupConfig`       | cgroup-v2 CPU/memory/device enforcement |
| `[hooks]`        | `HooksConfig`        | Prolog/epilog/job_submit script paths |
| `[devices]`      | `DevicesConfig`      | GPU auto-detect, CDI dirs, GRES definitions |
| `[mpi]`          | `MpiConfig`          | PMIx plugin paths, modex timeouts |
| `[health]`       | `HealthConfig`       | Scheduled health-check jobs, max_unavailable |
| `[metrics]`      | `MetricsConfig`      | OpenMetrics HTTP export (port 6822) |
| `[rest_api]`     | `RestApiConfig`      | Slurm-compatible REST (port 6820, off by default) |
| `[rlimits]`      | `RlimitsConfig`      | RLIMIT_MEMLOCK for job processes |
| `[topology]`     | `TopologyConfig`     | Switch hierarchy for locality-aware scheduling |
| `[[partitions]]` | `PartitionConfig`    | Queue definitions (nodes, limits, ACLs, preempt_mode) |
| `[[nodes]]`      | `NodeConfig`         | Per-node or selector-based CPU/memory caps, features |

Default ports: gRPC 6817, REST 6820, Raft 6821, Metrics 6822, Agent 6818,
WireGuard 51820.

---

## 3. Job model

### JobSpec

User-submitted job description. Fields cover resource requests (nodes, tasks,
CPUs, memory, GPUs via `gres`/`gpus`/`gpus_per_node`/`gpus_per_task`),
execution (script, argv, environment, work_dir), scheduling (QOS, priority,
reservation, dependency, constraint, begin_time, deadline), containers, MPI,
arrays, and K8s pod options.

`JobSpec` is serialized into the Raft WAL via `WalOperation::JobSubmit`. Every
new field must deserialize when absent: `#[serde(default)]`, or a plain
`Option<T>` (an `Option` with `deserialize_with` still needs `default`).
Without this the controller crashes replaying pre-upgrade entries. A frozen
v0.5.1 payload test in `wal.rs` guards this.

### JobState

13-variant enum matching Slurm's state model. `Job::transition()` is the
authoritative source; the full edge set:

```
Pending    -> Running | Cancelled | Deadline
Running    -> Completing | Completed | Failed | Cancelled | Timeout |
              NodeFail | Preempted | Suspended | OutOfMemory
Completing -> Completed | Failed | Cancelled | Timeout | NodeFail | OutOfMemory
Suspended  -> Running (resume) | Cancelled | Completing | Completed | Failed |
              Timeout | NodeFail | OutOfMemory
Preempted  -> Pending (requeue) | Cancelled | Completing (preempt-cancel drain)
Timeout | NodeFail | Failed -> Pending (auto-requeue)
```

Note: `Running -> Requeued` is NOT a valid transition (transition() never takes
it). Admin requeue uses a separate `requeue_to_pending()` path.

Key properties:
- `is_terminal()`: Completed, Failed, Cancelled, Timeout, NodeFail, Deadline, OutOfMemory
- `is_active()`: Running, Completing, Suspended
- `is_finalized()`: terminal + Preempted + Requeued (end of a run, not necessarily end of job)
- `Preempted` and `Requeued` are intermediate: they finalize a run but the job returns to Pending

The `Job::transition()` method enforces the state machine. `apply_transition()`
adds WAL replay tolerance (same-state is NoOp). `requeue_to_pending()` is a
separate path for admin requeue so nothing else can resurrect a terminal job.

### Job (internal record)

Held in controller memory. Key fields beyond the spec:
- `run_attempt`: monotonic epoch, bumped per dispatch. Guards stale completion reports.
- `node_completions`: per-node exit status, drives `derived_completion()`.
- `time_limit_signaled_at`, `cancel_signaled_at`: replicated instants for deterministic verdicts.
- `suspended_secs` + `suspended_at`: wall-clock accounting for suspend/resume.
- `idle_fill`: this run uses spare capacity outside QOS quota.
- `preferred_nodes`, `last_sched_eval`: transient per-scheduling-pass hints (`#[serde(skip)]`).

### PendingReason

60+ variants covering Slurm's WAIT_*/FAIL_* reason codes. Display strings are
byte-exact Slurm output (scraped by monitoring tools). New variants are added
alongside their enforcement logic to avoid dead code in the WAL contract.

---

## 4. Node model

### NodeState

9 variants: Idle, Allocated, Mixed, Down, Drain, Draining, Error, Unknown,
Suspended.

Transitions are event-driven via `NodeState::transition(event, admin_locked)`:
- `Register`: Unknown -> Idle
- `HeartbeatTimeout`: any live state -> Down (but Down/Drain are no-ops)
- `HeartbeatRecovered`: Down/Error -> Idle (blocked when `admin_locked`)
- `AdminSetState`: any -> any (operator override)
- `PowerSuspend`/`PowerResume`: Suspended lifecycle

`Draining -> Drain` is driven by `update_state_from_alloc()`, not `transition()`:
when a Draining node's last allocation is released, it promotes to Drain.

`admin_locked` prevents auto-recovery on a node an operator explicitly drained.
`update_state_from_alloc()` drives Idle/Mixed/Allocated transitions but never
overrides admin hold states.

### Node struct

Persisted in the Raft snapshot. Key fields:
- `total_resources` / `detected_resources`: post-clamp schedulable vs raw detected inventory.
- `alloc_resources`: consumed resources (add/subtract on job start/complete).
- `source`: NativeHost or Kubernetes (derived from agent registration metadata).
- `k0s_role`, `k0s_mesh_ip`, `k0s_pod_cidr`: native k0s cluster assignment.
- `weight`: scheduling preference (higher = preferred).
- `switch_name`: topology leaf switch (from config).

### NodeOverlay

Display-only condition on Idle nodes (Reserved, Planned) for sinfo output. Not
persisted; derived from live reservation/backfill state.

---

## 5. Resource types

### ResourceSet (inventory)

Per-node schedulable inventory: `cpus`, `memory_mb`, `gpus: Vec<GpuResource>`,
`generic: HashMap<String, u64>`, `generation`. The `clamped()` method applies
operator caps and reserved memory from `NodeConfig`.

### GpuResource

Individual GPU: `device_id` (positional), `gpu_type` (e.g. "mi300x"),
`memory_mb`, `peer_gpus`, `link_type` (PCIe/XGMI/NVLink), `stable_id`.
Allocation keys on `stable_id`, not positional `device_id`, so a mid-set GPU
removal does not renumber existing allocations.

### ResourceAllocations (consumed)

Tracks what is in use: `cpus`, `memory_mb`, `devices: HashMap<String, Vec<AllocatedDevice>>`,
`generation`. GPU entries are injectable (count=1 each, keyed by stable_id);
non-GPU GRES (bandwidth, licenses) are countable (one entry, count>1).
`add()`/`subtract()` use saturating arithmetic.

### GpuRequest / GpuDemand (`gpu_request.rs`)

Three user-facing GPU forms (total, per-node, per-task) collapse into
`GpuDemand` via `resolve_gpu_demand()`. At most one form may be set (a `gpu:`
entry in `gres` counts as implicit per-node). `distribute_total()` performs
greedy availability-aware placement for the total case.

### parse_gres

Parses `"gpu:mi300x:4"` -> `(name, type, count)`. Trims whitespace, rejects
empty strings.

---

## 6. Partition model

`Partition`: name, state (Up/Down/Drain/Inactive), node membership (hostlist
pattern or label selector), time limits, node limits, access control
(allow/deny accounts, groups, QOS), preempt_mode, priority_tier.

`PreemptType` (cluster-wide): None or QosPriority.
`PreemptMode` (per-partition, per-QOS): Off, Cancel, Requeue, Suspend. Ranked
by `aggressiveness()` for multi-partition jobs.

---

## 7. Reservation model

`Reservation`: name, time window, node list, user/account ACLs, flags
(maint, ignore_jobs, no_hold_jobs, overlap). Overlap is permitted when either
reservation has the overlap or maint flag.

Functions: `is_active()`, `allows_user()`, `prospective_overlap()` (blocks
unauthorized jobs from nodes reserved in the future),
`running_jobs_overlap_start()` (prevents creating a reservation that conflicts
with running work).

---

## 8. Accounting types

### TresType / TresRecord

Trackable resources: Cpu, Memory, Energy, Node, Gpu, Billing. `TresRecord` is a
wrapper struct around `values: HashMap<TresType, u64>` with `parse()`/`format()`
for the `cpu=N,node=N` wire format. Rejects unit-suffixed values (`mem=1G`) and
unknown types.

### Account / AccountLimits

Hierarchical account tree with per-account limits: max_running_jobs,
max_submit_jobs, grp_submit_jobs, max_tres_per_job, grp_tres, max_wall_minutes.

### Qos / QosLimits

Quality of Service: priority, usage_factor, preempt mode, preempt allow-list,
deny_on_limit flag, idle_fill_preemptable flag. Limits: per-user (max_jobs,
max_submit_jobs, max_tres), per-job (max_tres, max_wall), group-wide (grp_tres,
grp_submit_jobs, grp_wall_minutes). The `INFINITE` sentinel (`u32::MAX`)
encodes "no limit" on the wire, distinct from `0` ("block all").

### QOS enforcement (`qos.rs`)

`check_qos_limits()`: scheduler path, projects candidate job onto current load.
`check_qos_submit_limits()`: submit-time count caps (always deny, regardless of
DenyOnLimit). `check_qos_standalone_limits()`: standalone resource/wall breach
for DenyOnLimit submission gate. `grp_node_is_sole_blocker()`: idle-fill
eligibility (the job is over its group node quota and nothing else).

---

## 9. Auth

### Identity

Authenticated caller: user, uid, gid, is_admin, trusted_unix. Created from JWT
verification, native credential verification, or the `auth_none()` fallback.
`CONTROLLER_SUBJECT = "spurctld"` identifies controller-to-agent calls.

### BearerAuth

Shared verifier for controller, agent, REST, and k8s operator middleware.
Supports JWT (`plugin = "jwt"`) and native HMAC credentials (`plugin = "spur"`).

### AuthMode (defined in config.rs, not auth.rs)

Three modes: Disabled (no auth), Permissive (verify if present, allow anonymous),
Required (must present valid credential). Permissive rejects an invalid
credential -- it tolerates absence, not forgery.

### Token generation

`generate_token()` / `verify_token()`: HS256 JWT with sub, uid, exp, iat, admin
claims. `bind_job_spec()` overwrites spec user/uid/gid from verified identity.

---

## 10. Hooks (`hooks.rs`)

### Prolog/Epilog

`run_hook()`: async execution of prolog/epilog scripts with `HookContext`
environment (SPUR_JOB_ID, SPUR_JOB_GPUS, etc. plus SLURM_* twins). Requires
absolute path. Validates ownership/permissions via fstat on the open handle
(closes TOCTOU race). Falls back to /tmp if work_dir is inaccessible.

### Job Submit Hook

`run_submit_hook()`: receives resolved spec as JSON on stdin. Exit 0 blank =
accept; exit 0 + JSON = modify (whitelisted fields only); non-zero = reject
(stderr to user). 30s timeout, 1MB output cap. Whitelisted fields: qos,
partition, account, constraint, comment, reservation, priority,
time_limit_minutes, begin_time, gres, hold.

`SubmitHookOutcome`: Accept, Reject(reason), Modify(SubmitHookChanges).
Identity fields (user, uid, script, resource counts) are absent from the
whitelist by construction.

---

## 11. WAL (`wal.rs`) -- Persistence sensitivity

`WalOperation` is the Raft log entry enum. Every durable state mutation goes through it:
job submit/start/complete/cancel/suspend/resume/preempt/requeue, node
register/update/state-change/remove, partition/reservation CRUD, k0s lifecycle.

### Critical invariants

1. **Every new field on a WAL-serialized type must deserialize when absent:
   `#[serde(default)]`, or a plain `Option<T>` (an `Option` with
   `deserialize_with` still needs `default`).**
   The controller deserializes the entire WAL on startup. A field that cannot
   deserialize from a missing key crashes the controller when it replays an
   entry written before the field existed. Frozen-payload tests in `wal.rs`
   guard against this.

2. **Never rename or remove a variant of `WalOperation`, `JobState`,
   `PendingReason`, `NodeState`, or any type nested inside them.** The variant
   name is the JSON discriminant on the wire. A rename makes old entries
   unreadable. A removal crashes replay.

3. **New WalOperation variants must be appended, not inserted.** The enum uses
   serde's externally-tagged representation; variant order does not affect the
   wire format, but insertion between existing variants risks merge conflicts.

4. **Leader-stamped instants (`at` fields)**: Operations that date something a
   user reads back carry an `at: Option<DateTime<Utc>>` stamped by the
   proposing leader. Followers and replays use this instant, never their own
   clock. `stamp_occurred_at()` is exhaustive-match enforced: a new dateable
   variant that omits `at` is a compile error.

5. **Job and Node snapshots** are serialized whole into the Raft snapshot
   (JSON). The same `#[serde(default)]` rule applies to `Job`, `Node`,
   `JobSpec`, `ResourceSet`, `ResourceAllocations`, and every type they embed.
   Tests like `a_node_snapshot_without_k0s_last_error_still_deserializes`
   (in `node.rs`) verify this.

6. **`#[serde(skip)]` fields** (`preferred_nodes`, `last_sched_eval`) are
   transient per-scheduling-pass data that must not be persisted. They are
   rebuilt each tick.

7. **A new field a patch entry can clear must be `Option<Option<T>>` with
   `#[serde(default, deserialize_with = "clear_or_set", skip_serializing_if =
   "Option::is_none")]`**, exactly like `PartitionUpdate::grace_time`: absent
   is unchanged, `null` clears, a value sets.
   - Never a bare `Option<Option<T>>`: JSON writes `None` and `Some(None)` both
     as `null`, so the clear is lost on followers and on replay, while the
     leader (which applies its in-memory copy) looks correct.
   - Never convert an existing always-serialized field to `clear_or_set`: old
     entries hold `null` for "unchanged" and would replay as a clear. Legacy
     fields (`max_time_minutes`, `default_time_minutes`, `max_nodes`,
     `preempt_exempt_time`) stay `Option<T>` and record clears in
     `PartitionUpdate::clear` (`PartitionClears`).
   - Only to make another existing, always-serialized `PartitionUpdate` field
     clearable, add a flag to `PartitionClears`; never use it for a new field.
   - Every clearable field needs a serde round-trip test of set, clear and
     unchanged plus frozen wire forms (see the `grace_time` and `clear` tests
     in `wal.rs`), and an apply test that replays the JSON.

---

## 12. Step model (`step.rs`)

`JobStep`: sub-execution within a running job (srun inside sbatch). Tracks its
own resources, task distribution (Block/Cyclic/Plane/Arbitrary), state
(Pending/Running/Completed/Failed/Cancelled).

Special step IDs match Slurm's `slurm.h` sentinels: `STEP_BATCH` (0xFFFFFFFB),
`STEP_EXTERN` (0xFFFFFFFC), `STEP_INTERACTIVE` (0xFFFFFFFA). User srun steps
are numbered 0, 1, 2, ... below `STEP_RESERVED_MIN`.

`distribute_tasks()`: task-to-node mapping. `truncate_step_name()`: bounds name
to 256 bytes (must be applied before proposing, never in apply/restore).

---

## 13. Common pitfalls

### Breaking changes

- Adding a field to `JobSpec`, `Job`, `Node`, `ResourceSet`, `GpuResource`,
  `ResourceAllocations`, `Partition`, `Reservation`, or any type nested inside
  `WalOperation` that cannot deserialize when absent will crash the controller
  on upgrade. Use `#[serde(default)]`, or a plain `Option<T>` (an `Option`
  with `deserialize_with` still needs `default`). This is the single most
  common review finding.

- Renaming a `PendingReason` variant changes its wire name in the Raft log.
  A frozen-wire-name test (`grp_wall_reason_has_a_frozen_wire_name`) guards
  the newest variant; add one for any new variant that carries a non-obvious
  name.

- Removing a `JobState` or `NodeState` variant makes every WAL entry and
  snapshot that references it undeserializable. The proto roundtrip tests
  (`job_state_proto_discriminants_match_core`) would also fail.

### State machine gotchas

- `Preempted` is NOT terminal. It finalizes a run and returns the job to
  Pending (requeue mode) or routes through Completing (cancel mode). Code
  that checks `is_terminal()` on a preempted job will miss it.

- `occupies_resources()` is the correct predicate for quota/occupancy checks,
  not `state == Running`. A Pending job with a reserved placement
  (`allocated_nodes` set before dispatch) also occupies capacity.

- `begin_time` holds serve multiple purposes: user `--begin`, preemption
  requeue cool-off, and launch-failure backoff. Code that clears `begin_time`
  must understand which hold it is clearing.

- The `run_attempt` epoch prevents stale completion reports from discharging
  a requeued job's new run. WAL entries that interact with a run should
  carry and gate on `run_attempt` when possible. `JobComplete`,
  `JobStepComplete`, and `JobLaunchFailureDetail` also do not carry it.
  Some ops that can race a requeue (JobCancelSignaled, JobSuspend,
  JobResume, JobUserRequeue, JobEvict) do not carry it either.
  `JobTimeLimitSignaled` is legacy (`#[cfg(test)]` only); the live path is
  `JobTimeLimitExpired` which does carry `run_attempt`.

### Resource accounting

- GPU allocation keys on `stable_id`, never positional `device_id`. After a
  mid-set GPU removal, positional IDs renumber but stable IDs do not. Code
  that compares `device_id` values across allocation boundaries will
  double-book GPUs.

- `ResourceAllocations::subtract()` uses saturating arithmetic and prunes
  drained entries. Over-release clamps at zero rather than wrapping.

### Config validation

- `AuthConfig.resolved_jwt_key()` is called once at startup and cached. It
  is NOT updated by `reconfigure`.

- `rest_api.enabled` defaults to false. When enabled, the REST surface
  authenticates requests via `BearerAuth` (JWT or native credentials,
  governed by Required/Permissive/Disabled modes). Submit endpoints reject
  unauthenticated callers before dispatching.

- CIDR validation (`is_valid_cidr`, `cidrs_overlap`) is IPv4-only because
  the k0s provisioning paths are all `Ipv4Addr`.
