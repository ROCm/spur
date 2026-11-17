# AGENTS.md -- spurd (Spur Node Agent Daemon)

## Overview

`spurd` is the per-node agent daemon for the Spur scheduler. It runs on every
compute node in the cluster as a long-lived root process (port 6818 by default)
and is the node-side counterpart to `spurctld` (the controller, port 6817).

Responsibilities:
- **Register** with spurctld and send periodic heartbeats carrying node state.
- **Discover** local hardware (CPUs, memory, GPUs via CDI/GRES) and report it.
- **Execute** jobs: receive launch RPCs from the controller, fork/exec the
  workload inside cgroups/namespaces, enforce resource limits, reap exits.
- **Supervise** steps via `spurstepd` (a separate per-step supervisor binary
  communicating over Unix sockets).
- **Manage** an embedded k0s Kubernetes worker/controller via a systemd unit.
- **Authenticate** callers with JWT/bearer tokens (Tower middleware on gRPC).
- **Recover** after restart: re-adopt live stepd sessions, replay unacknowledged
  completions, fence stale jobs, reap orphaned cgroups.

The crate ships both a `[[bin]]` (the daemon, `src/main.rs`) and a `[lib]`
(`src/lib.rs`). The library is reused by `spur-stepd` so the supervisor binary
can call into `executor`, `container`, `stepd`, and supporting modules.
`lib.rs` exports `agent_server`, `cluster`, `reporter`, and `controller_auth`
as well, so `spur-stepd` links all of it; the runtime-only surface is not
split into its own crate yet, so this is a runtime, not link-time, isolation
boundary.

---

## Directory Structure

