# AGENTS.md -- spur-stepd

Thin binary over `spurd::stepd::run_process`; all logic lives in `crates/spurd/src/stepd.rs`.
Spawn, start gate, teardown order, completion reporting, PTY custody recovery (full opcode set: `CUSTODY_*` in `crates/spurd/src/stepd.rs`), and restart
recovery are described in `docs/knowledge/execution/job-execution.md`.

## Coupling

- The positional argv (`<state-dir> <job-id> <attempt> <launch-spec-path>`) is a contract
  between `spawn_stepd_process` (spurd `agent_server.rs`) and `run_process`. Change both.
- `descriptor.json` and `obligations.jsonl` are read by a restarted spurd for supervisors that
  outlived it. `FORMAT_VERSION` versions the descriptor and `PROTOCOL_VERSION` the control
  socket (`Hello` rejects a mismatch).
- `resolve_stepd_executable` looks beside the running `spurd`, then falls back to a bare
  `spurstepd` passed to `execv`: no `$PATH` search, so the fallback is CWD-relative.

## Invariants

- Security: the `Hello` capability token is compared in constant time (`subtle::ConstantTimeEq`).
  Session files are 0600 (they carry the job's environment); session dirs are 0700 with
  ownership verification.
- Every control connection is bounded (`STEPD_HANDSHAKE_TIMEOUT`, `CONTROL_REQUEST_IDLE_TIMEOUT`,
  `STEPD_REQUEST_TIMEOUT`, `CONTROL_LINE_LIMIT`).
- Start gate: `Cancelled` always wins over `Released`. A short settle window
  (`GATE_DECISION_SETTLE`) lets near-simultaneous `Start` and `Shutdown` both be processed.
- `run_supervisor` uses a biased `select!` with accept first: a job can exit before the agent's
  first connect, and the handoff must still complete.
- Allocation-only (`salloc`) steps have no child process; a requested teardown is itself the
  completion signal.
- Signals target the cgroup (`cgroup.procs`) first to reach descendants outside the process
  group, falling back to process-group kill when the cgroup reached nobody. SIGKILL also writes
  `cgroup.kill` to catch anything the `cgroup.procs` snapshot raced past. `Shutdown` goes
  through the same path via `begin_teardown`.

## Obligations and liveness

- A session is finalized only when `CompletionAcknowledged` follows the last `ExitObserved`
  and `ResourcesReleased` is recorded (`finalized_obligations`).
- A partial final line in `obligations.jsonl` (crash mid-append) is dropped on read; any other
  corruption fails.
- A supervisor that dies before recording `ExitObserved` gets a synthetic SIGKILL exit from
  `record_lost_supervisor_exit` so the normal completion replay reports it.
- `stepd_liveness` requires matching start ticks and a non-zombie process: a zombie keeps its
  start ticks until reaped.
- `prune_finalized` keeps answered user steps for `SETTLED_RETENTION` (capped at
  `SETTLED_CAPACITY`) so `await_step` callers can still read the exit.

## Testing

- The supervisor's unit tests live in spurd: `cargo test -p spurd -- stepd`. This crate only
  tests that `-V`/`--version` prints before arg validation.
