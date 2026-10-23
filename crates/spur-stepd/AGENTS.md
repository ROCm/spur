# AGENTS.md -- spur-stepd

Per-job supervisor binary. Spur's equivalent of Slurm's `slurmstepd`.

## Overview

`spurstepd` runs for the entire lifetime of a single job step, independent of
`spurd` (the node daemon). spurd double-forks spurstepd so it reparents to init;
a spurd restart does not kill the job. The supervisor owns the step's process
tree, cgroup, container rootfs, and PTY custody. It records durable obligations
to disk so completion reporting survives crashes on either side.

The binary is a thin wrapper (`src/main.rs`, 35 lines). All logic lives in
`spurd::stepd::run_process`, reused as a library from the `spurd` crate.

## Directory structure

```
crates/spur-stepd/
  Cargo.toml              # depends on spurd (library), spur-core, tokio
  src/main.rs             # entry point: tracing init -> spurd::stepd::run_process
  tests/version_output.rs # -V/--version prints before arg validation
```

The real implementation is `crates/spurd/src/stepd.rs` (~6150 lines), which
`spur-stepd` links via `spurd = { path = "../spurd" }`. This pulls in
`executor`, `container`, and `mpi_plugin` as a library without invoking spurd's
controller-RPC, k0s, or mesh-networking code.

## Invocation

spurd spawns the supervisor via double-fork+setsid (see `spawn_stepd_process`
in `agent_server.rs`):

```
spurstepd <state-dir> <job-id> <attempt> <launch-spec-path>
```

- `state-dir`: root of spurd's local state (e.g. `/run/spur`)
- `job-id`: numeric job ID
- `attempt`: run attempt number (for retries)
- `launch-spec-path`: path to a JSON file containing `StepdLaunchSpec`

The grandchild reports its real pid over a pipe before exec. FD_CLOEXEC on the
pipe write end means a successful exec sends EOF (no error byte); a failed exec
writes `[1]` before `_exit(127)`.

## Unix-socket protocol with spurd

### Session directory

Each supervisor creates `<state-dir>/runtime/<job>.<attempt>.<step>/` containing:

| File | Purpose |
|---|---|
| `descriptor.json` | Identity: job/step/attempt, supervisor pid+start_ticks, socket path, cgroup, namespace flags, workload pid, output paths |
| `runtime.sock` | Control socket (JSON-over-Unix-stream) |
| `ptyfd.sock` | PTY custody socket (SCM_RIGHTS fd-passing) |
| `obligations.jsonl` | Durable completion state machine |
| `failure.txt` | Why a launch never produced a workload |

All files are mode 0600. Directories are mode 0700 with ownership verification.

### Control protocol (runtime.sock)

Newline-delimited JSON. Each connection begins with a `Hello` handshake,
then request/response pairs:

```
Request (spurd -> stepd):
  Hello { protocol_version, capability, spurd_instance_id, run_attempt, step_id }
  QueryState
  Start
  SignalAllocation { signal }
  BeginTeardown
  Shutdown

Response (stepd -> spurd):
  Hello { protocol_version, job_id, run_attempt, step_id }
  State { job_id, run_attempt, step_id, active, exit_code, signal, job_pid, cgroup_path }
  Acknowledged
  Rejected { message }
```

The `Hello` handshake validates a random capability token (constant-time
comparison via `subtle::ConstantTimeEq`) plus protocol version, run attempt,
and step ID. Connections are bounded: 5s handshake, 60s idle, 10s per
request/response exchange.

### Agent notification socket (agent.sock)

Lives at `<state-dir>/runtime/agent.sock`. When the supervisor completes, it
pushes `StepdCompleted` to spurd over this socket. Retries 5 times with 1s gaps
and 5s per-attempt timeout. If spurd is unreachable, the durable obligation
record stays for spurd's next-startup recovery scan.

### PTY custody socket (ptyfd.sock)

Binary protocol using `sendmsg`/`recvmsg` with `SCM_RIGHTS` for fd-passing.
Wire format: 1-byte opcode + 4-byte little-endian session ID.

| Opcode | Meaning |
|---|---|
| `D` (0x44) | Deposit: hold a dup of this shell's pty master |
| `R` (0x52) | Reclaim: return a specific session's master |
| `A` (0x41) | Reclaim-any: return any orphaned master (post-restart recovery) |
| `X` (0x58) | Release: terminal closed, drop the held fd |
| `O` (0x4F) | Reply: found (carries the fd) |
| `N` (0x4E) | Reply: absent |

## Process supervision

### Start gate

Before launching the workload, the supervisor waits on the control socket for
spurd to send `Start` (or `BeginTeardown`/`Shutdown` to cancel). This ensures
spurd's own bookkeeping completes before the job script can run (e.g., so an
`srun` on the first line cannot outrace the job's registration). Timeout: 120s.

The gate uses a shared `Mutex<Option<GateOutcome>>` with a settle window (20ms)
so near-simultaneous `Start` and `Shutdown` both get processed. `Cancelled`
always wins over `Released`.

### Workload launch

After the gate opens:

1. Start supervised PMIx server if `spec.pmix` is present (multi-rank MPI)
2. Call `executor::launch_job` which forks the workload, sets up namespaces
   (PID/user/mount as configured), cgroups, container rootfs, I/O streams
