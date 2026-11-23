# Job Execution Pipeline

How a job travels from the controller's dispatch to a running process on a
compute node, how its exit propagates back, and how a restarted `spurd`
recovers. Crate-local constraints live in `crates/spurd/AGENTS.md` and
`crates/spur-stepd/AGENTS.md`. Container internals are in `containers-k0s.md`;
GPU resolution and environment variables are in `../devices/gpu-discovery.md`.

## 1. Dispatch and admission

- The controller sends `LaunchJob` to each target `spurd` with the spec, its
  binding decision (`allocated`), a `run_attempt` epoch, and (under
  `[auth] plugin = "spur"`) a signed execution credential. `run_attempt` is
  what lets the agent fence a stale launch whose ack was lost.
- Before touching any resource, the agent verifies the credential (what it
  binds is in `../security/auth-model.md`), refuses a launch whose
  `target_node` is not this agent, refuses uid 0 unless
  `[auth] allow_root_jobs` is set (`check_root_execution_allowed`), and
  validates `launch_placement` and the script. Only then does it record the
  credential in `LaunchAcceptance` or fence anything, so a refused request
  leaves the previous run and the credential untouched (and a retry of it is
  never answered as already launched).
- Default-deny root applies only when `spurd` itself runs as root. It must run
  before anything spawns: the uid comes from the wire, and the privilege drop
  treats uid 0 as "nothing to drop". `RunCommand` steps pass the same gate.

## 2. Agent-side launch

Order inside `launch_job`, and why:

1. Credential and root checks (section 1).
2. Acquire the per-job lifecycle lock (`JobLifecycle`). It is held until the
   run is tracked or its half-built state is cleaned up, so a re-dispatch never
   builds on top of the previous attempt's teardown.
3. Idempotent retry: if a supervisor for this exact `(job_id, step, run_attempt)`
   is already tracked, return success. This must precede fencing, which accepts
   an equal attempt and would otherwise stop the live run.
4. Fence older runs (`fence_displaced_run`) before reserving resources: the
   reservation drops the old run's ledger entry, which a failed fence would
   leave its still-running workload without. Each displaced `spurstepd` is
   SIGTERMed (liveness confirmed by pid + start ticks), lifetime owners first,
   and counts as gone only once its cgroup is reaped; with no recorded cgroup,
   the recorded workload tree is SIGKILLed. A tracked entry is removed only if
   it still points at the fenced session, since a newer attempt may have
   replaced it meanwhile. A newer attempt already tracked refuses the launch.
5. Reserve resources, then run the node prolog (`[hooks] prolog`, as root). A
   prolog failure returns `LaunchFailureProlog` in a successful response, with
   no self-drain: the controller owns drain and hold, because only it can pair
   them.
6. Spawn the supervisor (section 3).

## 3. Supervisor spawn and start gate

Every production launch runs under `spurstepd`. The agent writes the
`StepdLaunchSpec` to `<session_dir>/launch.json` and spawns the supervisor:

