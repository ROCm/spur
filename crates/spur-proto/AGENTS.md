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
- **Agent registration:** RegisterAgent, Heartbeat, ReportStepdRecovery
- **Admission tokens:** CreateToken, ListTokens, RevokeToken
- **Reservations:** CreateReservation, UpdateReservation, DeleteReservation,
  ListReservations
- **Diagnostics:** Ping, GetJobMetrics, GetNodeMetrics, GetRpcStats,
  GetSchedStats, GetAssocMgrInfo, ResetDiagStats
- **K8s operator:** ReportJobStatus, ExecInJob
- **Native k0s cluster:** ClusterUp, ClusterDown, ClusterStatus,
  ClusterKubeconfig, ClusterAddNodes, ClusterRemoveNodes

### SlurmAgent (spurd)

Per-node daemon. Receives dispatched work from the controller.

- **Job execution:** LaunchJob, StartJob, CancelJob, SuspendJob
- **Step dispatch:** RunCommand, CancelStep, AwaitStep,
  RegisterJobAllocation
- **PMIx:** PreparePmix, ReleasePmix
- **Interactive:** InteractiveSession (bidirectional streaming),
  StreamJobOutput
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
- **QOS:** CreateQos, DeleteQos, ListQos
- **Fairshare:** GetFairshareFactors
- **Audit:** GetTransactions

### RaftInternal

Opaque byte-envelope transport for the embedded Raft consensus layer.
Three RPCs: AppendEntries, Vote, InstallSnapshot. Payload is serialized
raft-rs messages; spur-proto does not interpret them.

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

## gRPC size limits

Asymmetric, set by the helper constructors in `lib.rs`:

- **Request (inbound):** 8 MiB (`MAX_GRPC_REQUEST_SIZE`) -- sized to a 4 MiB
  `JobSpec` plus framing overhead.
- **Response (outbound):** 32 MiB (`MAX_GRPC_MESSAGE_SIZE`) -- `GetJobs` /
  `GetNodes` on large clusters exceed tonic's 4 MiB default.

Always use the provided constructors (`controller_client`, `controller_server`,
`agent_server`, `accounting_client`, `accounting_server`) instead of
constructing clients/servers directly; they wire both limits.

## Retry policy

`controller_rpc_retryable()` classifies status codes:

- **Retryable:** Unavailable, Internal, DeadlineExceeded, Unknown, Cancelled
  (covers Raft-leader bounces, transport errors, client timeouts).
- **Not retryable:** InvalidArgument, PermissionDenied, NotFound,
  FailedPrecondition, OK -- spec rejections would repeat forever.

## Wire compatibility with Slurm

SPUR is not wire-compatible with Slurm. The proto schema is SPUR's own; it
mirrors Slurm's *conceptual* model (jobs, nodes, partitions, accounts, QOS,
fairshare, reservations, job steps, array jobs, preemption) but the wire format
is protobuf/gRPC, not Slurm's RPC protocol. CLI commands (`sbatch`, `squeue`,
`sacctmgr`, etc.) are reimplemented in Rust against this schema.

Field numbering is append-only. Removing or renumbering a field is a breaking
change for Raft log replay and rolling upgrades. Use `reserved` to retire
fields.

## Code generation

Dependencies (all workspace-level):

- `tonic` + `tonic-prost` -- runtime gRPC + prost integration
- `prost` + `prost-types` -- protobuf runtime (referenced only by generated code)
- `tonic-prost-build` -- build-time compiler

`cargo-shear` cannot see the generated code in `OUT_DIR`, so `prost`,
`prost-types`, and `tonic-prost` are listed in `[package.metadata.cargo-shear] ignored`.

To regenerate after editing a `.proto`:

```bash
cargo build -p spur-proto
```

No separate codegen step; `build.rs` runs on every build when a tracked proto
changes.