3. Record workload pid and start ticks in the descriptor
4. Self-deposit the PTY master if the launch used PTY I/O mode
5. Enter the supervisor loop

### Supervisor loop (`run_supervisor`)

Biased `tokio::select!`:
- Accept new control connections (priority: a job can exit before the agent
  connects, but the handoff must still complete)
- Poll completion every 100ms via `try_wait` on the child process

For allocation-only steps (`salloc`), there is no child process; teardown
itself is the completion signal.

### Signal delivery

Signals target the cgroup first (`cgroup.procs` enumeration), falling back to
process-group kill when the cgroup is empty, unreadable, or absent. SIGKILL
additionally writes `1` to `cgroup.kill` for atomic kernel-level termination
that catches races cgroup.procs enumeration cannot.

## Step lifecycle

### Teardown sequence

After the workload exits:

1. Read `memory.events` for OOM detection (sets `OOM_SIGNAL_FLAG`)
2. Record `ExitObserved` obligation (before any cleanup)
3. Run `TaskEpilog` hook as the job user inside the step cgroup
4. Stop PMIx server if running
5. Clean up cgroup (step cgroup, then job cgroup if this step owns the lifetime)
6. Clean up container rootfs (unmount overlay before removal)
7. Clean up job spool directory
8. Run `EpilogSlurmd` hook
9. Run SPANK `TaskExit` and `JobEpilog` hooks
10. Record `EpilogCompleted` obligation
11. Notify spurd via `agent.sock`
12. Record `CompletionAcknowledged` obligation if spurd acks

### Obligation state machine

Obligations are append-only in `obligations.jsonl`. A session is finalized when
both `CompletionAcknowledged` (after the last `ExitObserved`) and
`ResourcesReleased` have been recorded:

```
ExitObserved { exit_code, signal }     -- workload exited
EpilogCompleted { failed }             -- hooks ran
CompletionAcknowledged                 -- spurd forwarded to controller
ResourcesReleased                      -- agent released the allocation
```

Crash-safety: a partial final line (no trailing newline) is dropped on read.
A supervisor that dies before recording `ExitObserved` gets a synthetic
`SIGKILL` exit invented by `record_lost_supervisor_exit` so the completion
replay can still report it.

### Liveness detection

`stepd_liveness` checks `/proc/<pid>/stat`:
- Start ticks must match the recorded `process_start_ticks` (reuse detection)
- Process state must not be `Z` (zombie exclusion)

On spurd restart, `discover_live` walks session directories and classifies
each supervisor as `Live` or `Stale`.

### Session pruning

`prune_finalized` sweeps session directories:
- Finalized (acknowledged + resources released): removed immediately
- User step answers: retained for 600s so `await_step` callers can read the
  exit, capped at 1024 entries
- Abandoned (no obligation activity for 24h, supervisor stale): removed
- Unreadable descriptors with no recent activity: removed

## Signal handling

Signals from spurd arrive as `SignalAllocation { signal }` requests on the
control socket. The supervisor translates them to the workload:

1. Try cgroup-wide signal via `cgroup.procs` enumeration (reaches descendants
   outside the process group)
2. If cgroup signal reached nobody: fall back to process-group `kill`
3. On SIGKILL: additionally write `1` to `cgroup.kill` for atomic kernel-level
   kill (catches pid-reuse races)

`BeginTeardown` and `Shutdown` send SIGTERM via the same cgroup-first path.

## I/O management

- stderr is inherited from the double-fork (lands in the spurd agent log)
- Job stdout/stderr paths come from `StepdLaunchSpec.stdout_path` and
  `stderr_path`, resolved at launch and recorded in the descriptor
- PTY mode (`LaunchIo::Pty`): the pty master fd is deposited with the custody
  socket so the terminal survives agent restarts; a restarted agent reclaims
  orphaned masters via `CUSTODY_RECLAIM_ANY`
- Launch failures are appended to the job's stderr file (if it exists) and
  recorded in `failure.txt`

## Key constants

| Constant | Value | Purpose |
|---|---|---|
| `PROTOCOL_VERSION` | 1 | Control protocol version |
| `FORMAT_VERSION` | 1 | Descriptor format version |
| `CONTROL_LINE_LIMIT` | 64 KiB | Max JSON line on the control socket |
| `START_GATE_TIMEOUT` | 120s | Max wait for spurd to send Start |
| `STEPD_HANDSHAKE_TIMEOUT` | 5s | Hello exchange timeout |
| `CONTROL_REQUEST_IDLE_TIMEOUT` | 60s | Idle connection reap |
| `STEPD_REQUEST_TIMEOUT` | 10s | Full request/response bound |
| `AGENT_NOTIFY_ATTEMPTS` | 5 | Completion push retries |
| `SETTLED_RETENTION` | 600s | How long to keep answered user steps |
| `SETTLED_CAPACITY` | 1024 | Max retained step answers |
| `ORPHAN_RETENTION` | 24h | Abandoned session reap threshold |

## Build

```bash
cargo build -p spur-stepd          # produces target/debug/spurstepd
cargo test -p spur-stepd           # version flag tests only
cargo test -p spurd -- stepd       # runs stepd unit tests in spurd
```

The binary must be installed alongside `spurd`; `resolve_stepd_executable`
looks for it in the same directory as the running `spurd` binary, falling back
to bare `spurstepd` (no `$PATH` search, `execv` CWD-relative only).