```
src/
  main.rs             Daemon entry point: CLI parsing, config load, startup
                      orchestration, registration, heartbeat loop, SIGTERM
                      handling, periodic inventory refresh.

  lib.rs              Library root. Re-exports all public modules.

  agent_server.rs     gRPC service impl (`SlurmAgent` trait): LaunchJob,
                      CancelJob, RunCommand, SignalJob, AttachSession,
                      InteractiveSession, Start/Stop/GetClusterComponent, etc.
                      Contains AgentService, TrackedJob, RunningJobs map,
                      completion notification listener, the 2-second reaper/
                      monitor loop, and stepd recovery/adoption logic.
                      (~25k lines, largest file in the crate.)

  reporter.rs         NodeReporter: register, heartbeat_loop, deregister,
                      resource discovery, inventory delta classification,
                      debounced refresh, controller failover/deprioritization.

  executor.rs         Job launch: fork/exec, cgroup setup (cgroupv2), namespace
                      isolation (PID/mount via /usr/bin/unshare for non-container;
                      user namespace only for rootless containers), I/O wiring
                      (file or PTY), privilege drop, prolog execution, SPANK
                      plugin hooks, container path (raw fork + pivot_root).
                      Seccomp (SPUR_SECCOMP=1) and landlock (SPUR_LANDLOCK=1)
                      are opt-in; failures only log warnings. Owns
                      JobLaunchConfig, LaunchResult, RunningJob enum.

  stepd.rs            spurstepd protocol: StepdLaunchSpec (written to
                      launch.json on disk, read by the supervisor from its
                      command-line path), StepdStore (on-disk session state
                      in /var/spool/spur/runtime/), StepdDescriptor (persisted
                      metadata), obligation log, supervisor lifecycle
                      (start_job, signal, shutdown, serve_control), PTY
                      deposit/reclaim for interactive sessions.

  spec.rs             SpecEnforcer: applies the controller-delivered
                      CoreLayout to the `spur/` cgroup root (spec-core and
                      cap-trimmed daemon pinning) and to the local
                      NodeAllocation ledger (LedgerExclusions: spec/trimmed
                      CPUs excluded outright, each GPU's reserved CPUs
                      excluded from the general pool). RegistrationTicket /
                      Delivery track where a layout came from so only a
                      registration reply clears a stale one.

  step_resources.rs   A typed srun step's own share of its job on one node:
                      StepResources -> NodeRequest (resolve(), scaled by
                      tasks-on-node), unenforceable() (refuses rather than
                      silently sharing the job's limits when cgroups/root/
                      memory/device enforcement is unavailable),
                      PendingStepShares (in-flight typed-step reservations,
                      keyed by job/run_attempt/step, checked against the
                      job's live typed steps so concurrent steps cannot
                      oversubscribe its share), pick_step_cpus (pins a typed
                      step's CPUs to its own GPUs' reserved cores first, then
                      cores near those GPUs, then the rest of the job's CPUs).

  container.rs        Native container runtime (no Docker/containerd). Squashfs
                      images, mount/PID namespace isolation (user namespace
                      only for rootless), bind mounts, GPU
                      passthrough (AMD: /dev/kfd + renderD*; NVIDIA: /dev/nvidia*),
                      rootfs overlay, ContainerConfig.

  cluster.rs          k0s/K8s integration: ClusterSupervisor owns a systemd
                      unit for k0s controller/worker/single. Reconcile loop
                      writes/enables/starts the unit; K0sAgent drives
                      start/stop/status/drain/delete via controller RPCs.
                      CDI spec generation for GPU pods. Scoped kubeconfig
                      minting. Local-path storage provisioner manifest.

  device_cgroup.rs    cgroup-v2 device access control via BPF_PROG_TYPE_CGROUP_DEVICE.
                      Builds eBPF bytecode in-process (no toolchain). Maintains
                      base device rules (/dev/null, ptys, etc.) and per-job
                      allowed device lists. Host infra devices (fuse, IB verbs,
                      nvidia-caps) granted to all jobs.

  auth_middleware.rs   Tower layer on the agent gRPC surface. Under
                      `plugin = "spur"`, accepts native credentials only
                      (ControllerRpc verified against Ed25519 controller-
                      verification JWKS, UserRpc verified against HMAC auth
                      JWKS; JWTs are rejected). Under `plugin = "jwt"`,
                      checks HS256 JWTs signed with the cluster's jwt_key.
                      Inserts verified Identity into request extensions. In
                      permissive mode (auth.mode = "permissive") uncredentialed
                      calls are accepted with a warning.

  controller_auth.rs  Agent -> controller credentials. Only MINTS outbound
                      credentials. Under `[auth] plugin = "spur"`, mints a
                      fresh credential from the local spurauthd socket for
                      every outbound RPC. Inbound verification is handled by
                      auth_middleware.rs, not here.

  job_entry.rs        JobEntry: per-tracked-job metadata (PID, namespaces,
                      uid/gid, cgroup) used to build nsenter arguments for
                      attaching to a running job.

  job_lifecycle.rs    Per-job-id mutex that serializes setup/teardown. Prevents
                      a re-dispatch from racing the previous run's cleanup when
                      cgroup/spool paths are reused.

  step_completion.rs  StepCompletions: oneshot rendezvous between a launching
                      RPC (parks waiting for the step to finish) and the
                      supervisor's completion notification over the agent socket.

  task_hook.rs        TaskProlog / TaskEpilog execution. Runs as the job user
                      inside the step cgroup. Parses Slurm-compatible stdout
                      protocol (export/unset/print directives).

  mpi_plugin.rs       Runtime-loaded PMIx plugin host (dlopen). Wraps the
                      spur_mpi_pmix_* C ABI: server_start, server_stop,
                      verify_peers, setup_fork_env. Per-step namespace key.

  privdrop.rs         Privilege drop: resolves uid/gid/supplementary groups in
                      the parent, applies setgroups/setgid/setuid in the child
                      (async-signal-safe only). Refuses uid-0 execution unless
                      allow_root_jobs is set.

  seccomp.rs          Seccomp-BPF syscall filter (default-deny whitelist).
                      Blocks ptrace, mount, bpf, unshare while allowing the
                      ~150 syscalls GPU compute needs (ioctl for KFD/DRM, etc.).
                      Applied in pre_exec after fork.

  landlock.rs         Landlock filesystem access control (kernel 5.13+).
                      Restricts native-host jobs to work_dir (rw), system paths
                      (ro), GPU devices (rw). Skipped for container jobs (chroot
                      already provides isolation). Graceful no-op on older kernels.

  admission.rs       Node allocation ledger: CPU/GPU/memory entitlement tracking,
                      allocate/release/reconcile, launching TTL, capacity updates.

  pty.rs              PTY primitives: openpty with optional window size, pre_exec
                      hook to make the slave the controlling terminal. Used by
                      executor (batch PTY launch) and interactive session handler.

  startup_spool.rs    One-shot orphan job-spool cleanup at startup: discovers
                      `job<N>` dirs, asks the controller (strict GetJobs, batches
                      of 128) which are terminal or absent below next_job_id,
                      keeps anything owned locally, in a runtime session, a
                      populated cgroup, or held by a process (/proc scan), then
                      deletes the rest. 60 s budget; any doubt keeps the dir.
                      Also unnamed container rootfs dirs (`job_<N>`,
                      `step_<N>_<M>` in container_dir()): unmounted in the pass,
                      then deleted by a background task after serving.

  spool_fs.rs         Descriptor-relative spool deletion shared by startup and
                      completion cleanup: follows symlinks only in the root path,
                      never beneath it, no mount crossing (openat2 with an
                      openat + mnt_id fallback), streamed readdir, 256-level
                      depth cap, identity re-checks, kept_reason() for logging.
```

---

## Key Types and Interfaces

### agent_server.rs
- **`AgentService`** -- the gRPC service object. Holds the reporter, device
  registry, running-jobs map, k0s agent, allocation, step completions, job
  lifecycle serializer, hooks config, cgroup config, MPI config, and SPANK host.
  Implements `SlurmAgent` (generated from protobuf).
- **`TrackedJob`** -- per-job tracking entry: RunningJob handle, PID, stdout/stderr
  paths, step_id, cgroup_path, JobEntry (namespace metadata). Stored in the
  running-jobs map (`Arc<Mutex<HashMap<u32, TrackedJob>>>`).
- **`RunningJobs`** -- type alias for the shared running-jobs map.
- **`CompletionListenerContext`** -- state shared with the agent-socket listener
  that receives exit notifications from spurstepd.

