# Spur

Spur is an AI-native job scheduler written in Rust. It is drop-in compatible with Slurm's CLI, REST API, and C FFI while providing WireGuard mesh networking, GPU-first scheduling, and modern state management.

## Build

```bash
# Prerequisites: Rust 1.75+, protobuf-compiler
sudo apt install protobuf-compiler   # or: sudo dnf install protobuf-compiler
cargo build
```

Build takes ~30s on a modern machine. All crates build in one `cargo build`.

## Test

```bash
cargo test
```

All tests must pass. No external services needed (no database, no network, no GPU). Tests are self-contained.

## Project Layout

```
proto/slurm.proto              # Public API: Slurm-compatible gRPC service definitions
proto/raft_internal.proto      # Internal: Raft consensus RPCs between controllers
crates/
  spur-proto/                  # Generated gRPC code (build.rs runs tonic-build)
  spur-core/                   # Core types: Job, Node, ResourceSet, config, hostlist, WalOperation, partition, qos, step, topology, reservation, dependency, array, auth, accounting
  spur-net/                    # WireGuard mesh networking, IP pool, address detection
  spur-sched/                  # Backfill scheduler
  spurctld/                    # Controller daemon (the brain)
  spurd/                       # Node agent daemon (runs on compute nodes)
  spurdbd/                     # Accounting daemon (PostgreSQL)
  spurrestd/                   # REST API daemon
  spur-cli/                    # Multi-call CLI binary (spur, sbatch, squeue, etc.)
  spur-ffi/                    # C FFI shim (libspur_compat.so)
  spur-spank/                  # SPANK plugin host
  spur-k8s/                    # K8s integration
  spur-tests/                  # Integration test suite
```

## Key Architecture Decisions

- **Proto files**: `proto/slurm.proto` defines the public API — `SlurmController` (port 6817), `SlurmAgent` (port 6818), `SlurmAccounting` (port 6819). `proto/raft_internal.proto` is separate because Raft consensus is internal controller-to-controller plumbing, not part of the Slurm-compatible API surface that FFI and REST depend on.
- **State**: Always-on Raft consensus (openraft) in `spurctld/src/raft.rs`. Even single-node deployments run a 1-member Raft cluster. The Raft log is the sole durable store; snapshots are JSON-serialized `ClusterSnapshot` blobs. Recovery happens via Raft log replay + snapshot restore.
- **Scheduler**: Backfill scheduler in `spur-sched`. Runs every N seconds, assigns pending jobs to idle/mixed nodes.
- **Job dispatch**: Controller dispatches `LaunchJobRequest` to ALL allocated nodes (not just the first). Each node gets `peer_nodes` list and `task_offset`.
- **Networking**: Agents auto-detect WireGuard interface IP and self-report it during registration. Controller prefers self-reported address over TCP remote_addr. `spur net` CLI manages WireGuard mesh setup.
- **CLI**: `spur-cli` is a multi-call binary. Invoked as `spur <command>` or via symlinks (`sbatch`, `squeue`, etc.) for Slurm compatibility.
- **Config**: TOML format at `/etc/spur/spur.conf`. See `spur-core/src/config.rs` for all fields.

## Common Development Tasks

### Adding a new gRPC RPC

1. Add the RPC to `proto/slurm.proto`
2. `cargo build` regenerates the proto code in `spur-proto`
3. Implement the server handler in `spurctld/src/server.rs` (controller) or `spurd/src/agent_server.rs` (agent)

### Adding a new CLI command

1. Create a new module in `crates/spur-cli/src/` (see `net.rs` as an example)
2. Add `mod yourcommand;` to `crates/spur-cli/src/main.rs`
3. Add symlink dispatch in the `match bin_name` block (for backward-compat invocation via argv[0])
4. Add native dispatch in the `match args[1].as_str()` block (for `spur <command>` invocation)

### Adding a new config section

1. Add the struct to `crates/spur-core/src/config.rs`
2. Add the field to `SlurmConfig` with `#[serde(default)]`
3. Add default values
4. Update `default_config()` in `crates/spurctld/src/main.rs`

### Adding a new crate

1. Create `crates/your-crate/` with `Cargo.toml` and `src/lib.rs`
2. Add to `members` list in workspace `Cargo.toml`
3. Use `version.workspace = true` and `edition.workspace = true`
4. Reference workspace deps: `tokio = { workspace = true }`

## Conventions

- Workspace dependencies are declared in the root `Cargo.toml` under `[workspace.dependencies]`. Crates reference them with `{ workspace = true }`.
- Error handling: `anyhow::Result` for application code, `thiserror` for library error types.
- Async runtime: tokio (full features).
- Logging: `tracing` crate with `tracing-subscriber`.
- Proto conversion: Each gRPC handler converts between proto types and core types. Conversion helpers live in the same file as the server (e.g., `server.rs` has `proto_to_job_spec`, `job_to_proto`, etc.).
- Node state machine: `Idle`/`Mixed`/`Allocated` based on resource usage; `Down`/`Drain`/`Draining`/`Error`/`Unknown`/`Suspended` are admin/system states that override.
- Job state machine: `Pending → Running → Completing → Completed/Failed/Cancelled/Timeout/NodeFail/Preempted`. Jobs can also be `Suspended`. See `spur-core/src/job.rs`.

## Environment Variables

| Variable | Used by | Description |
|----------|---------|-------------|
| `SPUR_CONTROLLER_ADDR` | CLI, spurd, spurrestd | Controller gRPC address (default: `http://localhost:6817`) |
| `SPUR_WG_INTERFACE` | spurd | WireGuard interface name for address detection (default: `spur0`) |
| `SPUR_PROLOG` | spurd | Script to run before each job |
| `SPUR_EPILOG` | spurd | Script to run after each job |

## Do Not

- Do not add timeouts to tests. Tests should be fast and deterministic.
- Do not add external service dependencies to unit tests (no database, no network).
- Do not change the proto package name (`package slurm;`) — FFI and REST compatibility depend on it.
- Do not use `unwrap()` in library code. Use `?` or explicit error handling.
