# Spur Architecture

## 1. System overview

Spur is an AI-native job scheduler written in Rust, designed as a drop-in
replacement for Slurm. It provides Slurm-compatible CLI, REST API, and C FFI
surfaces while internally using Raft consensus, gRPC transport, WireGuard mesh
networking, and GPU-first scheduling.

### Daemon types

| Daemon | Binary | Role | Port |
|---|---|---|---|
| **spurctld** | `spurctld` | Controller. Serves the gRPC API (SlurmController + SlurmAccounting) on 6817, optional REST API (Axum) on 6820, and Raft consensus on 6821. Accounting runs in-process backed by PostgreSQL. | 6817 (gRPC), 6820 (REST, opt-in), 6821 (Raft) |
| **spurd** | `spurd` | Node agent. Runs on each compute node. Registers with the controller, sends heartbeats, receives job launch/cancel commands, manages k0s component lifecycle. | 6818 |
| **spurstepd** | `spurstepd` | Per-job supervisor. Spawned by spurd for each launched job (and each numbered srun step). Survives spurd restarts. Separate binary, separate trust boundary. | Unix socket (local) |
| **spur-cli** | `spur` | Multi-call CLI. Invoked as `spur <command>` or via Slurm-compatible symlinks (`sbatch`, `squeue`, `sinfo`, etc.). Talks to spurctld on port 6817. | N/A (client) |
| **spurauthd** | `spurauthd` | Local credential mint. Listens on `/run/spur/<cluster>/auth.sock` on login and compute nodes; issues short-lived tokens for CLI and step processes. | Unix socket (local) |
| **spur-k8s-operator** | `spur-k8s-operator` | Kubernetes operator. Watches Spur CRDs and projects Spur accounts into K8s quota/RBAC. Runs inside the K8s cluster. | Operator (in-cluster) |

Slurm splits accounting into `slurmdbd` and REST into `slurmrestd`. Spur
collapses both into spurctld, eliminating two network-facing daemons and
their inter-daemon wiring.

## 2. Data flow: job submission through execution

```text
User (sbatch/srun)
    |
    v
spur-cli --- gRPC ---> spurctld (SlurmController.SubmitJob)
                            |
                            | 1. Validate spec, resolve partition/QOS/account
                            | 2. Propose WalOperation::JobSubmit to Raft
                            | 3. Raft commits -> job enters PENDING
                            |
                        scheduler_loop (leader only, event-driven + periodic tick)
                            |
                            | 4. BackfillScheduler evaluates pending queue
                            | 5. Selects nodes via cons_tres allocation
                            | 6. Proposes WalOperation::JobStart (nodes, resources)
                            |
                            | 7. Dispatch: gRPC fan-out to selected spurd agents
                            v
                        spurd (SlurmAgent.LaunchJob)
                            |
                            | 8. Allocate cgroup, inject GPU devices
                            | 9. Fork spurstepd via double-fork
                            v
                        spurstepd
                            |
                            | 10. Execute user script/command
                            | 11. Monitor process, enforce limits
                            | 12. On exit: notify spurd via Unix socket
                            |
                        spurd -> spurctld (ReportJobStatus RPC carries completion)
                            |
                            | 13. Propose WalOperation::JobNodeComplete
                            | 14. When all nodes report: JobComplete -> COMPLETED/FAILED
```

For multi-node PMIx jobs, step 7 is two-phase: `PreparePmix` (start PMIx
servers, exchange peer info) then `LaunchJob` on all nodes once preparation
succeeds. `StartJob` releases the workload gate after the controller commits
the Running state so the job cannot observe itself as Pending.

## 3. Cluster topology

### Single controller (default)

```text
                 +------------------+
                 |    spurctld      |
                 | (Raft 1-member)  |
                 | gRPC :6817       |
                 | REST  :6820 (opt)|
                 | Raft  :6821      |
                 | Metrics :6822    |
                 +--------+---------+
                          |
            +-------------+-------------+
            |             |             |
     +------+------+ +---+---+ +-------+------+
     | spurd :6818 | | spurd | | spurd :6818  |
     | (compute-1) | | (c-2) | | (compute-N)  |
     +-------------+ +-------+ +--------------+
```

### HA (multi-controller)

Multiple spurctld instances form a Raft cluster. Peers are listed in
`[controller].peers` in spur.conf. Leader handles writes; followers
auto-forward client requests. The Raft internal transport
(`raft_internal.proto`) runs on a separate listen address
(`[controller].raft_listen_addr`).

