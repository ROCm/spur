# Job Suspend/Resume — Design (Slurm Parity, #270)

## Problem

Spur has no job suspend/resume. `scontrol suspend <id>` / `resume <id>` are
unrecognized subcommands, there are no `SuspendJob`/`ResumeJob` RPCs, and the
agent never issues SIGSTOP/SIGCONT. The `JOB_SUSPENDED` enum value and the
`Running↔Suspended` / `Suspended→Cancelled` state-machine edges already exist
(`spur-core/src/job.rs`), but nothing drives a job into or out of that state.

Slurm transitions `RUNNING → SUSPENDED → RUNNING` cleanly, freezing the process
(SIGSTOP) while **retaining** the node allocation, and excludes suspended time
from run-time accounting and wall-clock (time-limit) enforcement.

Verified against Slurm 25.11.6 on the testbed: plain `scontrol suspend` keeps
`StartTime` stable, keeps the allocation, and pushes the effective time-limit
deadline out by the suspended duration.

## Scope (decided)

- **Accounting:** Full parity. Track accumulated suspended duration; exclude it
  from `run_time()` *and* from time-limit enforcement so a suspended job does
  not time out while frozen and regains its full wall-clock budget on resume.
- **Authorization:** Match existing Spur convention. RPCs carry a `user` field
  for parity/forward-compat but add no new enforcement (consistent with how
  `cancel_job` currently treats `user` as advisory). Admin-only enforcement is
  a separate cross-cutting effort, out of scope.
- **Resources:** Retain allocation while suspended (plain `scontrol suspend`
  semantics). The scheduler does not hand a suspended job's nodes to other
  pending jobs; resume just SIGCONTs in place. Gang/preempt resource-release is
  explicitly out of scope.

## Non-goals

- Admin/privilege enforcement for suspend/resume.
- Releasing resources on suspend (gang scheduling / preemption flows).
- k8s backend suspend (controller-side machinery applies; agent signalling for
  the native executor only in this change).

## Architecture

State flip is recorded through Raft; the controller then dispatches the freeze/
thaw signal to all allocated agents. Suspended-time accounting lives in
`spur-core` so it is unit-testable without a cluster, and is driven by explicit
timestamps carried in dedicated WAL ops so replay and snapshot-restore are
deterministic.

### 1. Proto (`proto/slurm.proto`)

New controller RPCs and one agent RPC. Field numbers are per-message and do not
collide with PR #274 (which added `JobInfo` 33/34, `RunStepRequest` 7,
`ReportJobStatusRequest` 8 — all different messages).

```proto
service SlurmController {
  rpc SuspendJob(SuspendJobRequest) returns (google.protobuf.Empty);
  rpc ResumeJob(ResumeJobRequest) returns (google.protobuf.Empty);
}
message SuspendJobRequest { uint32 job_id = 1; string user = 2; }
message ResumeJobRequest  { uint32 job_id = 1; string user = 2; }

service SlurmAgent {
  rpc SuspendJob(AgentSuspendJobRequest) returns (google.protobuf.Empty);
}
// false => SIGSTOP (suspend), true => SIGCONT (resume)
message AgentSuspendJobRequest { uint32 job_id = 1; bool resume = 2; }
```

One agent RPC overloaded with a `resume` bool, mirroring how
`AgentCancelJobRequest` overloads `signal`.

### 2. Core (`spur-core/src/job.rs`)

Add two fields to `Job` (both `#[serde(default)]` for old-log/snapshot compat):

```rust
pub suspended_at: Option<DateTime<Utc>>,  // set while currently suspended
pub suspended_secs: i64,                  // accumulated total, seconds
```

- State-machine edges already permit `Running↔Suspended` and
  `Suspended→Cancelled` — no change to `transition()`'s match.
- `run_time()` subtracts `suspended_secs` plus any in-progress
  `(now - suspended_at)`.
- New `effective_deadline(start, time_limit) -> DateTime<Utc>` returns
  `start + time_limit + suspended_secs (+ in-progress suspension)`, used by the
  timeout enforcer. `StartTime` itself is never mutated (Slurm parity).

The accumulation itself is applied in the WAL apply path (below), not inside
`transition()`, so it is driven by logged timestamps rather than apply-time
clock.

### 3. WAL (`spur-core/src/wal.rs` + `spurctld/src/cluster.rs` apply)

Two new `WalOperation` variants carrying explicit, controller-stamped
timestamps (replay-deterministic by construction):

```rust
JobSuspend { job_id: JobId, at: DateTime<Utc> },
JobResume  { job_id: JobId, at: DateTime<Utc> },
```

Apply logic:

- `JobSuspend` → `job.transition(Suspended)`, set `job.suspended_at = Some(at)`.
- `JobResume` → if `suspended_at` is set, `job.suspended_secs += (at - suspended_at).num_seconds()`,
  clear `suspended_at`, then `job.transition(Running)`.

Per the repo WAL gotcha: every construction site must set the new fields
(serde default only covers deserialization, not construction); grep all
`WalOperation::` match arms and constructors.

### 4. Controller (`spurctld/src/server.rs` + `cluster.rs`)