### executor.rs
- **`JobLaunchConfig`** -- all resolved parameters for launching a job process.
- **`LaunchResult`** -- the running process handle, resolved I/O paths, PTY master
  fd, cgroup path, post-prolog environment.
- **`RunningJob`** -- enum: `Managed` (tokio Child), `Forked` (container, raw pid
  + optional pidfd), `AllocationOnly` (companion allocation holder with no process).
- **`LaunchError`** -- typed: `PrologFailed`, `NodeFault` (drains the node),
  `Other`.
- **`launch_job()`** -- top-level entry point for executing a job on this node.

### stepd.rs
- **`StepdLaunchSpec`** -- written to `launch.json` on disk by the agent;
  the supervisor reads it from the path passed as a command-line argument.
  Contains the full job spec, cgroup config, resources, PMIx plan, device
  injection plan.
- **`StepdStore`** -- on-disk session state manager. Root at
  `<state_dir>/runtime/`. Each session is a directory named `<job>.<attempt>.<step>`.
- **`StepdDescriptor`** -- persisted descriptor (`descriptor.json`) for a stepd
  session: job id, attempt, step id, PID, socket path, cgroup path.
- **`Stepd`** -- in-memory handle for a live supervisor: poll completion, signal,
  begin teardown.
- **`StepdRequest` / `StepdResponse`** -- line-delimited JSON protocol over
  the Unix socket between agent and supervisor.

### reporter.rs
- **`NodeReporter`** -- registers with the controller, runs the heartbeat loop,
  re-reads WireGuard keys, carries the live allocation and k0s status.
- **`HeldJobs`** trait -- source of currently-held job IDs for heartbeat.
- **`InventoryDelta`** / **`RefreshAction`** -- debounced inventory change
  classification (unchanged / grew / shrank / held-device-lost).
- **`adopt_core_layout()`** -- stores a controller-delivered `CoreLayout` +
  `LayoutRevision`, recording the delivery source (registration reply vs
  heartbeat); `ensure_spec_enforced()` applies it to the cgroup root and the
  job ledger if not already in effect.
  A call that changes the layout wakes `heartbeat_loop` through a `Notify`
  (`request_heartbeat()` does too, once after startup replay), so the
  `Heartbeat` RPC's `layout_blocked_cpus` field reaches the controller at once.
- **`layout_report()`** -- the held fingerprint and enforcement error with
  `NodeAllocation::blocked_reserved_cpus()` against the held layout's
  `gpu_reserved`, snapshotted under one `spec` lock, then counted under a
  blocking ledger `lock()` (never `try_lock`: a miss would read as nothing
  blocked). Runs in the heartbeat's `spawn_blocking` after `apply_spec_layout()`.

### spec.rs
- **`SpecEnforcer`** -- holds the layout last delivered and the cgroup root
  path it is applied to. `needs_apply()` / `apply_spec_layout()` enforce the
  root -- a `cpuset.cpus` excluding spec cores and cap-trimmed CPUs -- whenever
  either is non-empty, so a `cpus =` cap alone (no spec cores) still narrows
  the root; a layout with neither releases it. Root memory limits come only
  from spec-core layouts: `mem_spec_mb` on a cap-only layout stays
  scheduling-only, never written to the root. `daemon_cpuset()` gives spurd's
  own thread pinning under `daemons_off_spec` (spec cores plus cap-trimmed
  CPUs -- daemons may use CPUs a cap keeps from jobs).
- **`LedgerExclusions`** -- spec/trimmed CPUs (excluded outright) and each
  GPU's reserved CPUs (excluded from the general pool only); `apply()` calls
  `NodeAllocation::set_spec_exclusions()` / `set_gpu_reserved()`.
- **`RegistrationTicket`** / **`Delivery`** -- where a layout delivery came
  from, so a stale heartbeat-sourced layout can never overwrite a fresher one
  and only a registration reply clears a previously-held layout to `None`.
- **Root ownership** -- `release_root()` normally resets only limits its own
  marker (in `state_dir`) proves it wrote. A node holding ANY layout (spec
  cores or `restricted_cores_per_gpu` only) is spur-managed, though: the root
  is also released when its on-disk cpuset/memory show it was left narrowed
  by some earlier run, even without that marker, logged once via `info!`. A
  node with no layout held (`held == None`) still leaves an unmarked root
  exactly as found -- that case may be an operator's own limits.
- **No narrowing** -- `reconcile_root_cpuset()` keeps every CPU any `job_*`
  dir's cpuset names in the root, populated or not: an orphan dir only ever
  widens the root, never narrows it. A content-changing `adopt()`, and a
  fresh spurd's first enforce, take a memory floor: the root's on-disk
  `memory.high` and the runs then counted as populated (`job_<id>_<attempt>`
  dirs plus ledger runs with no cgroup yet). While one of them runs,
  `memory.high` stays at least the floor and `memory.max` is not lowered;
  raising applies at once. `reporter.rs::read_live_jobs()` sets each
  `LiveJob.populated` from its `cgroup.events`' `populated` field (unreadable
  counts as populated, the conservative default); `NodeReporter::live_jobs()`
  then also marks a dir populated if the node's ledger still holds that run,
  covering a starting job whose cgroup predates its first process (real
  `cpuset.cpus` already on disk, `populated 0` until a process joins a
  `step_*` child). A run the ledger holds but with no cgroup dir at all yet
  (reserved, before cgroup creation) is reported with the ledger's own
  `owner_cpu_ids()`, not an empty list, so it is not narrowed out of the
  root either. Only a true orphan -- unpopulated and unknown to the
  ledger -- is excluded from the floor's job set, so it can never pin
  `memory.high`/`memory.max` forever.

