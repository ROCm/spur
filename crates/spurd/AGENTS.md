# AGENTS.md -- spurd (Spur Node Agent Daemon)

Job execution, stepd supervision, containers, and k0s are described cross-crate in
`docs/knowledge/execution/job-execution.md` and `docs/knowledge/execution/containers-k0s.md`;
agent auth in `docs/knowledge/security/auth-model.md`. This file covers spurd-local constraints.

## Coupling

- The `[lib]` target is linked by `spur-stepd`, so changing a public interface in any
  `lib.rs` module affects both binaries.
- Production launches always go through `spurstepd`. Direct `executor::launch_job` from
  `AgentService` is a test-only path (`force_legacy_launch`). In production a supervised job
  is tracked as `RunningJob::AllocationOnly` (its `try_wait` is always `None`), so the monitor
  loop's `try_wait`/epilog branch only runs in tests; completions arrive as `StepdCompleted`
  on `agent.sock`, and the monitor's real job is `reconcile_orphaned_allocations`.
- Time limits are enforced by the controller (`enforce_time_limits` in spurctld sends
  `CancelJob`), not by the agent's monitor loop.
- Control messages (signal, query, shutdown) use the per-session `runtime.sock`; completion
  notifications use the separate `agent.sock`. Both are line-delimited JSON, not protobuf.
- A restarted supervisor answers `Hello` with a new instance ID; the agent must re-validate it.
- Typed-step refusal uses the `spur-launch-refusal: step-share-busy` trailer
  (`spur_proto::REFUSAL_STEP_SHARE_BUSY`); `spur-cli`'s srun retries on it (`BusyStepWait`).

## Invariants

- `LaunchJob` verifies the execution credential, then validates the whole request (target
  node, root execution, `launch_placement`, the script) before recording the credential in
  `LaunchAcceptance` or fencing anything, so a refused request leaves the previous run and the
  credential untouched.
- `JobLifecycle` serializes setup/teardown per job ID. Without it a re-dispatch races the
  previous run's cgroup/spool cleanup, since those paths are reused.- Jobs run as process-group leaders; signals go to the whole group (`killpg`), not just the
  tracked PID.
- `AllocationOnly` (a companion node's slice with no user code) must NOT be PID-namespaced:
  as PID 1 its unhandled signals are dropped by the kernel.
- `heartbeat_loop` is spawned before stepd completion replay so a slow replay never looks like
  a dead node.
- On SIGTERM with held stepds, spurd does not deregister, so the controller does not fence
  running jobs.
- `RLIMIT_MEMLOCK` is raised at startup (BPF on kernels < 5.11) before anything inherits it.
- A missing config at the default path is fine; a missing explicit `--config` is an error.
- `LaunchError::NodeFault` (I/O failure in spurd's own spool tree) self-drains the node.
  A prolog failure also drains, but the controller drives that.
- `is_suspect_gpu_dropout` refuses to converge an inventory refresh that goes from >0 GPUs
  to 0 (likely a KFD/CDI loader error, not hardware loss).
- seccomp (`SPUR_SECCOMP=1`) and landlock (`SPUR_LANDLOCK=1`) are opt-in; failures only warn.
- Privilege drop has two paths: in `pre_exec` for non-namespaced jobs, and via `setpriv` inside
  the namespace wrapper for namespaced ones. Change both together.

## Core layout (`spec.rs`, `reporter.rs`)

- Layouts arrive on registration replies and launches (`Delivery`). A launch older than the
  watermark is ignored, and only a registration reply may clear the held layout to `None`
  (`RegistrationTicket` drops stale replies). A layout change reaches a running agent through
  a controller-requested re-register (`record_layout_heartbeat`); already-running jobs are
  never repinned.
- A layout change wakes `heartbeat_loop` via `Notify` so `layout_blocked_cpus` reaches the
  controller at once.
- `layout_report()` must take the ledger with a blocking `lock()`, never `try_lock`: a miss
  would read as nothing blocked.
- The root `cpuset.cpus` is narrowed whenever spec cores or cap-trimmed CPUs exist (a `cpus =`
  cap alone still narrows it). Root memory limits come only from spec-core layouts:
  `mem_spec_mb` on a cap-only layout is scheduling-only. `daemon_cpuset()` lets daemons use
  cap-trimmed CPUs.
- **Root ownership**: `release_root()` normally resets only what its marker (in `state_dir`)
  proves it wrote. A node holding any layout is spur-managed, so a root left narrowed by an
  earlier run is released even without the marker. With no layout held, an unmarked root is
  left as found (it may be an operator's own limits).
- **No narrowing**: `reconcile_root_cpuset()` keeps every CPU any `job_*` dir names, populated
  or not, so an orphan only widens the root. A content-changing `adopt()` and a fresh spurd's
  first enforce take a memory floor (the root's `memory.high` and the populated runs); while
  one of those runs, `memory.high` stays at least the floor and `memory.max` is not lowered.
  `read_live_jobs()` treats an unreadable `cgroup.events` as populated; `live_jobs()` also
  marks a run populated if the ledger holds it (a starting job's cgroup is `populated 0`
  until a process joins a `step_*` child), and reports a ledger run with no cgroup yet using
  `owner_cpu_ids()`. Only a true orphan (unpopulated and unknown to the ledger) leaves the
  floor's job set, so it cannot pin memory limits forever.
- `replay_adopted_allocations()` runs `apply_spec_exclusions()` first so a recovered job never
  lands on spec or another GPU's reserved cores. Contested recorded CPUs are dropped rather
  than failing the whole replay (that would leave live GPUs reading free). A cpuset read back
  from the job's cgroup is capped at `resources.cpus`, since the cgroup may still show the
  root's full cpuset.

## Typed srun steps (`step_resources.rs`)

- A typed step is refused rather than silently sharing the job's limits when the agent cannot
  enforce it (`unenforceable`).
- For a job with no `--mem`, `StepdJobResources.memory_ceiling_mb` (controller's
  `node_memory_ceiling_mb`, or detected memory from an older controller) is the job total, so
  the job cgroup and its typed steps' `check_room` share one ceiling. Native-host only.
- Two-phase claim: `ClaimStep` reserves on every node before any runs. `run_command` takes the
  claim right after admission, before replay/duplicate checks, so a re-sent `RunCommand`
  never holds a second claim. The TTL is clamped to `[STEP_CLAIM_TTL, MAX_STEP_CLAIM_TTL]`.
  A `claimed` request whose claim expired returns `FailedPrecondition` (non-busy, so the
  controller cancels the other nodes) instead of reserving single-phase.
- A cancel tombstone (`LaunchAcceptance::is_cancelled`) refuses later `RunCommand`/`ClaimStep`.
  A busy-retry release (`ClaimStepRequest{release: true}`) never tombstones, since srun retries
  the same step id.
- An `--overlap` step is limited like a typed step but holds nothing against the job's share.
- `pick_step_cpus` returns an empty pick when too few job CPUs are free; the step then holds
  CPUs by count and shares the job's cpuset. Silent degrade, not an error.
- **PendingStepShares** covers only the window between reserving a share and handing it to a
  supervisor. It is in-memory and per process; once a supervisor exists its stepd descriptor
  records the share, so running steps survive an agent restart with limits intact.

## Testing

- Live k0s/systemd tests are `#[ignore]` and need root plus `SPUR_CLUSTER_DERISK=1`.
