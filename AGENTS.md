# Spur

Spur is an AI-native job scheduler written in Rust. Drop-in compatible with Slurm's CLI, REST API, and C FFI while providing WireGuard mesh networking, GPU-first scheduling, and modern state management.

## Build

```bash
# Prerequisites: protobuf-compiler (Rust toolchain is pinned in rust-toolchain.toml)
sudo apt install protobuf-compiler
cargo build
```

## Architecture

- **spurctld**: controller. Serves `SlurmController` + `SlurmAccounting` gRPC on port 6817. Accounting runs in-process on PostgreSQL (`accounting.database_url`). HA via openraft, always on (single-node runs a 1-member Raft cluster); the leader handles writes, non-leaders forward.
- **spurd**: node agent on each compute node. Registers, heartbeats, and receives launch/cancel via `SlurmAgent` gRPC on port 6818 by default.
- **spurstepd** (`crates/spur-stepd`): per-job (and per numbered `srun` step) supervisor spawned by `spurd`; survives `spurd` restart/upgrade. A separate binary, not a subcommand, so the process beside a job never runs `spurd`'s network-facing RPC or k0s admin code (different trust boundary). It still links the whole `spurd` lib crate, so this is runtime isolation, not link-time. Talks to `spurd` only over a local, versioned Unix-socket protocol (`spurd::stepd`), never the network.
- **spur-cli**: multi-call binary (`spur <command>` or Slurm symlinks like `sbatch`, `squeue`). Talks to `spurctld` (6817) for scheduling/admin/accounting and to `spurd` (`SlurmAgent`, 6818) for `srun` step I/O and `sattach`.
- **spurauthd**: native credential mint, only when `[auth] plugin = "spur"`. Runs on every login and compute node on a local Unix socket, not the network. CLI and `spurd` request a short-lived credential before each RPC; caller identity comes from `SO_PEERCRED`. `spurctld` and `spurd` verify credentials but do not mint them.
- **spur-k8s-operator** (`crates/spur-k8s`): watches `SpurJob` CRDs and submits them to `spurctld`, registers K8s nodes, and serves a virtual-agent gRPC surface (6818) that launches jobs as pods.
- **No separate accounting or REST daemons** (Slurm's `slurmdbd`/`slurmrestd`): `spurctld` serves accounting and the Axum REST API directly, avoiding another network-facing daemon and inter-daemon networking. `spurstepd` is separate for a different reason: privilege/fault isolation of a local per-job supervisor.
- **Proto**: `proto/slurm.proto` is the public API surface FFI and REST depend on. `proto/raft_internal.proto` is internal controller-to-controller plumbing only.
- **Config**: TOML at `/etc/spur/spur.conf`; all fields in `crates/spur-core/src/config.rs`.

Each crate has an `AGENTS.md` with crate-local rules. Read it before editing that crate.

## Conventions

- Proto conversion helpers live in the same file as the gRPC server (e.g. `server.rs` has `proto_to_job_spec`, `job_to_proto`).
- When designing, implementing, or updating RPC calls, avoid unbounded requests: set a clear timeout unless there is a specific, justified reason not to.
- Write idiomatic Rust. Keep control flow flat: early returns and `?` instead of deep nesting. Prefer small, focused, independently testable functions.
- Slurm compatibility is a migration bridge, not a design constraint. Only user-facing surfaces (CLI, REST API, FFI) need to stay compatible. Internals should prefer simple, modern defaults; do not inherit legacy complexity.
- User-facing changes ship with docs. When you change CLI flags, REST/gRPC API surface, config fields, or user-visible behavior, update the pages in `docs/` that describe it. A new feature must always be documented; consider whether it warrants its own page or subsection rather than a passing mention.

## Knowledge Base

The knowledge base is `docs/knowledge/<area>/*.md` (cross-crate) plus each crate's `AGENTS.md` (crate-local). It is for agents and maintainers; user-facing docs live in `docs/`.

- **What belongs**: invariants, pitfalls, cross-file/cross-crate coupling ("change X, also change Y"), cross-component flows where ordering matters, design rationale not visible in code, and non-default test requirements. Not restatements of code: no module trees, API/type/RPC tables, enum variant lists, constant values, counts, or dependency lists. Not copies of user docs: point to the `docs/` page instead.
- **When**: update in the same PR when you change an invariant, protocol, persisted format, state machine, or architectural boundary the KB describes.
- **Where**: crate-local facts in that crate's `AGENTS.md`, cross-crate facts in the matching `docs/knowledge/<area>/` file. State each fact once and link to it elsewhere.
- **How**: edit in place, present tense. Name symbols (e.g. `RAFT_MAX_MESSAGE_SIZE`) so drift is greppable. Delete stale claims; no "update:"/"previously" notes, history, or issue/PR numbers.
- **Before opening a PR**: grep the KB for the symbols you changed and run `scripts/check-knowledge-sync.sh`.
- **New area**: add any new `docs/knowledge/<area>/` directory to the directory list in `scripts/check-kb-accuracy.sh`, or the KB Accuracy check will not read it.
- CI (`scripts/check-knowledge-sync.sh`, and the KB Accuracy check on PRs labeled `kb-check`) only detects drift; it does not fix it.

## Git Workflow

PRs are squash-merged, so the **PR title is the final commit message**. Use conventional commit format for PR titles: `<type>(<scope>): <message>`

- **type**: `fix`, `feat`, `refactor`, `test`, `docs`, `chore`, `perf`, `ci`, `build`, `style`, `revert`
- **scope**: crate name (e.g. `spur-cli`, `spurctld`, `spur-core`). If no single crate applies, use a concise area scope (e.g. `proto`, `deploy`, `config`).
- **message**: imperative mood, lowercase, no trailing period.

Only `feat`, `fix`, and `perf` appear in the release changelog. Non-user-facing work (CI, tooling, infra) must use `ci`, `build`, or `chore`. CI enforces this: `fix(ci): ...` is rejected, use `ci: ...`.

PR descriptions should be concise, not line-by-line changelogs. Cover: what this fixes/adds, the approach, important design choices or trade-offs, planned follow-ups (if any), and how it was tested. Keep individual commits meaningful for reviewers.

When filing issues, focus on the problem: what happened, what was expected, and how to reproduce. Do not prescribe a fix; that biases whoever addresses it.

Do not use `#N` to reference numbered points within a PR or issue description (GitHub links `#1`, `#2` to issues/PRs). Use `[N]` instead (e.g. "as noted in [1] above").

A pre-commit hook is available in `.githooks/` (activate with `git config core.hooksPath .githooks`). It enforces formatting and SPDX license headers.

## Breaking Changes

Some changes compile and pass tests but break a running cluster on upgrade or break existing clients. Code review MUST flag these. Prefer the compatible option (add, don't remove/rename/retype); when a break is unavoidable, append `!` to the PR title type/scope (e.g. `fix(spur-core)!: ...`, `feat(proto)!: ...`). Watch for:

- **Persisted state (Raft/WAL log, snapshots)**: `WalOperation` and every type it reaches (`JobSpec`, `JobStep`, `Reservation`, `NodeState`, resource/alloc types, `AdmissionToken`, k0s state) serialize to JSON in the Raft log; `PersistedSnapshot` likewise. A new field without `#[serde(default)]` or `Option`, or a renamed/removed field or enum variant, makes a new controller crash replaying old entries.
- **gRPC proto (`proto/slurm.proto`)**: renumbering, removing, or retyping a field breaks FFI, REST, and cross-version wire compat. Only append fields with new tags; never reuse or renumber tags.
- **Config (`/etc/spur/spur.conf`, `config.rs`)**: removing/renaming a field or changing its type breaks deployed configs.
- **User-facing CLI/REST**: removing a flag, subcommand, or response field, or changing its meaning, breaks Slurm-compatible scripts.

Validate your changes before submitting:

```bash
cargo clippy --workspace --exclude spur-ffi --all-targets --locked  # no warnings
cargo test --locked                                                 # all tests pass, no external services needed
cargo shear --locked --deny-warnings                                # no unused dependencies (cargo +stable install --locked cargo-shear@1.14.0)
scripts/check-knowledge-sync.sh                                     # knowledge base docs match source
```

## Do Not

- **IMPORTANT**: If you encounter a security issue while working (hardcoded secrets, open permissions, unsafe patterns), always report it to the user even if it is unrelated to the current task. Do not attempt to fix it silently.
- **IMPORTANT**: Do not add comments that explain *what* the code does. Comments are only for *why*: non-obvious intent, trade-offs, or constraints the code cannot convey. Self-explanatory code gets no comments.
- **IMPORTANT**: Do not reference issue numbers, PR numbers, or task IDs in code comments. That context belongs in git history.
- Do not add comments that narrate the intent of a fix or review feedback (e.g. "changed per review", "moved here to fix X"). Review context belongs in the commit message or PR discussion.
- Keep comments short; 1-3 lines is the norm. Do not write long multi-line comment blocks unless it is really a must. If an explanation needs a full paragraph, it usually belongs in the commit message or PR description.
- **IMPORTANT**: Do not apply band-aid fixes. If a fix would be cleaner with a broader refactor that improves testability or idiomaticness, do the refactor and explain the reasoning to the user. Maintainability over fast delivery.
- Do not write tests that simulate a unit's behavior instead of calling the actual unit. Tests must exercise real code.
- Do not write tests that depend on the test runner's environment. Use explicit fixtures so tests pass identically everywhere.
- Do not add timeouts to tests. Tests should be fast and deterministic.
- Do not add external service dependencies to unit tests.
- Do not change the proto package name (`package slurm;`); FFI and REST compatibility depend on it.
- Do not use `unwrap()` in library code. Use `?` or explicit error handling.
- Do not commit files that may contain secrets.

## Where to look

- Job / node state machines: `crates/spur-core/src/job.rs`, `crates/spur-core/src/node.rs`
- Raft/WAL operations (`WalOperation`): `crates/spur-core/src/wal.rs`
- Scheduling algorithm: `crates/spur-sched/src/backfill.rs`
- Slurm migration guide: `docs/migration-from-slurm.rst`; deployment: `docs/deployment/`, `examples/k8s/`
- E2E tests against real clusters (Python): `tests/native_host/e2e/`, `tests/k8s/e2e/`; in-process Rust tests: `crates/spur-tests/src/`
- User docs: `README.md`, `docs/`
