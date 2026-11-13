# Spur

Spur is an AI-native job scheduler written in Rust. Drop-in compatible with Slurm's CLI, REST API, and C FFI while providing WireGuard mesh networking, GPU-first scheduling, and modern state management.

## Build

```bash
# Prerequisites: protobuf-compiler (Rust toolchain is pinned in rust-toolchain.toml)
sudo apt install protobuf-compiler
cargo build
```

## Architecture

- **spurctld** — Controller daemon. Serves the gRPC API (`SlurmController` + `SlurmAccounting` on port 6817). Accounting runs in-process backed by PostgreSQL (`accounting.database_url`). Supports HA via Raft log replication (openraft, always-on — even single-node runs a 1-member Raft cluster). Leader handles writes; non-leaders forward automatically.
- **spurd** — Node agent daemon. Runs on each compute node. Registers with the controller, sends heartbeats, and receives job launch/cancel commands via gRPC (`SlurmAgent` on port 6818).
- **spurstepd** — Job supervisor, spawned by `spurd` for each launched job and for each numbered `srun` step inside one, surviving an `spurd` restart/upgrade independently. A genuinely separate binary from `spurd` (not a subcommand): it sits beside a job's execution for that job's whole lifetime, a different trust/threat boundary than `spurd`'s network-facing RPC and cluster-admin (k0s) surface, so its process never runs that code at all. It still depends on the `spurd` lib crate as a whole (the runtime-only surface — `executor`/`container`/`stepd` — isn't split into its own crate yet), so this is a runtime, not link-time, isolation boundary. Talks to `spurd` over a local, versioned Unix-socket protocol (see `spurd::stepd`), never over the network.
- **spur-cli** — Multi-call CLI binary. Talks to `spurctld` (port 6817) for scheduling, admin, and accounting, and to `spurd` (`SlurmAgent`, port 6818 by default) for `srun` step I/O and `sattach`. Invoked as `spur <command>` (e.g. `spur submit`, `spur queue`) or via Slurm-compatible symlinks (`sbatch`, `squeue`, `sinfo`, etc.).
- **spurauthd**: Native credential mint, needed only when `[auth] plugin = "spur"`. Runs on every login and compute node and listens on a local Unix socket, not the network. The CLI and `spurd` ask it for a short-lived credential before each RPC; it takes the caller's identity from `SO_PEERCRED`. `spurctld` and `spurd` verify these credentials but do not mint them.
- **spur-k8s-operator** (`crates/spur-k8s`): Kubernetes operator. Watches `SpurJob` CRDs and submits them to `spurctld`, registers K8s nodes with the controller, and serves a virtual-agent gRPC surface (port 6818) that launches jobs as pods.
- **No separate daemons for accounting or REST.** Slurm splits these into `slurmdbd` and `slurmrestd`. In Spur, `spurctld` handles accounting (backed by PostgreSQL) and the REST API (Axum) directly, avoiding an extra *network-facing* daemon and the inter-daemon networking that would come with one. `spurstepd` is a separate binary for a different reason (privilege/fault isolation for a local per-job supervisor, not a network service) — see the `spurstepd` entry above.
- **Proto**: `proto/slurm.proto` is the public API surface that FFI and REST depend on. `raft_internal.proto` is separate — internal controller-to-controller plumbing only.
- **Config**: TOML format at `/etc/spur/spur.conf`. See `crates/spur-core/src/config.rs` for all fields.

## Conventions

- Proto conversion helpers live in the same file as the gRPC server (e.g., `server.rs` has `proto_to_job_spec`, `job_to_proto`).
- When designing, implementing, or updating RPC calls, avoid unbounded requests: set a clear timeout unless there is a specific, justified reason not to.
- Write idiomatic Rust. Keep control flow flat — use early returns and `?` instead of deep nesting. Prefer small, focused functions that are independently testable.
- Slurm compatibility is a migration bridge, not a design constraint. Only user-facing surfaces (CLI, REST API, FFI) need to stay compatible. Internals should prefer simple, modern defaults — do not inherit legacy complexity.
- User-facing changes ship with docs. Whenever you change CLI flags, REST/gRPC API surface, config fields, or user-visible behavior, check `docs/` and update the pages that describe it; a new feature must always be documented — consider whether it warrants its own page or subsection rather than a passing mention.

## Git Workflow

PRs are squash-merged, so the **PR title is the final commit message**. Use conventional commit format for PR titles: `<type>(<scope>): <message>`

