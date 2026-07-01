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
- **spur-cli** — Multi-call CLI binary. Talks to `spurctld` for scheduling, admin, and accounting (all on port 6817). Invoked as `spur <command>` (e.g. `spur submit`, `spur queue`) or via Slurm-compatible symlinks (`sbatch`, `squeue`, `sinfo`, etc.).
- **No separate daemons for accounting or REST.** Slurm splits these into `slurmdbd` and `slurmrestd`. In Spur, `spurctld` handles accounting (backed by PostgreSQL) and the REST API (Axum) directly. This keeps the distribution to three binaries (`spurctld`, `spurd`, `spur`) and eliminates inter-daemon networking.
- **Proto**: `proto/slurm.proto` is the public API surface that FFI and REST depend on. `raft_internal.proto` is separate — internal controller-to-controller plumbing only.
- **Config**: TOML format at `/etc/spur/spur.conf`. See `spur-core/src/config.rs` for all fields.

## Conventions

- Proto conversion helpers live in the same file as the gRPC server (e.g., `server.rs` has `proto_to_job_spec`, `job_to_proto`).
- Write idiomatic Rust. Keep control flow flat — use early returns and `?` instead of deep nesting. Prefer small, focused functions that are independently testable.
- Slurm compatibility is a migration bridge, not a design constraint. Only user-facing surfaces (CLI, REST API, FFI) need to stay compatible. Internals should prefer simple, modern defaults — do not inherit legacy complexity.

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

Validate your changes before submitting:

```bash
cargo clippy --workspace --exclude spur-ffi --all-targets --locked  # no warnings
cargo test --locked                                                 # all tests pass, no external services needed
```

## Do Not

- **IMPORTANT**: If you encounter a security issue while working (hardcoded secrets, open permissions, unsafe patterns), always report it to the user even if it is unrelated to the current task. Do not attempt to fix it silently.
- **IMPORTANT**: Do not add comments that explain *what* the code does. Comments are only for *why* — non-obvious intent, trade-offs, or constraints the code itself cannot convey. Self-explanatory code gets no comments.
- **IMPORTANT**: Do not reference issue numbers, PR numbers, or task IDs in code comments. That context belongs in git history, not in the source.
- Do not add comments that narrate the intent of a fix or review feedback (e.g. "changed per review", "moved here to fix X"). The code should stand on its own — review context belongs in the commit message or PR discussion.
- **IMPORTANT**: Do not apply band-aid fixes. If a fix would be cleaner with a broader refactor that improves testability or idiomaticness, do the refactor. Explain the reasoning to the user. Maintainability over fast delivery.
- Do not write tests that simulate a unit's behavior instead of calling the actual unit. Tests must exercise real code.
- Do not write tests that depend on the test runner's environment. Use explicit fixtures so tests pass identically everywhere.
- Do not add timeouts to tests. Tests should be fast and deterministic.
- Do not add external service dependencies to unit tests.
- Do not change the proto package name (`package slurm;`) — FFI and REST compatibility depend on it.
- Do not use `unwrap()` in library code. Use `?` or explicit error handling.
- Do not commit files that may contain secrets.

## Further Reading

See `README.md` and `docs/` for detailed documentation. End-to-end tests against real clusters live in `tests/native_host/e2e/` and `tests/k8s/e2e/`.