```text
  spurctld-1 <----Raft----> spurctld-2 <----Raft----> spurctld-3
  (leader)                  (follower)                 (follower)
```

Agents take a comma-separated `SPUR_CONTROLLER_ADDR` with automatic
failover. The controller cluster presents a single logical endpoint;
client RPCs hitting a follower are forwarded to the leader transparently.

## 4. Raft consensus

Raft is always-on. Even a single-node deployment runs a 1-member Raft
cluster that self-elects instantly (same pattern as Apache Kudu).

### State machine

All state-mutating operations go through `WalOperation`, proposed via
`ClusterManager::propose()` and applied through the `StateMachineApply`
trait. The Raft log is the sole durable store. Some transient fields
(e.g., heartbeat timestamps, dispatch status) are written directly
outside the WAL for performance.

### WAL operations (key variants)

| Operation | Purpose |
|---|---|
| `JobSubmit` | New job enters the system |
| `JobStateChange` | Transition between states (Pending->Running, etc.) |
| `JobStart` | Attach allocation (nodes, resources, per-node slices) |
| `JobComplete` | Terminal state reached |
| `JobNodeComplete` | Per-node completion report |
| `JobTimeLimitSignaled` | Wall-clock exhaustion signal |
| `JobCancelSignaled` | Cancellation signal recorded |
| `JobPreemptRequeue` | Atomic preempt + requeue to Pending |
| `JobPreemptCancel` | Atomic preempt + cancel |
| `NodeRegister` | New node joins the cluster |
| `NodeStateChange` | Node state transition |
| `ReservationCreate/Update/Delete` | Reservation management |
| `PartitionCreate/Update/Delete` | Partition management |
| `K0sSetPhase` | Kubernetes cluster phase transitions |

### Storage layout

```text
{state_dir}/raft/
    vote.json          # Current Raft vote
    log/*.json         # WAL entries (JSON-serialized)
    snapshot.json      # Full state snapshot
```

WAL entries serialize to JSON. A new field without `#[serde(default)]` or
`Option` breaks replay of old entries -- this is the primary backward
compatibility constraint.

### Leader election

On startup, `start_raft_with_recovery_mode()` initializes the openraft
instance. Single-node clusters self-elect. Multi-node clusters participate
in standard Raft leader election. The controller spawns a leadership watcher
(`spawn_leadership_watcher()`) that feeds term+instant to the health checker
and the orphan-sweep grace period. The k0s reconciler uses `is_leader()`;
time-limit enforcement uses `ensure_leader()`.

### Snapshots

`SpurStore` supports snapshot build and restore. Snapshots are shipped in
chunks of 1 MiB raw bytes (`RAFT_SNAPSHOT_MAX_CHUNK_SIZE`); JSON encoding
inflates these ~3.4x, so the gRPC message limit is 32 MiB
(`RAFT_MAX_MESSAGE_SIZE`).

## 5. gRPC services

### SlurmController (port 6817)

Served by spurctld. The primary API surface for clients, agents, and
operators.

**Job management:** SubmitJob, GetJobs, GetJob, CancelJob, CompleteJob,
SuspendJob, ResumeJob, UpdateJob, RequeueJob, RunStep, ExecInJob,
JobKeepalive, GetJobSteps, CreateJobStep, CompleteJobStep, ReportJobStatus.

**Node management:** RegisterAgent, Heartbeat, ReportStepdRecovery,
GetNodes, GetNode, UpdateNode, DrainNode, DeregisterNode, DeregisterAgent.

**Partition/Reservation:** GetPartitions, CreatePartition, UpdatePartition,
DeletePartition, CreateReservation, UpdateReservation, DeleteReservation,
ListReservations, GetFabricTopology, ValidateBlockFile.

**Cluster (k0s):** ClusterUp, ClusterDown, ClusterStatus,
ClusterKubeconfig, ClusterAddNodes, ClusterRemoveNodes.

**Diagnostics:** Ping, GetJobMetrics, GetNodeMetrics, GetRpcStats,
GetSchedStats, GetAssocMgrInfo, ResetDiagStats, Reconfigure.

**Admission:** CreateToken, ListTokens, RevokeToken.

### SlurmAgent (port 6818)

Served by spurd on each compute node. Called by spurctld for job dispatch
and cluster component management.

**Job execution:** LaunchJob, StartJob, CancelJob, SuspendJob,
PreparePmix, ReleasePmix, RunCommand, CancelStep,
RegisterJobAllocation, AwaitStep.

**Cluster components:** StartClusterComponent, StopClusterComponent,
GetClusterComponentStatus, CreateK0sJoinToken, DrainK8sNode,
DeleteK8sNode, GetKubeconfig.