- **type**: `fix`, `feat`, `refactor`, `test`, `docs`, `chore`, `perf`, `ci`, `build`, `style`, `revert`
- **scope**: crate name (e.g. `spur-cli`, `spurctld`, `spur-core`). If no single crate applies, use a concise scope reflecting the area of change (e.g. `proto`, `deploy`, `config`).
- **message**: imperative mood, lowercase, no trailing period.

Only `feat`, `fix`, and `perf` appear in the release changelog. Non-user-facing work (CI, tooling, infra) must use `ci`, `build`, or `chore` so it stays out of release notes. CI enforces this: a PR titled `fix(ci): ...` will be rejected, use `ci: ...` instead.

PR descriptions should be concise and readable — not line-by-line changelogs. Cover: what this fixes/adds, the approach taken, important design choices or trade-offs, planned follow-ups (if any), and how it was tested. Keep individual commits meaningful for reviewers.

When filing issues, focus on the problem: what happened, what was expected, and how to reproduce. Do not prescribe a fix — that biases the person or agent addressing it.

Do not use `#N` to reference numbered points within a PR or issue description — GitHub interprets `#1`, `#2`, etc. as links to other issues/PRs. Use `[N]` instead (e.g. "as noted in [1] above").

A pre-commit hook is available in `.githooks/` (activate with `git config core.hooksPath .githooks`). It enforces formatting and SPDX license headers.

## Breaking Changes