### step_resources.rs
- **`StepLimits`** -- what a typed step narrows its job's limits to on this
  node (cpus, memory_mb, gpus, pinned cpu_ids); `None` leaves a resource
  shared with the rest of the job. An `overlap` step (`srun --overlap`) is
  limited the same way but holds nothing exclusively.
- **`NodeRequest`** -- a step's request resolved for one node, before any GPU
  is picked; `resolve()` scales `cpus_per_task`/`mem_per_*`/gpu counts by the
  step's tasks on this node.
- **`PendingStepShares`** -- in-flight typed-step reservations for a job
  attempt, checked against both its live (held) typed steps and other
  pending launches so two concurrent steps cannot oversubscribe the job's
  share; `reserve_pinned()` returns a `ShareExhausted` error (mapped to gRPC
  `ResourceExhausted`) when the ask does not fit.
- **`PinSource`** / **`pick_step_cpus()`** -- CPUs for a typed, GPU-holding
  step come from its own GPUs' reserved cores first, then cores near those
  GPUs, then the rest of the job's CPUs; too few free CPUs falls back to the
  job's shared cpuset instead of failing the step.

### cluster.rs
- **`K0sAgent`** -- RPC-driven owner of k0s on this node. start/stop/status/
  drain/delete, join token minting, admin/user kubeconfig, CDI spec writes.
- **`ClusterSupervisor`** -- owns and reconciles a single spurd-managed k0s
  systemd unit. Writes the unit file (Type=simple, KillMode=process, Delegate=yes,
  Restart=always), enables it, starts it, heals it every 30s.
- **`ClusterRole`** -- Controller / Worker / Single.
- **`K0sNodeState`** -- atomic state (unit_active, restart_count, install_ms)
  read by the reporter each heartbeat.

---

## Agent Registration and Heartbeat

### Startup sequence (main.rs)
1. Parse CLI args and load `/etc/spur/spur.conf` (best-effort; missing file at
   the default path is expected).
2. Raise RLIMIT_MEMLOCK (BPF on kernels < 5.11).
3. Install controller auth (native mint via spurauthd, or none).
4. Discover on-disk stepd sessions (`StepdStore::discover_live`): classify as
   live (PID exists), stale (PID gone), or corrupted (unparseable descriptor).
5. Reap stale sessions' cgroups (`executor::cleanup_cgroup`), record lost
   supervisor exits.
6. Detect node address: explicit `--address` > WireGuard interface > hostname.
7. Initialize device registry (CDI cache + GRES config).
8. Discover local resources (cpus from sysfs, memory, GPUs from registry).
9. Parse node labels from `--label` flags.
10. Create `NodeReporter`, call `reporter.register()` (gRPC `RegisterAgent`).
11. Spawn `heartbeat_loop` immediately -- before completion replay, so a slow
    replay never looks like a dead node.
12. Replay unacknowledged stepd completions.
13. Build `AgentService`, adopt recovered stepds, replay allocations (then
    `request_heartbeat()`), settle stale stepds, then run the one-shot orphan
    job-spool cleanup (`startup_spool::run`, bounded by a 60 s budget) before
    anything serves.
14. Bind agent-socket Unix listener for supervisor completion notifications,
    spawn the completion retry background task.
15. Adopt any running spurd-owned k0s unit, spawn the k0s supervise loop.
16. Start the reaper/monitor loop (`start_monitor`).
17. Spawn periodic inventory-refresh loop (debounced, seen-twice).
18. Start the gRPC server (with auth middleware), then spawn the background
    deletion of orphan container rootfs dirs queued in step 13
    (`RootfsCleanup::spawn`; dropping the handle stops it on exit).
19. Report recovered/stale sessions to the controller.
20. Wait on SIGTERM or server exit.

### Heartbeat protocol
`NodeReporter::heartbeat_loop` runs every 30s (hardcoded). Each heartbeat:
- Reads free memory, CPU load from `/proc/meminfo` and `/proc/loadavg`.
- Re-reads the WireGuard public key from the mesh interface.
- Collects held job IDs from the running-jobs map.
- Collects per-job GPU stable_ids from the NodeAllocation.
- Reads k0s node state (unit_active, restart_count, one-shot install_ms).
- Applies the held layout to the cgroup root and reports its fingerprint with,
  per GPU, the reserved CPUs held by a job not holding that GPU.
- Sends a `Heartbeat` RPC to the controller.
- On failure, deprioritizes the failed endpoint for `failover_cooldown` and
  tries the next one.