**Networking:** ApplyMesh (pushes WireGuard peer set).

**Signals:** SignalJob.

**Diagnostics:** Ping, GetNodeResources, ProbeStepd, ExecInJob,
StreamJobOutput, InteractiveSession.

### SlurmAccounting (port 6817, in-process)

Served by spurctld alongside SlurmController. Backed by PostgreSQL.
Connects lazily; jobs with QOS/account constraints are held while admission
caches are cold, but once loaded a database outage does not block scheduling.

**Job history:** RecordJobStart, RecordJobEnd, GetJobHistory, GetUsage.

**Entity management:** CreateAccount, DeleteAccount, ListAccounts,
AddUser, RemoveUser, ListUsers, AddCoordinator, RemoveCoordinator,
ListCoordinators, CreateQos, DeleteQos, ListQos, ModifyUsage.

**Fairshare:** GetFairshareFactors.

**Audit:** GetTransactions.

## 6. Scheduling cycle

The scheduler loop runs on the Raft leader only, driven by both events
(job submission notifications) and a periodic tick (default interval
configurable via `[scheduler].interval_secs`).

```rust
loop {
    select! {
        _ = scheduler_notify.notified() => {}  // event-driven wake
        _ = interval.tick() => {}              // periodic fallback
    }

    ensure_leader().await;  // yields until this node holds the Raft lease

    // Pre-schedule maintenance
    cancel_unsatisfiable_dependency_jobs();
    drive_bb_stage_in();           // Burst buffer stage tracking
    sweep_orphaned_placements();   // Clean stale dispatches
    purge_expired_reservations();
    enforce_reservation_end_times();
    evict_expired_terminal_jobs();
    run_node_health_pass();

    // Classify pending queue
    (pending, idle_fill_candidates) = pending_jobs_with_idle_fill_candidates();
    nodes = nodes_off_dispatch_cooldown();

    // Run backfill scheduler
    assignments = BackfillScheduler.schedule(pending, nodes, partitions, ...);

    // Dispatch each assignment
    for assignment in assignments {
        propose WalOperation::JobStart via Raft;
        fan out LaunchJob RPCs to assigned agents;
        propose WalOperation::JobStateChange (Pending -> Running);
    }
}
```

### Backfill algorithm

1. Sort pending jobs: reservation holders first, then hoisted preemptors,
   then by descending priority, then by job_id (FIFO within a priority).
   Idle-fill candidates (jobs exceeding QOS group node quota) are appended
   after all in-quota jobs.
2. For each top-priority job, compute earliest start across suitable nodes,
   creating shadow reservations on per-node timelines.
3. For lower-priority jobs, check if they fit in timeline gaps without
   delaying any shadow reservation.
4. Idle-fill candidates are offered only spare capacity.

The scheduler uses `cons_tres` (consumable trackable resources) allocation:
CPU, memory, GPUs, and generic resources (licenses, etc.) are tracked
per-node with topology-aware placement (XGMI, NVLink affinity).

## 7. Job lifecycle

```text
                       +----------+
              submit   |          |  dependency/begin-time wait
         +------------>| PENDING  |<-----------+
         |             |          |             |
         |             +----+-----+             |
         |                  |                   |
         |            schedule+dispatch         |
         |                  |                   |
         |             +----v-----+       +-----+------+
         |             |          | requeue|            |
         |             | RUNNING  +------->| REQUEUED   |
         |             |          |        +------------+
         |             +----+-----+
         |                  |
         |    +-------------+-------------+----------------+
         |    |        |        |         |        |       |
         |  +-v------+ |  +----v-----+ +-v------+ | +-----v------+
         |  |TIMEOUT | |  |COMPLETING| |CANCELLED| | |OUT_OF_MEMORY|
         |  +--------+ |  +----+-----+ +---------+ | +------------+
         |         +----v---+   |              +----v----+
         |         | FAILED |   +---->+        |NODE_FAIL|
         |         +--------+        |        +---------+
         |                      +----v------+
         |                      | COMPLETED  |
         |                      +------------+
         |
         |             +-----------+       +-----------+
         |             | SUSPENDED |       | PREEMPTED |
         |             +-----+-----+       +-----+-----+
         |                   |                   |
         |          can reach COMPLETING     can reach COMPLETING
         |          + all terminal states    (-> PENDING if requeue mode)
         |          directly
         |
         +----> DEADLINE (pending past deadline)
```