- Double fork (`spawn_stepd_process`, Slurm's `slurmstepd` pattern). The
  intermediate child exits so the grandchild reparents to init, and the parent
  `waitpid`s it immediately so it never lingers as a zombie. The grandchild
  calls `setsid()` (with the required `KillMode=process` on the `spurd` unit,
  an agent restart does not kill it), writes its real pid
  over a pipe, marks the write end `FD_CLOEXEC`, and `execv`s. A successful
  exec closes the pipe with no byte; a failed one writes an error byte before
  `_exit(127)`.
- Readiness: the agent polls `runtime.sock` with a `Hello` handshake plus
  `QueryState` until it answers or a deadline passes. `Hello` validates the
  protocol version and the session identity, so the agent knows it reached the
  right supervisor instance.
- Start gate: the supervisor holds the workload in `await_start` until the
  agent sends `Start`, which it does on the controller's `StartJob` RPC. The
  controller sends `StartJob` only after committing the job Running, so an
  `srun` on the script's first line cannot race the job's bookkeeping.

## 4. Workload launch

`executor::launch_job` runs inside the supervisor once the gate opens.

- SPANK `Init`/`TaskInit` run in the supervisor (root). `TaskProlog` then runs
  as the job user inside the step cgroup, and its `export`/`unset` edits fold
  into the workload's environment.
- Non-container: when `spurd` is root (and the launch is not a multi-rank PMIx
  wrapper, joined step, or allocation holder), the job runs as
  `/usr/bin/unshare --pid --mount --fork /bin/bash <wrapper>`, otherwise as
  `/bin/bash <script>`. The cgroup join happens in `pre_exec`, not parent-side
  after spawn, because under `unshare --fork` a parent-side move races the fork
  and misses the workload. The namespace wrapper drops privilege itself via
  `setpriv --init-groups` after its mounts; dropping before `unshare` makes the
  syscall fail with EPERM.
- Container: a raw `fork()` whose child joins the cgroup and raises
  `RLIMIT_MEMLOCK` while still root, runs `container_init()`, and reports `OK`
  or `E:<msg>` over a sync pipe before exec. Namespace and mount details are in
  `containers-k0s.md`.
- The job script and namespace wrapper are written under node-local
  `/var/spool/spur/`, not the user's `work_dir`, because root writes to an NFS
  root_squash mount fail.

## 5. Cgroups

- The job cgroup `/sys/fs/cgroup/spur/job_<id>_<attempt>` carries the limits
  and device filter; processes run in `step_<id>` leaves beneath it, because
  cgroup v2 refuses processes in a node whose children have controllers. Only
  the step owning the job's lifetime configures the job cgroup; other steps
  join it. A job with no `--mem` is capped at the node's schedulable memory
  rather than left unlimited.
- Failure policy: when `spurd` runs as root, failing to create the cgroup root
  or the job directory is always fatal. `[cgroup] required` gates only the
  softer failures (controller delegation, limit writes, device filter install,
  post-spawn join verification). A non-root (development) `spurd` consults
  `required` for every failure; the default `false` degrades with a warning.
- A leftover job cgroup from an earlier run is reaped (SIGKILL + rmdir) by
  `claim_cgroup_dir` and never adopted; as root, one that cannot be cleared
  fails the launch. A step's cgroup is a leaf, so reaping it cannot kill
  siblings.

## 6. Device isolation

- cgroup v2 has no `devices` controller, so `device_cgroup` builds a
  `BPF_PROG_TYPE_CGROUP_DEVICE` program in-process (no eBPF toolchain) and
  attaches it to the job cgroup. It default-denies, then allows base
  pseudo-devices (`/dev/null`, ptys, ...), host infra devices granted to every
  job (`/dev/fuse`, `/dev/infiniband`, `/dev/nvidia-caps`), and the job's
  allocated device nodes.
- It is installed only with `[cgroup] constrain_devices` (default on). If
  installation fails and `required` is false, the job runs unfiltered with a
  warning.
- The filter is per job, not per step: a step inherits it by living under the
  job cgroup. That is why a step whose job cgroup is missing is refused under
  `required` instead of creating one, which would carry no filter.
- For non-container jobs, the namespace wrapper (`build_namespace_wrapper`)
  replaces `/dev/dri` with a tmpfs holding only the allocated render nodes.
- GPU environment variables are userspace cooperation only, not enforcement;
  see "GPU environment variables" in `../devices/gpu-discovery.md`.

## 7. Completion and teardown

The supervisor's teardown order after the workload exits:

1. Read `memory.events` before the cgroup is removed; an OOM kill sets
   `OOM_SIGNAL_FLAG` on the reported signal.
2. Append `ExitObserved` to `obligations.jsonl` before any cleanup, so a crash
   in teardown or the epilog cannot erase the exit status.
3. Run `TaskEpilog` as the job user inside the still-live step cgroup.
4. Stop the PMIx server, reap the cgroup, and remove the container rootfs.
   The lifetime-owning step also removes the job spool; a step's spool
   outlives it so the agent can still read its output.
5. For the step owning the job's lifetime only: node epilog (`[hooks] epilog`,
   root) and SPANK `TaskExit`/`JobEpilog`. Then append `EpilogCompleted`.
6. Push `StepdCompleted` to `spurd` on `<state_dir>/runtime/agent.sock`. On
   `Acknowledged`, append `CompletionAcknowledged`.

On `StepdCompleted`, `spurd` checks the session capability, reports to the
controller with `ReportJobStatus` (an epilog failure carries a drain), and
answers `Acknowledged` only if that report landed, otherwise `Deferred`. It
releases the allocation and records `ResourcesReleased` only once cleanup is
confirmed or the completion is reported, and only for the job's last
supervisor of that attempt, because the allocation outlives individual steps.
`spurd` (not the supervisor) prunes finalized session directories.

