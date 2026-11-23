# spur-proto

Generated Rust bindings for SPUR's protobuf service definitions. This crate is
the single source of truth for all gRPC types and service traits used across the
SPUR workspace.

## Proto sources

The `.proto` files live at the repo root, not inside this crate:

- `proto/slurm.proto` -- all Slurm-compatible services and messages
- `proto/raft_internal.proto` -- opaque Raft RPC envelope (AppendEntries, Vote, InstallSnapshot)

`build.rs` compiles both via `tonic-prost-build` with `--experimental_allow_proto3_optional`
and explicit `cargo:rerun-if-changed` tracking (the protos are outside the crate
directory, so cargo's default heuristic misses them).

## Modules

```
pub mod proto       -- tonic::include_proto!("slurm")
pub mod raft_proto  -- tonic::include_proto!("raft_internal")
```

`proto::*` is re-exported at the crate root for convenience.

## Services

### SlurmController (spurctld)

Central scheduler and cluster manager. Raft-replicated.

- **Job lifecycle:** SubmitJob, GetJobs, GetJob, CancelJob, CompleteJob,
  SuspendJob, ResumeJob, UpdateJob, RequeueJob, JobKeepalive
- **Node management:** GetNodes, GetNode, UpdateNode, DrainNode,
  DeregisterNode, DeregisterAgent
- **Partitions:** GetPartitions, CreatePartition, UpdatePartition, DeletePartition,
  Reconfigure
- **Job steps:** GetJobSteps, CreateJobStep, CompleteJobStep, RunStep
- **Agent registration:** RegisterAgent, Heartbeat, ReportStepdRecovery, ReportCheckpointTelemetry
- **Admission tokens:** CreateToken, ListTokens, RevokeToken
- **Reservations:** CreateReservation, UpdateReservation, DeleteReservation,
  ListReservations
- **Diagnostics:** Ping, GetJobMetrics, GetNodeMetrics, GetRpcStats,
  GetSchedStats, GetAssocMgrInfo, ResetDiagStats
- **K8s operator / agent:** ReportJobStatus (also called by spurd), ExecInJob
  (also serves `spur exec`)
- **Fabric:** GetFabricTopology, ValidateBlockFile
- **Native k0s cluster:** ClusterUp, ClusterDown, ClusterStatus,
  ClusterKubeconfig, ClusterAddNodes, ClusterRemoveNodes

### SlurmAgent (spurd)

Per-node daemon. Receives dispatched work from the controller.

- **Job execution:** LaunchJob, StartJob, CancelJob, SuspendJob
- **Step dispatch:** RunCommand, CancelStep, ClaimStep, AwaitStep,
  RegisterJobAllocation
- **PMIx:** PreparePmix, ReleasePmix
- **Interactive:** InteractiveSession (bidirectional streaming),
  StreamJobOutput
- **Signals:** SignalJob
- **Resource query:** GetNodeResources, ProbeStepd, Ping, ExecInJob
- **k0s component control:** StartClusterComponent, StopClusterComponent,
  GetClusterComponentStatus, CreateK0sJoinToken, DrainK8sNode,
  DeleteK8sNode, GetKubeconfig
- **WireGuard mesh:** ApplyMesh

### SlurmAccounting

Persistent job history and resource-management database.

- **Job records:** RecordJobStart, RecordJobEnd, GetJobHistory, GetUsage
- **Accounts:** CreateAccount, DeleteAccount, ListAccounts
- **Users:** AddUser, RemoveUser, ListUsers
- **Coordinators:** AddCoordinator, RemoveCoordinator, ListCoordinators
- **QOS:** CreateQos, DeleteQos, ListQos
- **Usage:** ModifyUsage
- **Fairshare:** GetFairshareFactors
- **Audit:** GetTransactions

### RaftInternal

Opaque byte-envelope transport for the embedded Raft consensus layer.
Three RPCs: AppendEntries, Vote, InstallSnapshot. Payload is serialized
openraft messages; spur-proto does not interpret them.

## Key message types

| Type | Role |
|---|---|
| `JobSpec` | Submission payload (resources, script, env, container, MPI, K8s pod opts) |
| `JobInfo` | Full job record returned by queries and accounting |
| `NodeInfo` | Node state, resources, topology, labels, features |
| `PartitionInfo` | Partition config and membership |
| `ResourceSet` | Advertised node capacity (CPUs, memory, GPUs with topology) |
| `ResourceAllocations` | What a job holds on a node |
| `GpuResource` | Per-device GPU identity: type, memory, peer links, stable BDF id |
| `GpuRequest` | GPU ask at job/node/task granularity |
| `PmixLaunchPlan` | Multi-node PMIx server bootstrap parameters |
| `ContainerSpec` | OCI container config for steps (image, mounts, env, entrypoint) |
| `MeshMembership` | Full-mesh WireGuard peer set pushed to agents |
| `InteractiveInput/Output` | Bidirectional PTY stream (init, stdin, resize, signal / data, exit) |
| `CoreLayout` | A node's spec/GPU-reserved/trimmed/general CPU split, agent-enforced |
| `SocketCoreThread` | Slurm's S:C:T for a node, from the agent's `CpuTopology`; `NodeInfo.socket_core_thread` (tag 67), absent when unknown |
| `GpuCpus` | One GPU's reserved picks and nearby CPUs as CPU-list strings (empty = none/unknown); `NodeInfo.gpu_cpus` (tag 68). `NodeInfo.capabilities` (tag 69) echoes the agent's registration capabilities |
| `StepResources` | A typed step's own CPU/memory/GPU ask on a node, carried on the `RunStep`/`RunCommand` request messages |
| `GpuBlockedCpus` | Per GPU, reserved CPUs a non-holder still holds under `core_layout_fingerprint`; carried on the `Heartbeat` RPC's `layout_blocked_cpus` field (tag 11, empty = none blocked, also what an older agent sends) |

## Capability negotiation

The `Ping` response's `capabilities` field lists optional controller
behaviours by name (e.g. `"step-resources"`) so a client can refuse a
request an older controller would otherwise silently weaken, instead of
failing later or running with less isolation than asked for. Append new
capability strings; never repurpose one already shipped.

## gRPC size limits

Asymmetric, set by the helper constructors in `lib.rs`:

- **Request (inbound):** 8 MiB (`MAX_GRPC_REQUEST_SIZE`) -- sized to a 4 MiB
  `JobSpec` plus framing overhead.
- **Response (outbound):** 32 MiB (`MAX_GRPC_MESSAGE_SIZE`) -- `GetJobs` /
  `GetNodes` on large clusters exceed tonic's 4 MiB default.

Use `controller_client`, `controller_server`, `agent_server`,
`accounting_client` and `accounting_server`; they wire both size limits.
There is no agent-client helper; spurctld builds `SlurmAgentClient`
directly with explicit limits.

## Retry policy

`controller_rpc_retryable()` classifies status codes:

- **Retryable:** Unavailable, Internal, DeadlineExceeded, Unknown, Cancelled
  (covers Raft-leader bounces, transport errors, client timeouts).
- **Not retryable:** InvalidArgument, PermissionDenied, NotFound,
  FailedPrecondition, OK -- spec rejections would repeat forever.

## Launch refusal trailer

An agent refusing `LaunchJob` with `ResourceExhausted` because a GPU's reserved
CPUs are held under an older layout sets the metadata `LAUNCH_REFUSAL_HEADER`
(`spur-launch-refusal`) to `REFUSAL_RESERVED_CPUS_HELD` (`reserved-cpus-held`);
`is_reserved_cpus_held_refusal()` reads it. A trailer, not a `LaunchFailureKind`,
so an older controller keeps its `ResourceExhausted` path (node cooldown).

## Wire compatibility with Slurm

SPUR is not wire-compatible with Slurm. The proto schema is SPUR's own; it
mirrors Slurm's *conceptual* model (jobs, nodes, partitions, accounts, QOS,
fairshare, reservations, job steps, array jobs, preemption) but the wire format
is protobuf/gRPC, not Slurm's RPC protocol. CLI commands (`sbatch`, `squeue`,
`sacctmgr`, etc.) are reimplemented in Rust against this schema.

Field numbering is append-only. Removing or renumbering a proto field breaks
mixed-version wire compatibility between CLI, agent and controller during
rolling upgrades. WAL replay compatibility is a separate serde concern on
spur-core types. Use `reserved` to retire fields.

## Code generation

Dependencies (all workspace-level):

- `tonic` + `tonic-prost` -- runtime gRPC + prost integration
- `prost` + `prost-types` -- protobuf runtime (referenced only by generated code)
- `tonic-prost-build` -- build-time compiler

`cargo-shear` cannot see the generated code in `OUT_DIR`, so `prost-types`
and `tonic-prost` are listed in `[package.metadata.cargo-shear] ignored`.

To regenerate after editing a `.proto`:

```bash
cargo build -p spur-proto
```

No separate codegen step; `build.rs` runs on every build when a tracked proto
changes.