**Note:** `REQUEUED` is not entered via `Job::transition()` -- it is an
accounting label set when a job is requeued. See `Job::transition` in
`spur-core/src/job.rs` as the authoritative state machine.

**States:**

| State | Meaning |
|---|---|
| PENDING | Waiting for resources, dependencies, or begin-time |
| RUNNING | Dispatched and executing on allocated nodes |
| COMPLETING | Some nodes reported; waiting for remaining |
| COMPLETED | All nodes reported, exit code 0 |
| FAILED | All nodes reported, non-zero exit |
| CANCELLED | Cancelled by user or admin |
| TIMEOUT | Wall-clock time limit exceeded |
| NODE_FAIL | Allocated node went down during execution |
| PREEMPTED | Evicted by higher-priority job |
| SUSPENDED | Paused by admin (SIGSTOP) |
| DEADLINE | Still pending past deadline |
| OUT_OF_MEMORY | OOM-killed (cgroup) |
| REQUEUED | End-of-run marker for admin requeue; job returns to PENDING |

Terminal states are persisted via `WalOperation::JobComplete`. The
`JobNodeComplete` WAL entry tracks per-node completion; when all allocated
nodes report, the job transitions to its final state. An `exit_code` of 0
yields COMPLETED; non-zero yields FAILED. Timeout, cancel, and node-fail
override the exit-code-based decision.

## 8. Node lifecycle

```text
  spurd starts
      |
      v
  RegisterAgent RPC ---> spurctld proposes WalOperation::NodeRegister
      |
      | state: UNKNOWN -> IDLE
      v
  heartbeat_loop (periodic HeartbeatRequest)
      |
      |--- heartbeat arrives: update last_seen, resources, running jobs
      |--- heartbeat timeout: flagged not-responding; DOWN after two failed probes
      |--- heartbeat recovered: state -> IDLE (unless admin-locked)
      |
      v
  AdminSetState(Drain) ---> DRAINING (if jobs running) or DRAIN (if idle)
      |
      | jobs complete on drained node
      v
  DRAINING -> DRAIN
      |
      v
  DeregisterAgent (SIGTERM) ---> node marked DOWN; record retained
```

**Heartbeat payload:** hostname, load average, free memory, WireGuard
mesh key, running job IDs with allocated GPU stable_ids, k0s component
status. Resource inventory and node address are not carried in heartbeats
(they are set at registration).

**Health checker:** runs on the leader at `health_tick_secs` interval.
Nodes missing heartbeats past `heartbeat_timeout_secs` are flagged
not-responding (no new work) and probed directly; one whose agent fails two
probes a health pass apart is marked DOWN, unless so many nodes are silent at
once that the leader suspects itself (`max_stale_node_fraction`).
Running jobs on a DOWN node are evicted (cancel sent to agents, steps
force-completed). A leadership grace period prevents false positives
immediately after election.

**Inventory refresh:** spurd periodically re-discovers local hardware
(GPU topology changes from partition switches). Changes are debounced
(seen-twice) and converged with the controller via re-registration.
A full GPU dropout (>0 to 0) is treated as a suspect scan and ignored.

## 9. K8s integration

Spur embeds k0s to provide a managed Kubernetes cluster on top of the
Spur-managed nodes. Two integration modes exist:

### Native k0s (embedded)

Enabled via `[cluster].enabled = true`. The controller runs a leader-gated
reconcile loop (`cluster_k8s::run`) that:

1. Selects a control-plane node (operator-pinned or auto-selected).
2. Allocates mesh IPs from the WireGuard CIDR.
3. Generates k0s config with pod/service CIDRs and CNI settings.
4. Drives `StartClusterComponent` RPCs to agents (k0s as a systemd unit).
5. Issues join tokens for workers via `CreateK0sJoinToken`.
6. Pushes WireGuard mesh membership via `ApplyMesh`.
7. Monitors node status; marks cluster degraded on provisioning timeout.

The k0s binary runs as a systemd unit managed by spurd, not as a child
process. It survives spurd restarts. Node add/remove is supported online
via `ClusterAddNodes`/`ClusterRemoveNodes` (cordon + drain + k0s reset).

### Operator mode (spur-k8s crate)

The `spur-k8s` crate provides a Kubernetes operator that watches Spur CRDs
and translates them into Spur API calls. It includes:

- **Job controller:** reconciles SpurJob CRDs to job submissions.
- **Node watcher:** syncs K8s node status with Spur node state.
- **Quota controller:** projects Spur accounts into K8s quota/RBAC.
- **Health:** the operator's HTTP health server (readiness/liveness).
- **Virtual agents:** K8s nodes appear as Spur compute nodes without
  running spurd, using the operator as the intermediary.

