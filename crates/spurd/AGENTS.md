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
12. Replay unacknowledged stepd completions, spawn retry background task.
13. Build `AgentService`, adopt recovered stepds, replay allocations, settle
    stale stepds.
14. Bind agent-socket Unix listener for supervisor completion notifications.
15. Adopt any running spurd-owned k0s unit, spawn the k0s supervise loop.
16. Start the reaper/monitor loop (`start_monitor`).
17. Spawn periodic inventory-refresh loop (debounced, seen-twice).
18. Start the gRPC server (with auth middleware).
19. Report recovered/stale sessions to the controller.
20. Wait on SIGTERM or server exit.

### Heartbeat protocol
`NodeReporter::heartbeat_loop` runs every 30s (hardcoded). Each heartbeat:
- Reads free memory, CPU load from `/proc/meminfo` and `/proc/loadavg`.
- Re-reads the WireGuard public key from the mesh interface.
- Collects held job IDs from the running-jobs map.
- Collects per-job GPU stable_ids from the NodeAllocation.
- Reads k0s node state (unit_active, restart_count, one-shot install_ms).
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
  triggers begin_teardown internally — cgroup-first SIGTERM, sent once per run:
  a repeated Shutdown during teardown sends nothing more unless the first
  reached no process). Unsupervised steps and jobs follow the same once-only
  rule in `graceful_cancel`; explicit signals are always delivered.
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