- `server.rs`: `suspend_job` / `resume_job` handlers using the same
  leader-forward pattern as `cancel_job` (snapshot job for `allocated_nodes`,
  call cluster method, then `tokio::spawn` the agent dispatch).
- `cluster.rs`: `suspend_job(job_id, user)` validates `state == Running` (bail
  otherwise); `resume_job(job_id, user)` validates `state == Suspended`. Each
  proposes its WAL op with `at = Utc::now()`. **No deallocation** — unlike
  `cancel_job` which uses `JobComplete`.
- `scheduler_loop.rs`: `send_suspend_to_agents(cluster, job, resume: bool)`
  mirroring `send_cancel_to_agents` — fan out `AgentSuspendJobRequest` to every
  allocated node.

### 5. Timeout enforcer (`spurctld/src/scheduler_loop.rs`)

Replace the `start_time + time_limit` deadline (currently ~line 726) with
`job.effective_deadline(start_time, time_limit)`. Suspended jobs are already
excluded because the enforcer scans only `Running`/`Completing`; the change
ensures a *resumed* job's deadline reflects time spent frozen.

### 6. Agent (`spurd/src/agent_server.rs`)

New `suspend_job` RPC handler: look up the job in the `running` map and call the
existing `kill_signal(SIGSTOP)` (suspend) or `kill_signal(SIGCONT)` (resume).
`kill_signal` already handles both managed PIDs and container process-trees, so
no new signalling code is required.

### 7. CLI (`spur-cli/src/scontrol.rs`)

Add `Suspend { job_id }` / `Resume { job_id }` clap variants mirroring
`Hold`/`Release`, each invoking the new controller RPC. The `scontrol` symlink
already routes through this enum, so both `scontrol suspend <id>` and
`spur scontrol suspend <id>` work. `JobState=SUSPENDED` already renders in
`squeue` / `scontrol show job`.

## Data flow

```
spur scontrol suspend N
  → SlurmController.SuspendJob{N, user}
    → (leader) cluster.suspend_job: validate Running, propose JobSuspend{N, now}
       → Raft commit → apply: transition(Suspended), suspended_at = now
    → send_suspend_to_agents(resume=false)
       → SlurmAgent.SuspendJob{N, resume=false} on each allocated node
          → kill_signal(SIGSTOP)

spur scontrol resume N
  → SlurmController.ResumeJob{N, user}
    → cluster.resume_job: validate Suspended, propose JobResume{N, now}
       → apply: suspended_secs += now - suspended_at; clear; transition(Running)
    → send_suspend_to_agents(resume=true) → kill_signal(SIGCONT)
```

## Error handling

- Suspend on a non-Running job → `cluster.suspend_job` bails; handler returns
  `Status::failed_precondition`. Same for resume on a non-Suspended job.
- Job not found → `Status::not_found`.
- Agent dispatch failures are logged as warnings and skipped (same as
  `send_cancel_to_agents`); the state change is already durable via Raft, so a
  transient agent miss does not corrupt controller state. (A node that missed a
  SIGSTOP keeps running until the next signal; acceptable and matches the
  existing cancel-dispatch failure model.)

## Testing

No mocks of the unit under test; real spawned processes for signal tests; no
timeouts; no network/DB (per CLAUDE.md).

- **spur-core:** transition guards (reject suspend from Pending/terminal,
  reject resume from non-Suspended); `run_time()` excludes a suspended span;
  `effective_deadline` math with zero / one / multiple suspensions; serde
  round-trip of `JobSuspend`/`JobResume` WAL ops and the new `Job` fields.
- **spurctld:** `suspend_job`/`resume_job` accept on valid state, reject on
  invalid; deadline extension visible after resume; WAL apply accumulates
  `suspended_secs` from logged timestamps (replay determinism).
- **spurd:** SIGSTOP actually stops a real spawned process (observe
  `/proc/<pid>/stat` state `T`), SIGCONT resumes it (state back to `R`/`S`).
- **spur-tests:** end-to-end submit → suspend → resume → complete; assert
  `JobState` transitions and that run-time excludes the suspended window.

## Live verification (testbed)

Build release, deploy to 147 (per lab-testbed procedure), and diff Spur vs
Slurm 25.11.6 on 145:

1. `scontrol suspend N` → `JobState=SUSPENDED`, process state `T`, allocation
   retained, `StartTime` unchanged.
2. `scontrol resume N` → `JobState=RUNNING`, process state `R/S`.
3. Suspend spanning the time-limit window → job does **not** time out while
   suspended; after resume the deadline is pushed out by the suspended duration.
4. `scontrol show job` run-time excludes suspended time.

## Relationship to PR #274

Disjoint in content (#274 = exit-code/reason fields; this = state-transition
RPCs and suspended-time accounting). Both edit the same six files but in
separate regions; whichever lands second rebases with trivial adjacent-addition
conflicts. Branched off `upstream/main`.

## Conventions

- Issue title form `parity(slurm): ...`; PR title must be conventional-commit
  (`feat(spur): ...`) — `validate-pr-title` CI rejects the `parity(slurm):` form.
- `anyhow::Result` in app code, `thiserror`/explicit in library code; no
  `unwrap()` in library code.
- After proto edits across a branch switch, force proto regen:
  `touch proto/slurm.proto crates/spur-proto/build.rs && cargo build -p spur-proto`.