If `spurd` is unreachable or the report fails, the obligation stays
unacknowledged on disk. `spurd` replays completion reports from the log at
startup and on a periodic retry loop; it does not re-run cleanup or hooks.

## 8. Multi-node jobs and steps

- PMIx: the supervisor, not the agent, hosts the PMIx server
  (`start_supervised_pmix`), so an agent restart does not take the rendezvous
  down. A supervisor crash does, and its ranks lose their server. The per-rank
  wrapper is built in the supervisor because only the server's host can hand
  out each rank's bootstrap environment. The two-phase `PreparePmix` dispatch
  is in `../common/architecture.md`.
- `--mpi=mpirun` batch: rank 0 runs `mpirun`; companion nodes run a hold
  script, because a batch job's per-node lifetime is tied to its script and
  exiting would release the node before `mpirun` finishes.
- `srun` inside a batch script sends `RunStep` to the controller, which sends
  `RunCommand` to each participating agent. Each spawns a separate `spurstepd`
  for the step, in a leaf of the batch job's cgroup, hosting its own PMIx
  server for the step's ranks.

### Script placement

A batch script runs on the batch host only (`ResolvedPlacement::BatchHost`, the
first allocated node) or on every allocated node (`AllNodes`). The controller
decides once per run at dispatch (`script_placement::resolve_run_placement`) and
records it in `WalOperation::JobStart.resolved_placement`; `LaunchJobRequest`
carries it as `script_placement` (empty from older controllers).

1. A batch job (`JobSpec::needs_native_nodes`) never runs on K8s virtual nodes:
   their agents launch only SpurJobs, which carry no script. It is not eligible
   for them (`NodePlacement::eligible`), waits as `BadConstraints` with
   `BATCH_SCRIPT_K8S_UNSUPPORTED` while too few other nodes exist, submit
   rejects a `--nodelist` that leaves too few (`validate_batch_nodelist`), and
   dispatch holds a run that still includes one.
2. Otherwise `JobSpec.script_placement`, else `[controller] script_placement`
   (default `auto`). `auto` is a text scan: `BatchHost` when the script itself
   calls `srun`, `mpirun`, or `mpiexec`. srun and salloc jobs always use the
   scan, as do SpurJobs, whose missing script scans as `AllNodes`.

On `BatchHost`, every node but the one at `task_offset` 0 runs
`batch_companion_hold_script` with stdio on `/dev/null`, holding its slice for
the steps the script launches. The controller stops those nodes when the batch
host reports (`ended_run_stragglers`), and the batch host's exit is the job's.
A `BatchHost` script is a driver, not an MPI rank, so batch-level PMIx fan-out
and prepare are skipped.

Agents advertise `CAP_SCRIPT_PLACEMENT`. One without it ignores the field and
applies the scan (`Node::reads_placement_off_script`), which changes the launch
only on a multi-node or PMIx run (`ResolvedPlacement::launches_differently_from`).
For a job where it does (`JobSpec::needs_placement_aware_agents`, given
`ClusterState.script_placement_default`), backfill and the victim search drop
such nodes (`NodePlacement::without_misplacing_agents`), a job whose additive
`--nodelist` names one waits, and dispatch backs off a run that still lands on
one.

### Per-node runs (`AllNodes`)

Each copy's exit stands on its own (`runs_per_node` in `cluster.rs`; salloc and
native srun allocations run no script and are left out).

- While such a job spans more than one node and is Running outside a preemption
  grace (`ends_copy_by_copy`), `node_complete` proposes the exit as
  `WalOperation::JobCopyExited`. `JobNodeComplete` keeps its older meaning (the
  first report ends the run), so every controller replays an older leader's log
  the same way. `ClusterManager::apply_node_exit` applies both; its tests
  (`cluster::tests::node_exit`) pin the decision table without the WAL.
- A clean exit leaves the job Running (`NodeCompleteResult::StillRunning`) but
  keeps the node's slice charged (`Job.retained_nodes`): `spurd` keeps a job's
  allocation until its last supervisor on the node exits, steps included, so a
  node given back at its copy's exit could be dispatched to while the agent
  still holds it.