Some changes compile and pass tests but break a running cluster on upgrade or break existing clients. Code review MUST flag these. Prefer the compatible option (add, don't remove/rename/retype); when a break is genuinely unavoidable, append `!` to the PR title type/scope (e.g. `fix(spur-core)!: ...`, `feat(proto)!: ...`) so it is called out. Watch for:

- **Persisted state (Raft/WAL log, snapshots)** — `WalOperation` and every type it reaches (`JobSpec`, `JobStep`, `Reservation`, `NodeState`, resource/alloc types, `AdmissionToken`, k0s state) serialize to JSON in the Raft log; `PersistedSnapshot` likewise. A new field without `#[serde(default)]` or `Option`, or a renamed/removed field or enum variant, makes a new controller crash replaying old entries.
- **gRPC proto (`proto/slurm.proto`)** — renumbering, removing, or retyping a field breaks FFI, REST, and cross-version wire compat. Only append fields with new tags; never reuse or renumber tags.
- **Config (`/etc/spur/spur.conf`, `config.rs`)** — removing/renaming a field or changing its type breaks deployed configs.
- **User-facing CLI/REST** — removing a flag, subcommand, or response field, or changing its meaning, breaks Slurm-compatible scripts.

Validate your changes before submitting:

```bash
cargo clippy --workspace --exclude spur-ffi --all-targets --locked  # no warnings
cargo test --locked                                                 # all tests pass, no external services needed
cargo shear --locked --deny-warnings                                # no unused dependencies (cargo +stable install --locked cargo-shear@1.14.0)
scripts/check-knowledge-sync.sh                                     # knowledge base docs match source
```

## Do Not

- **IMPORTANT**: If you encounter a security issue while working (hardcoded secrets, open permissions, unsafe patterns), always report it to the user even if it is unrelated to the current task. Do not attempt to fix it silently.
- **IMPORTANT**: Do not add comments that explain *what* the code does. Comments are only for *why* — non-obvious intent, trade-offs, or constraints the code itself cannot convey. Self-explanatory code gets no comments.
- **IMPORTANT**: Do not reference issue numbers, PR numbers, or task IDs in code comments. That context belongs in git history, not in the source.
- Do not add comments that narrate the intent of a fix or review feedback (e.g. "changed per review", "moved here to fix X"). The code should stand on its own — review context belongs in the commit message or PR discussion.
- Keep comments short — 1-3 lines is the norm. Do not write long, multi-line comment blocks unless it is really a must; if an explanation needs a full paragraph, it usually belongs in the commit message or PR description, not the source.
- **IMPORTANT**: Do not apply band-aid fixes. If a fix would be cleaner with a broader refactor that improves testability or idiomaticness, do the refactor. Explain the reasoning to the user. Maintainability over fast delivery.
- Do not write tests that simulate a unit's behavior instead of calling the actual unit. Tests must exercise real code.
- Do not write tests that depend on the test runner's environment. Use explicit fixtures so tests pass identically everywhere.
- Do not add timeouts to tests. Tests should be fast and deterministic.
- Do not add external service dependencies to unit tests.
- Do not change the proto package name (`package slurm;`) — FFI and REST compatibility depend on it.
- Do not use `unwrap()` in library code. Use `?` or explicit error handling.
- Do not commit files that may contain secrets.

## Per-Crate Knowledge

Each crate has an `AGENTS.md` with architecture, key types, state machines, testing patterns, and pitfalls. Load on demand — don't read all at once.

### Binary crates

| Crate | Purpose |
|---|---|
| [`crates/spurctld/`](crates/spurctld/AGENTS.md) | Controller daemon — Raft consensus, scheduler loop, cluster state, accounting, REST API, hooks, k8s |
| [`crates/spurd/`](crates/spurd/AGENTS.md) | Node agent — job execution, stepd spawning, heartbeat, containers, device management, k0s |
| [`crates/spur-stepd/`](crates/spur-stepd/AGENTS.md) | Job supervisor — per-job process isolation, Unix-socket protocol, PTY custody |
| [`crates/spur-cli/`](crates/spur-cli/AGENTS.md) | Multi-call CLI — Slurm-compatible symlinks (sbatch, squeue, sinfo, …), 25 dispatch entries |
| [`crates/spur-k8s/`](crates/spur-k8s/AGENTS.md) | K8s operator — SpurJob CRD, virtual agent, node watcher, account quotas |
| [`crates/spurauthd/`](crates/spurauthd/AGENTS.md) | Auth daemon — native credential minting (SPURTKN1 tokens) on Unix socket |

### Library crates

| Crate | Purpose |
|---|---|
| [`crates/spur-core/`](crates/spur-core/AGENTS.md) | **Shared types** — config, job/node/partition/reservation models, WAL ops, auth. Change-sensitive: WalOperation-reachable types are Raft-persisted |
| [`crates/spur-sched/`](crates/spur-sched/AGENTS.md) | Scheduler — backfill algorithm, GPU-first placement, resource timeline, cons_tres allocation |
| [`crates/spur-proto/`](crates/spur-proto/AGENTS.md) | Protobuf — 3 gRPC services (SlurmController, SlurmAccounting, SlurmAgent) + RaftInternal, wire compat rules |
| [`crates/spur-net/`](crates/spur-net/AGENTS.md) | Networking — WireGuard mesh, OCI image puller, address pool |
| [`crates/spur-devices/`](crates/spur-devices/AGENTS.md) | Device management — CDI/GRES GPU discovery, stable BDF IDs, injection plans |
| [`crates/spur-metrics/`](crates/spur-metrics/AGENTS.md) | Telemetry — OpenMetrics 1.0 exposition for all controller subsystems |
| [`crates/spur-client/`](crates/spur-client/AGENTS.md) | gRPC transport — channel with HA failover across controller endpoints |
| [`crates/spur-ffi/`](crates/spur-ffi/AGENTS.md) | C FFI — libspur_compat.so, Slurm-like function names over Spur-specific structs (not ABI-compatible with libslurm) |
| [`crates/spur-mpi-pmix/`](crates/spur-mpi-pmix/AGENTS.md) | MPI — PMIx server plugin, modex exchange, loaded at `--mpi=pmix` |
| [`crates/spur-spank/`](crates/spur-spank/AGENTS.md) | SPANK — dlopen plugin host for Slurm SPANK shared libraries |
| [`crates/spur-update/`](crates/spur-update/AGENTS.md) | Self-update — GitHub releases check, binary replacement (rename + copy, optional SHA256 verification) |
| [`crates/spur-tests/`](crates/spur-tests/AGENTS.md) | Test harness — Slurm-numbered in-process tests (t01–t60), fixture builders |

## Where to look

| Area | Start here |
|---|---|
| Entry point | `crates/spurctld/src/main.rs` (controller), `crates/spurd/src/main.rs` (agent) |
| Proto API surface | `proto/slurm.proto` (~2360 lines, 3 services) |
| Config format | `crates/spur-core/src/config.rs` (TOML, all sections) |
| Job state machine | `crates/spur-core/src/job.rs` (13 states, transition matrix) |
| Node state machine | `crates/spur-core/src/node.rs` (9 states, event-driven) |
| Scheduling algorithm | `crates/spur-sched/src/backfill.rs` |
| Raft/WAL operations | `crates/spur-core/src/wal.rs` (41 variants) |
| User-facing docs (Sphinx) | `docs/` |
| E2E tests (Python) | `tests/native_host/e2e/`, `tests/k8s/e2e/` |
| Unit/component tests (Rust) | `crates/spur-tests/src/` |
| Deployment | `docs/deployment/`, `examples/k8s/` |
| Slurm migration guide | `docs/migration-from-slurm.rst` |

## Further Reading

See `README.md` and `docs/` for detailed documentation. End-to-end tests against real clusters live in `tests/native_host/e2e/` and `tests/k8s/e2e/`.
