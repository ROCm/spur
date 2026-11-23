# AGENTS.md -- spur-proto

Generated bindings for `proto/slurm.proto` and `proto/raft_internal.proto`. The
protos live at the repo root, outside this crate, which is why `build.rs` tracks
them with explicit `cargo:rerun-if-changed`.

## Wire compatibility

- Spur is not wire-compatible with Slurm. The schema mirrors Slurm's concepts,
  but the wire is protobuf/gRPC, not Slurm's RPC protocol; Slurm compatibility
  lives only in the reimplemented CLI, REST, and FFI.
- Retire fields with `reserved` (append-only rules are in root AGENTS.md).
- Proto compat covers mixed-version CLI, agent, and controller during rolling
  upgrades. WAL/snapshot replay is a separate serde concern on spur-core types;
  a proto-safe change can still break it.

## Capability negotiation

- `PingResponse.capabilities` lists optional controller behaviours by name
  (e.g. `"step-resources"`) so a client can refuse a request an older
  controller would silently weaken, instead of running with less isolation
  than asked for.
- Append new capability strings; never repurpose one already shipped.

## gRPC size limits

- Limits are asymmetric: inbound requests capped at `MAX_GRPC_REQUEST_SIZE`
  (sized to the 4 MiB `JobSpec` cap plus framing), outbound responses up to
  `MAX_GRPC_MESSAGE_SIZE` (`GetJobs`/`GetNodes` on large clusters exceed
  tonic's 4 MiB default).
- Build clients and servers through the helpers in `lib.rs`
  (`controller_client`, `controller_server`, `agent_server`,
  `accounting_client`, `accounting_server`), which wire both limits. There is
  no agent-client helper: every `SlurmAgentClient` caller (spurctld
  `agent_client.rs`, spur-cli `interactive.rs`) must set both limits explicitly.

## Retry policy

- `controller_rpc_retryable()` retries only transient/transport codes (Raft
  leader bounces, client timeouts). Spec rejections (`InvalidArgument`,
  `PermissionDenied`, `NotFound`, `FailedPrecondition`) must stay
  non-retryable or they repeat forever.

## Launch refusal trailer

- A `ResourceExhausted` launch refusal over GPU reserved CPUs held under an
  older layout is marked with metadata `LAUNCH_REFUSAL_HEADER` =
  `REFUSAL_RESERVED_CPUS_HELD`; test it with `is_reserved_cpus_held_refusal()`.
- It is a trailer, not a `LaunchFailureKind` variant, so an older controller
  keeps its plain `ResourceExhausted` path (node cooldown).

## Build quirks

- `cargo-shear` cannot see generated code in `OUT_DIR`, so `prost-types` and
  `tonic-prost` are in `[package.metadata.cargo-shear] ignored`. Do not remove
  them as "unused".
