@AGENTS.md

## Per-Crate Knowledge

Each crate has an `AGENTS.md` with architecture, key types, state machines, testing patterns, and pitfalls. Load on demand — don't read all at once.

### Binary crates

| Crate | Purpose |
|---|---|
| [`crates/spurctld/`](crates/spurctld/AGENTS.md) | Controller daemon — Raft consensus, scheduler loop, cluster state, accounting, REST API, hooks, k8s |
| [`crates/spurd/`](crates/spurd/AGENTS.md) | Node agent — job execution, stepd spawning, heartbeat, containers, device management, k0s |
| [`crates/spur-stepd/`](crates/spur-stepd/AGENTS.md) | Job supervisor — per-job process isolation, Unix-socket protocol, PTY custody |
| [`crates/spur-cli/`](crates/spur-cli/AGENTS.md) | Multi-call CLI — Slurm-compatible symlinks (sbatch, squeue, sinfo, …), 44 commands |
| [`crates/spur-k8s/`](crates/spur-k8s/AGENTS.md) | K8s operator — SpurJob CRD, virtual agent, node watcher, account quotas |
| [`crates/spurauthd/`](crates/spurauthd/AGENTS.md) | Auth daemon — native credential minting (SPURTKN1 tokens) on Unix socket |

### Library crates

| Crate | Purpose |
|---|---|
| [`crates/spur-core/`](crates/spur-core/AGENTS.md) | **Shared types** — config, job/node/partition/reservation models, WAL ops, auth. Change-sensitive: all types are Raft-persisted |
| [`crates/spur-sched/`](crates/spur-sched/AGENTS.md) | Scheduler — backfill algorithm, GPU-first placement, resource timeline, cons_tres allocation |
| [`crates/spur-proto/`](crates/spur-proto/AGENTS.md) | Protobuf — 3 gRPC services (SlurmController, SlurmAccounting, SlurmAgent), wire compat rules |
| [`crates/spur-net/`](crates/spur-net/AGENTS.md) | Networking — WireGuard mesh, OCI image puller, address pool |
| [`crates/spur-devices/`](crates/spur-devices/AGENTS.md) | Device management — CDI/GRES GPU discovery, stable BDF IDs, injection plans |
| [`crates/spur-metrics/`](crates/spur-metrics/AGENTS.md) | Telemetry — OpenMetrics 1.0 exposition for all controller subsystems |
| [`crates/spur-client/`](crates/spur-client/AGENTS.md) | gRPC transport — channel with HA failover across controller endpoints |
| [`crates/spur-ffi/`](crates/spur-ffi/AGENTS.md) | C FFI — libslurm ABI compat shim for existing Slurm-linked tools |
| [`crates/spur-mpi-pmix/`](crates/spur-mpi-pmix/AGENTS.md) | MPI — PMIx server plugin, modex exchange, loaded at `--mpi=pmix` |
| [`crates/spur-spank/`](crates/spur-spank/AGENTS.md) | SPANK — dlopen plugin host for Slurm SPANK shared libraries |
| [`crates/spur-update/`](crates/spur-update/AGENTS.md) | Self-update — GitHub releases check, SHA256-verified atomic binary replacement |
| [`crates/spur-tests/`](crates/spur-tests/AGENTS.md) | Test harness — Slurm-numbered integration tests (t01–t60), fixture builders |

## Where to look

| Area | Start here |
|---|---|
| Entry point | `crates/spurctld/src/main.rs` (controller), `crates/spurd/src/main.rs` (agent) |
| Proto API surface | `proto/slurm.proto` (~2000 lines, 3 services) |
| Config format | `crates/spur-core/src/config.rs` (TOML, all sections) |
| Job state machine | `crates/spur-core/src/job.rs` (13 states, transition matrix) |
| Node state machine | `crates/spur-core/src/node.rs` (9 states, event-driven) |
| Scheduling algorithm | `crates/spur-sched/src/backfill.rs` |
| Raft/WAL operations | `crates/spur-core/src/wal.rs` (~40 variants) |
| User-facing docs (Sphinx) | `docs/` |
| E2E tests (Python) | `tests/native_host/e2e/`, `tests/k8s/e2e/` |
| Unit/component tests (Rust) | `crates/spur-tests/src/` |
| Deployment | `docs/deployment/`, `examples/k8s/` |
| Slurm migration guide | `docs/migration-from-slurm.rst` |