### Deregistration
On SIGTERM with no held stepds, the agent deregisters from the controller
(5s timeout). If stepds are held, it preserves registration so the controller
does not fence the running jobs.

---

## Job Execution

### Launch flow (agent_server -> executor)
1. Controller sends `LaunchJob` RPC with the full job spec.
2. `AgentService` acquires the `JobLifecycle` lock for this job ID (serializes
   with any in-progress teardown of the same ID).
3. Resolves GPU device injection from the allocation + device registry.
4. Builds `JobLaunchConfig` from the RPC fields + agent config.
5. Prepares a `StepdLaunchSpec` and launches `spurstepd` as the supervisor.
   Production always uses spurstepd (`stepd_enabled = true` at
   `agent_server.rs:6125`); direct `executor::launch_job` calls are a
   test-only path gated behind `cfg(test)` via `force_legacy_launch`.
6. `launch_job` runs the prolog script (if configured), then `spawn_job_process`.

### spawn_job_process
- Creates the spool directory (`/var/spool/spur/job<id>/`), writes the job
  script there.
- Sets up cgroup-v2: creates the job cgroup under `/sys/fs/cgroup/spur/`,
  writes cpu/memory/device limits, attaches the BPF device filter.
- Opens I/O (file-based stdout/stderr or PTY pair).
- Runs SPANK `Init` / `TaskInit` hooks.
- Runs TaskProlog (if configured) as a child of the agent process (not
  inside the job's PID/mount namespace).
- Forks the job process:
  - **Non-container**: `tokio::Command`. When spurd is root and the launch
    is not pmix multi-task, a joined step, or an allocation holder, the
    COMMAND is `/usr/bin/unshare --pid --mount --fork /bin/bash <wrapper>`
    (PID + mount namespace, no user namespace). Otherwise the command is
    `/bin/bash <script>` directly. pre_exec hooks add cgroup join, opt-in
    seccomp filter (`SPUR_SECCOMP=1`, failures log a warning), opt-in
    landlock (`SPUR_LANDLOCK=1`, failures log a warning), RLIMIT, and
    stdio wiring. Privilege drop happens in pre_exec only on the
    non-namespace path (namespaced jobs drop privilege inside the wrapper).
  - **Container**: raw `fork()` + `container_init()`. As root, only
    NEWNS|NEWPID; user namespace is used only for rootless operation.
    pivot_root into squashfs rootfs, bind mounts for GPU devices,
    privilege drop.

### Monitor loop (`start_monitor`)
A background task polls every 2 seconds:
- `try_wait` on each tracked RunningJob. In production, supervised jobs are
  tracked as `RunningJob::AllocationOnly` whose `try_wait` always returns
  `None`; completions arrive as `StepdCompleted` notifications on
  `agent.sock`. The `try_wait`/epilog branch is effectively the test-only
  direct-launch path (gated behind `force_legacy_launch`).
- `reconcile_orphaned_allocations`: reclaims allocations with no tracked,
  non-launching job. In production this is the monitor's primary function.
- Reports any completions and runs hooks.
- Time limits are enforced by the controller (`enforce_time_limits` in
  `scheduler_loop.rs` sends `CancelJob`), not by this loop.

---

## stepd Spawning

### Architecture
For most jobs, spurd delegates execution to `spurstepd`, a separate binary
that acts as the per-step supervisor. This provides process isolation: the
supervisor outlives any single RPC and can be re-adopted if spurd restarts.

### Unix socket protocol
Communication uses line-delimited JSON over a Unix socket at
`<state_dir>/runtime/<job>.<attempt>.<step>/runtime.sock`.

- **`StepdRequest`**: `Hello` (handshake with instance ID), `QueryState`,
  `Start`, `SignalAllocation`, `Shutdown` (cancels before Start; after Start,
  triggers begin_teardown internally — cgroup-first SIGTERM).
- **`StepdResponse`**: `Hello`, `State` (runtime snapshot), `Acknowledged`,
  `Rejected`.
- **`AgentNotification`**: sent over the *agent* socket
  (`<state_dir>/runtime/agent.sock`) by the supervisor when a step completes.
  Carries job_id, run_attempt, step_id, exit_code, signal, epilog_failed,
  capability.

### Session lifecycle
1. Agent prepares a session directory, writes `descriptor.json` and the launch
   spec (`launch.json`).
2. Agent spawns `spurstepd` pointing at the session directory.
3. Supervisor reads `launch.json` from its command-line argument and binds
   `runtime.sock`. Agent connects and sends `Hello`.
4. Supervisor calls `executor::launch_job` to fork/exec the workload.
5. On exit, supervisor writes to the obligation log and sends an
   `AgentNotification` over the agent socket.
6. Agent acknowledges the completion to the controller, then marks the session
   finalized. Finalized sessions are pruned on startup and periodically.

### Recovery after spurd restart
- `StepdStore::discover_live` classifies sessions by probing supervisor PIDs.
- Live sessions are re-adopted: the agent monitors them for completion.
- Stale sessions (supervisor dead): cgroups are reaped, lost-supervisor exits
  are recorded, and the controller is notified to fence them.
- Corrupted sessions (unparseable descriptor): treated like stale -- cgroup
  reaped, fencing report sent.
- `replay_adopted_allocations()` rebuilds the in-memory `NodeAllocation`
  ledger for every live (re-adopted) descriptor: `apply_spec_exclusions()`
  runs first so a recovered job never lands on spec or another GPU's
  reserved cores. A descriptor's recorded `cpu_ids` are re-claimed via
  `claimable_cpu_ids()` (drops cores no longer free rather than rejecting
  the whole replay); without recorded CPUs, `read_live_cpus()` reads the
  job's own cgroup `cpuset.cpus.effective` to recover what it actually holds
  (`LiveCpus::Gone` when the cgroup is gone -- nothing to place; `Unknown`
  when it cannot be read -- counted but not placed on specific cores). A
  `Running` cpuset is capped at `resources.cpus` after claiming, since the
  job's cgroup can still show the whole root's cpuset if it never wrote its
  own (e.g. no recorded cpu_ids and no cpuset written before an upgrade).

---

## Typed srun Steps

A step that types `-c`/`--mem*`/`--cpus-per-gpu`/`--mem-per-gpu` or a GPU
count/type (a `RunCommandRequest`/`RunStepRequest` carrying `resources: Some`)
gets its own CPUs, memory, and GPUs on each node, pinned to its GPUs'
reserved cores when the node has any (`step_resources.rs`). A plain step
(`resources: None`) keeps sharing its job's cgroup limits, as before.

- **Memory ceiling for a no-`--mem` job (native-host only).** The controller
  sends the node's schedulable memory as `node_memory_ceiling_mb` on launch;
  when that is 0 (an older controller), the agent falls back to its own
  detected total memory. The value is stored in
  `StepdJobResources.memory_ceiling_mb` and used as the job's memory total
  whenever the job itself has no `--mem` budget, so the job's cgroup and its
  typed steps' `check_room` share the same ceiling rather than one
  unconstrained share split unchecked between concurrent steps. K8s pods get
  no such limit (decided in SPUR-404): this is native-host only.
- **Refusal, not silent weakening.** A typed step is refused outright --
  rather than launched sharing the job's limits -- when the controller lacks
  `step-resources` in `PingResponse.capabilities`, the agent cannot enforce
  it (`step_resources::unenforceable`: cgroups disabled, spurd not root, or
  the specific resource's cgroup knob off), or it is a `--pty`/`srun --jobid`
  attach.
- **Two-phase claim.** A typed multi-node step claims its share on every node
  (`ClaimStep`) before any node runs it, so a node that is busy never leaves
  another node running while srun retries. The claim reserves only; `run_command`
  takes it right after admission, before any replay/duplicate-request check, so a
  re-sent `RunCommand` for a live or settled step never holds a second claim for
  the TTL. The controller sizes the TTL per step (`ClaimStepRequest.ttl_secs =
  2 * dispatch deadline + 30s`, `0` = agent default); the agent clamps it to
  `[STEP_CLAIM_TTL (120s), MAX_STEP_CLAIM_TTL (1800s)]`. A `RunCommandRequest`
  that arrives `claimed` but holds no claim (TTL expired, or `spurd` restarted
  between phases) returns `FailedPrecondition` instead of silently reserving a
  single-phase share -- a non-busy refusal, so the controller cancels the step's
  other nodes at once rather than waiting out their time limit.
- **Busy share.** If the step does not fit beside the job's other live typed
  steps on some node, the agent returns `Status::resource_exhausted` carrying
  the `spur-launch-refusal: step-share-busy` trailer; `spur-cli`'s srun retries
  with Slurm's `step creation temporarily disabled, retrying` message
  (`BusyStepWait`) rather than failing outright. A cancel tombstone recorded
  for the step (`LaunchAcceptance::is_cancelled`) refuses a later `RunCommand`
  or `ClaimStep` with "step J.S was cancelled" even without execution
  credentials; a plain busy-retry release (`ClaimStepRequest{release: true}`)
  never tombstones, since srun retries the same step id after one. The
  controller's `run_step` only leaves a busy step `Running` for a waiting
  retry when `RunStepRequest.wait_if_busy` is set (new `spur-cli` sets it);
  otherwise it records the step `Cancelled` immediately so a client that
  never retries does not leave it `Running` until the job ends.
- **OOM vs cancel.** A step killed by its own memory limit reports
  `oom_killed` on the `RunCommandResponse`/`RunStepResponse` (ends
  `OUT_OF_MEMORY`); a client giving up on a step that never ran reports
  `cancelled` on `CompleteJobStepRequest` (ends `CANCELLED`).
- **An `--overlap` step** is limited like any typed step but holds nothing
  (shares live GPUs/CPUs with the job and other overlap steps, never counted
  against the job's share).

---

## Container Runtime

Native container support without Docker, containerd, or any external daemon.
Inspired by Enroot.

- **Image format**: squashfs (same as Enroot). Import OCI/Docker images with
  `spur image import`.
- **Image search path**: `$SPUR_IMAGE_DIR` > `/var/spool/spur/images` >
  `~/.spur/images/` (job user's home) > agent's `~/.spur/images/`.
- **Isolation**: mount + PID namespace (`NEWNS|NEWPID`) when running as root;
  user namespace added only for rootless operation. pivot_root into extracted
  squashfs rootfs. Overlay for writable layers.
- **GPU passthrough**: AMD = bind-mount `/dev/kfd` + `/dev/dri/renderD*` + ROCm
  libs. NVIDIA = bind-mount `/dev/nvidia*` + driver libs.
- **Bind mounts**: `ContainerConfig.mounts` (source:target:ro/rw).
- **Process model**: raw `fork()` (not tokio::Command) with the container init
  done between fork and exec. PID tracked via `pidfd_open` on kernels >= 5.3.
- **Rootfs dirs**: unnamed `job_<N>` / `step_<N>_<M>` (step ids from 0), named
  ones at the sanitized name, under `container_dir()` (`$SPUR_CONTAINER_DIR` >
  `/var/spool/spur/containers` > `$HOME/.spur/containers`). Normal teardown
  (`cleanup_rootfs`) removes them; leftovers from a crash are removed by the
  startup orphan pass, which unmounts layers first (no lazy detach) and keeps a
  rootfs held as a process root or bound anywhere (per-namespace mountinfo).
- **Overlay mode currently always falls back to extraction**: `upper` is a
  tmpfs but `work` sits on the base filesystem, which overlayfs rejects, so
  every rootfs is a full unsquashfs tree.

---

## k0s/K8s Integration

spurd can manage an embedded k0s Kubernetes worker (or controller/single) via
a systemd unit. This is RPC-driven by spurctld's `cluster_k8s` module.

### Design principles
- k0s runs as its own systemd unit, NOT as a spurd child. This means:
  - It survives spurd restarts.
  - It stays out of the executor/monitor/time-limit job path.
- Recovery is split: systemd `Restart=always` handles process crashes; the
  spurd reconcile loop (every 30s) handles stopped/disabled/deleted/drifted
  units.
- The unit uses `Type=simple` (k0s does NOT sd_notify), `KillMode=process`
  (leaves containerd/kubelet/pod cgroup alone on stop), `Delegate=yes`.

### Lifecycle
- `K0sAgent::start()`: auto-installs k0s if missing, writes config + join
  token to `/etc/k0s/`, renders the systemd unit, enables, starts.
- `K0sAgent::stop()`: disables the unit, optionally runs `k0s reset` +
  purges the unit file and token.
- `K0sAgent::adopt_running_unit()`: on spurd restart, probes for an active
  spurd-owned k0s unit, parses its ExecStart to rebuild ClusterSupervisor.

### GPU pods
Before starting a worker/single node, spurd writes an AMD GPU CDI spec to
`/etc/cdi/amd.json` so k0s's containerd can inject `amd.com/gpu` into pods.

### Storage
Control-plane nodes ship a local-path storage provisioner manifest to
k0s's manifest-deployer directory.

---

## Device Management

### Device discovery
`spur-devices` crate provides the `DeviceRegistry`, populated from:
- **CDI cache**: scans CDI spec directories for device specs (AMD GPUs via
  `/dev/kfd`, `/dev/dri/renderD*`, sysfs topology).
- **GRES config**: `[devices].gres` entries in `spur.conf` (Slurm GRES parity).

### GPU injection
When a job is allocated GPUs, the agent resolves `stable_id` values to
physical devices via the registry and builds:
- **`HostInjectionPlan`** for native jobs: device paths + environment vars.
- CDI-based container device injection.
- cgroup device filter rules (BPF program).

### cgroup-v2 device control (`device_cgroup.rs`)
Since cgroup-v2 dropped the `devices` controller, device access is enforced
via `BPF_PROG_TYPE_CGROUP_DEVICE` programs built and loaded in-process (no
eBPF toolchain needed). Each job gets:
- Base rules: `/dev/null`, `/dev/zero`, `/dev/full`, `/dev/random`,
  `/dev/urandom`, `/dev/tty`, `/dev/console`, `/dev/ptmx`, `/dev/pts/*`.
- Host infra: `/dev/fuse`, `/dev/infiniband/*`, `/dev/nvidia-caps/*`.
- Per-allocation GPU device nodes.

### Periodic inventory refresh
A background loop re-discovers devices every interval, classifies changes
against the live allocation (debounced: must be seen twice), and converges
the controller via re-registration. Guards against suspect full-GPU dropouts
(likely a loader error, not a real change).

---

## Health Checks

### Heartbeat as health signal
The heartbeat itself is the primary liveness signal: if the controller does
not hear from a node within its configured timeout, it fences the node.

### Node-fault drain
`LaunchError::NodeFault` (an I/O failure in spurd's own spool tree) triggers
the agent to self-drain, preventing further job dispatches to a broken node.
A prolog failure also drains, but the controller drives that.

### cgroup OOM detection
`executor::cgroup_oom_killed` checks the cgroup's `memory.events` for
OOM kills. The reaper loop reads this on job exit to annotate the completion.

### k0s health
`K0sNodeState` reports unit_active, restart_count, and a one-shot
install_ms to the controller each heartbeat (surfaced as `spur_k8s_node_*`
metrics).

### Inventory suspect-scan guard
`is_suspect_gpu_dropout` refuses to converge a discovery that went from
>0 GPUs to 0 (likely a KFD/CDI loader error, not real hardware loss).

---

## Common Pitfalls

### Build and test
- spurd has both `[[bin]]` and `[lib]` targets. The lib is consumed by
  `spur-stepd`, so changes to public interfaces in lib.rs modules affect
  both crates.
- `agent_server.rs` is ~25k lines. When searching for a handler, look for
  `async fn launch_job`, `async fn cancel_job`, etc. inside
  `impl SlurmAgent for AgentService`.
- Tests that exercise cgroup/namespace/seccomp/landlock require Linux and
  often root. They are gated behind `#[ignore]` or env-var checks
  (e.g. `SPUR_CLUSTER_DERISK=1`).

### Daemon behavior
- spurd raises its own `RLIMIT_MEMLOCK` at startup (for BPF on kernels
  < 5.11). This happens before anything inherits the limit.
- Config file at `/etc/spur/spur.conf` is optional at the default path.
  An explicitly specified `--config` path that is missing IS an error.
- The `state_dir` (`/var/spool/spur` by default) is shared between spurctld
  and spurd. spurd's own runtime state lives in `<state_dir>/runtime/`.
- On SIGTERM with held stepds, spurd does NOT deregister -- it preserves
  registration so the controller does not fence running jobs.
- Startup spool cleanup only deletes after an *authoritative* GetJobs answer;
  an older controller never sets `authoritative`, so nothing is deleted until
  controllers are upgraded. Absence counts only below `next_job_id`.
- The spool and cgroup roots are fixed paths, so two spurd instances of
  different clusters on one host can delete each other's leftover spools.
  Isolated test instances need a private mount namespace for `/var/spool`.
- The startup pass skips with cgroups disabled and treats any unreadable
  `/proc` entry as "keep everything" for its batch; reading other processes
  needs `CAP_SYS_PTRACE`.

### Job execution
- Jobs are forked as their own process-group leaders. Signals go to the
  negative PID (the whole group), not just the tracked process.
- Container jobs use raw `fork()`, not `tokio::Command`. The PID is tracked
  via `pidfd_open` on kernels >= 5.3 to prevent PID recycling races.
- `JobLifecycle` serializes setup/teardown per job ID. Without it, a
  re-dispatch can race the previous run's cgroup/spool cleanup.
- `AllocationOnly` RunningJob holds a companion node's slice of an allocation
  without running user code. It must NOT be PID-namespaced (it would become
  PID 1 and drop unhandled signals).

### Core layout and typed steps
- The controller delivers a node's `CoreLayout` on registration only; a
  layout change reaches a running agent through a controller-requested
  re-register (`record_layout_heartbeat`), not a live push. Mid-flight
  repinning of already-running jobs is out of scope.
- `pick_step_cpus` pins a typed step's CPUs only when enough of the job's
  free CPUs are near its GPUs; otherwise it returns an empty pick and the
  step falls back to sharing the job's cpuset by count, not by specific
  cores. This is a silent degrade, not an error.
- `PendingStepShares` covers only the window between reserving a share and
  handing it to a supervisor; it is in-memory, unpersisted, per agent
  process. Once a supervisor exists, its stepd descriptor is the share's
  record, so a step already running survives an agent restart and is
  re-adopted with its limits intact; only a step still launching when spurd
  crashes loses its reservation (the launch itself did not complete either).

### stepd sessions
- The Unix socket protocol is line-delimited JSON, NOT framed protobuf.
- Session directories are named `<job_id>.<run_attempt>.<step_id>`.
- The agent socket (`agent.sock`) is separate from per-session sockets.
  Completion notifications flow over the agent socket; control messages
  (signal, query, shutdown) flow over the session socket.
- A restarted supervisor writes a fresh Hello with its new instance ID;
  the agent must re-validate it.

### k0s integration
- k0s units are identified by a marker string `"(managed by spurd)"` in the
  Description field. Changing this string would disown all existing units.
- `adopt_running_unit` parses ExecStart back into ClusterConfig. The
  render-parse roundtrip must be byte-identical or reconcile rewrites the
  unit every tick.
- `k0s reset` is destructive and host-wide. It is only run after confirming
  the unit is stopped. A token minted against a torn-down CA must be purged
  even if reset fails.

### Security
- Default-deny for root execution: `allow_root_jobs` must be explicitly set
  in `[auth]`. Without it, a uid-0 job spec is refused.
- The agent gRPC surface requires a valid credential when
  `auth.mode = "required"`. Under `plugin = "spur"`, only native
  ControllerRpc/UserRpc credentials are accepted (Ed25519 + HMAC JWKS);
  under `plugin = "jwt"`, HS256 JWTs signed with jwt_key. Without the
  auth middleware, reaching port 6818 is enough to run arbitrary work on
  the node.
- Under `plugin = "jwt"`, the key is a symmetric shared secret (like
  Slurm's munge.key). A compromised node can impersonate the controller
  to another node.