- The first failed copy is recorded in `Job.first_failed_node`, moves the job to
  Completing, and supplies the job's state, exit, and signal; a copy OOM-killed
  while being stopped cannot change it (`Job::reported_outcome`, also read by
  the completing timeout's force-finish). The apply drops a repeat
  `JobCopyExited` so it cannot overwrite that failure; a repeat
  `JobNodeComplete` still overwrites, as on older controllers. Steps can no
  longer target a node that finished.
- When the run ends (first failure, last copy, or any report in a preemption
  grace), a kept node goes back only once no live user step of the job runs
  there (`nodes_running_steps`); the copy whose exit ends the run is kept the
  same way. `ended_run_stragglers` stops every node the run still holds
  (`Job::held_nodes()`), kept ones included, on the report that ends the run or
  a retry of it, never on a stopped copy's own report.
- The run waits (Completing, or Running in its grace) until each kept node's
  last step records its `JobStepComplete`, which frees the node
  (`release_after_step_end`) and then ends the run (`finish_settled_run`). Only
  an end the nodes confirm counts: in such a run (`keeps_nodes_for_steps`), a
  client's own end of a step (an `srun` abandoning it, or closing its PTY) is
  recorded by `record_step_end_once_gone` only once every node of the step
  answers `AwaitStep`, which spurd does after the step's cleanup. `CancelStep`
  only signals, so a cancel escalates to SIGKILL after the kill wait.
- A step whose client died records no end (only `run_step` records a numbered
  step's), so the completing-timeout loop also runs
  `confirm_steps_on_kept_nodes` every pass for a Completing run's live steps on
  kept nodes; a leader-local set (`begin_step_end_confirmation`) keeps one wait
  per step. Not-found from an agent is final once `not_found_is_final` holds (it
  registered before the step started, or has been up as long as a re-attach
  waits), else after that long. A node that never confirms leaves the step live.
- A grace hands each freed node to the preemptor at that point. The completing
  timeout's force-finish quarantines and kills kept nodes like unreported ones
  without changing the verdict. A kept node that goes Down or is removed takes
  its steps with it: `evict_jobs_on_node` frees its slice and drops it from
  `retained_nodes` at any point, ending a run that only waited on it. The
  steps' records stay open, since a step spanning another kept node may still
  run there.
- Every other ending (`JobComplete`, requeue, preemption, eviction) frees all
  but `Job::released_nodes()` at once. While a run holds this state, snapshots
  are fenced from older controllers (see
  [Raft](../consensus/raft.md#breaking-change-sensitivity)). Suspend keeps its
  own handling.

## 9. Interactive sessions

- `salloc` gets an allocation-only supervisor on `STEP_EXTERN`: no workload,
  just the held cgroup and allocation. The user's shell runs client-side.
- `srun --pty`: the supervisor opens the pty and the slave becomes the
  workload's controlling terminal. The supervisor holds a dup of the master in
  custody on `<session_dir>/ptyfd.sock` (`SCM_RIGHTS`), so the terminal
  survives an agent restart: a restarted agent reclaims orphaned masters with
  `CUSTODY_RECLAIM_ANY`, and sends `CUSTODY_RELEASE` once the terminal closes.
- `TaskProlog` `print` output to a PTY uses non-blocking writes
  (`write_fd_nonblocking`) so a launch with no reader attached yet never
  stalls; output beyond the terminal buffer is dropped with a warning.

## 10. Recovery after agent restart

Supervisors run in their own session and the `spurd` unit must set
`KillMode=process`, so a `spurd` restart leaves them running;
the new `spurd` reconnects over their sockets. On startup:

1. `StepdStore::discover_live` sorts session directories into Live and Stale by
   pid + start ticks; an unreadable descriptor is rejected.
2. Stale: `record_lost_supervisor_exit` records a synthetic SIGKILL exit when
   no `ExitObserved` exists, so the normal completion replay reports the job.
   A stale session whose cleanup is unconfirmed keeps its allocation, since a
   dead supervisor can leave live descendants. A corrupted session's cgroup is
   reaped using the identity in its directory name.
3. Live: re-adopted into the `running` and `stepds` maps, and their
   allocations replayed into the ledger. Descriptors predating stable GPU ids
   have their positional ids translated (`translate_legacy_gpu_ids`).
4. Unacknowledged completions are replayed from `obligations.jsonl`
   (`replay_unacknowledged_stepd_completions`), after the heartbeat loop starts.
5. Each recovered session is reported to the controller
   (`report_stepd_recovery`), which may fence it; a fenced session is torn
   down.