## 10. Networking: WireGuard mesh overlay

When `[network].wg_enabled = true`, Spur builds a WireGuard overlay
across all nodes. The default interface is `spur0`. Without k0s the
topology is hub-and-spoke (`spur net join` adds a node-to-controller
tunnel). A full mesh is established only when `[cluster].enabled = true`
and the k0s reconciler pushes `ApplyMesh` with the complete peer set.

**Setup flow:**

1. `spur net init` generates a WireGuard keypair and config on the
   controller.
2. `spur net join` or agent registration with `--token` adds a node to
   the mesh.
3. When `[cluster].enabled`, the k0s reconciler tick pushes the
   authoritative peer set to all nodes via the `ApplyMesh` RPC,
   upgrading the topology to a full mesh.

**Mesh properties:**

- Hub-and-spoke by default; full mesh when k0s `ApplyMesh` is active.
- Mesh IPs allocated from `[network].wg_cidr`.
- spurd auto-detects its address from the WireGuard interface if
  `--address` is not set.
- When running Calico CNI in bird mode, pod traffic routes natively over
  the mesh (no double-encapsulation).

The `spur-net` crate handles address detection, WireGuard config
management, OCI image pulling, and comm address normalization.

## 11. Crate dependency graph

```text
spur-proto          (protobuf codegen: slurm.proto + raft_internal.proto)
    |
    v
spur-core           (domain types: Job, Node, Partition, WAL, config, topology)
    |               depends on: spur-proto, spur-devices
    v
spur-sched          (backfill scheduler, cons_tres allocation, node matching)
    |               depends on: spur-core
    v
spur-metrics        (OpenMetrics exposition for jobs, nodes, partitions)
    |               depends on: spur-core
    |
spur-net            (WireGuard, address detection, OCI, mesh management)
    |               no internal deps
    |
spur-client         (gRPC channel helpers, timeouts)
    |               no internal deps
    |
spur-devices        (CDI cache, GRES discovery, GPU device registry)
    |               no internal deps
    |
spur-mpi-pmix       (PMIx wire protocol for multi-node MPI)
    |               no internal deps
    |
spur-spank          (SPANK plugin compatibility layer)
    |               no internal deps
    |
spur-update         (background update checker, binary self-update)
    |               no internal deps
    |
    +-------+-------+-------+-------+
    |       |       |       |       |
    v       v       v       v       v

spurctld            (controller daemon)
    depends on: spur-core, spur-proto, spur-sched, spur-net,
                spur-metrics, spur-update
    key 3rd-party: openraft, tonic, axum, tokio, sqlx (PostgreSQL)

spurd               (node agent daemon + library crate)
    depends on: spur-core, spur-proto, spur-client, spur-net,
                spur-sched, spur-devices, spur-spank, spur-update
    key 3rd-party: tonic, tokio, nix

spur-stepd          (per-job supervisor binary)
    depends on: spurd (lib crate), spur-core
    runtime isolation only (shares spurd's link surface)

spur-cli            (multi-call CLI)
    depends on: spur-core, spur-proto, spur-client, spur-net, spur-update
    key 3rd-party: clap, tonic

spur-ffi            (C FFI / libspur_compat.so)
    depends on: spur-proto, spur-client

spur-k8s            (K8s operator + CRDs)
    depends on: spur-core, spur-proto
    key 3rd-party: kube-rs

spurauthd           (auth daemon)
    depends on: spur-core

spur-tests          (integration test harness)
    depends on: spur-core, spur-proto, spur-sched, spur-spank
```

### Key architectural boundaries

- **spur-core** is the shared domain model. Every crate that deals with
  jobs, nodes, or config depends on it.
- **spur-proto** owns the public API surface (`slurm.proto`). FFI and REST
  depend on its stability. `raft_internal.proto` is separate, internal-only.
- **spurstepd** depends on spurd's lib crate at link time but enforces
  runtime isolation: it never executes spurd's network-facing RPC or
  cluster-admin code paths.
- **spur-sched** is used by both spurctld (scheduling decisions) and spurd
  (node allocation tracking via `cons_tres::NodeAllocation`).

### Configuration

All configuration is TOML at `/etc/spur/spur.conf`, parsed by
`spur-core/src/config.rs`. Runtime config changes via `scontrol reconfigure`
re-read the file on the leader. Some scheduler parameters (tick interval,
max jobs per cycle) are baked at startup and require a controller restart.
Topology is rebuilt each scheduling cycle from the current node set.
