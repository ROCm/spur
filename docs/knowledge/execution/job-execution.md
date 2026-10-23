# Job Execution Pipeline

How a job travels from the controller's dispatch decision to a running process
on a compute node, and how its exit propagates back.

## 1. Dispatch: controller to agent

The controller (`spurctld`) sends a `LaunchJob` gRPC to the target node's
agent (`spurd`). The request carries:

- `LaunchJobRequest.spec` -- the user's `JobSpec` (script, env, container
  image, uid/gid, resource request).
- `allocated` -- the controller's binding decision (CPU ids, GPU stable_ids,
  memory).
- `run_attempt` -- monotonically increasing epoch; lets the agent fence a
  stale launch that lost its ack.
- `execution_credential` -- Ed25519-signed credential the agent verifies before
  touching any resource. Covers job id, node, attempt, uid/gid, command
  digest, and the allocated slice. (HMAC is used for user-RPC credentials,
  not execution credentials.)
- `peer_nodes`, `task_offset`, `pmix_plan` -- multi-node topology.

The agent validates the credential (`native_exec::verify_execution`), refuses
root uid unless `allow_root_jobs` is set, and confirms the request targets
this hostname.

## 2. Agent-side launch

### Lifecycle serialization

A per-job `lifecycle` mutex is acquired before any work begins. It is held
until the job is tracked in the `running` map (or cleaned up on failure), so a
retried dispatch of the same id cannot build on top of a half-torn-down
previous attempt.

### Idempotent retry

If a `stepd` supervisor for this exact `(job_id, step_id, run_attempt)` is
already tracked and alive, the agent returns success without re-allocating --
the controller lost the ack and retried.

### Fencing

Before launching, the agent fences any displaced supervisor from a prior
`run_attempt`:

1. SIGTERM the old `spurstepd` process (confirmed live via pid + start-ticks).
2. Wait for the old cgroup to empty (or SIGKILL the workload tree if no
   cgroup was recorded).
3. Remove the tracked entry only if it still points at the displaced session.

### Resource allocation

`allocate_local_resources` runs under the allocation lock:

- CPU cores are pinned from the `NodeAllocation`.
- GPU stable_ids are resolved through the `DeviceRegistry`. The registry
  builds both a `HostInjectionPlan` (env vars for non-container jobs) and a
  `ContainerDevicePlan` (bind-mount list for container jobs).

### Environment

`SpurEnv` builds the job environment with Slurm-compatible twins
(`SPUR_JOB_ID` + `SLURM_JOB_ID`), plus Spur-only vars (`SPUR_NODE_RANK`,
`SPUR_CONTROLLER_ADDR`). The agent's own binary directory is prepended to
`PATH` so `srun` works inside batch scripts.

### Script construction

The user's script is wrapped depending on the launch shape:

| Shape | Condition | Wrapper |
|---|---|---|
| `Script` | single task, no MPI | raw script |
| `FanOut` | `tasks_per_node > 1` or PMIx | `build_multi_task_wrapper` (non-PMIx) or `build_multi_task_pmix_wrapper` (PMIx) |
| `LoneRank` | single PMIx rank on this node | `build_single_task_wrapper` |
| `Mpirun` | `--mpi=mpirun` | `build_mpi_mpirun_wrapper` on rank 0; hold script on companions |

For `--mpi=pmix`, the per-rank wrapper is deferred to the supervisor
(`start_supervised_pmix`) because only the PMIx server process can hand out
each rank's bootstrap environment.

### Prolog hooks

The node prolog (`[hooks] prolog`) runs as root before the workload. A failure
returns `LaunchFailureProlog` to the controller, which owns the drain + hold
decision.

## 3. Supervised launch (stepd)

Every production launch is supervised. The agent serializes a
`StepdLaunchSpec` to `<session_dir>/launch.json`, then spawns `spurstepd`.

### Double-fork spawn

`spawn_stepd_process` follows Slurm's `slurmstepd` pattern:

```text
spurd (parent)
  fork() -> intermediate child
               fork() -> grandchild (setsid, becomes spurstepd)
               _exit(0)          -- orphans grandchild to init
  waitpid(intermediate)          -- reap zombie
  read(pipe) <- grandchild pid   -- 4 bytes, ne
  poll(pipe) <- exec status      -- FD_CLOEXEC: EOF = success, 1 byte = failure
```

The grandchild calls `setsid()` to become a session leader, then `execv`s
the `spurstepd` binary. This decouples the supervisor's lifetime from the
agent's -- an agent restart does not kill running jobs.

### Readiness and start gate

After spawn, the agent waits for the Unix socket at
`<session_dir>/runtime.sock` to appear. The supervisor then holds the
workload behind a start gate (`await_start`) until the agent sends a `Start`
control message -- this prevents the batch script's first `srun` from racing
the agent's bookkeeping.

## 4. Executor: process launch

`executor::launch_job` (called by the supervisor) does the actual fork.

### Cgroup setup

`setup_cgroup` creates `/sys/fs/cgroup/spur/job_<id>_<attempt>/`:

1. Delegates controllers (`+memory +cpu +pids +cpuset`) on the spur root.
2. Creates the job directory, reaping any stale occupant.
3. Writes limit files: `memory.max`, `cpuset.cpus`, `pids.max`.
4. Steps that do not own the job's lifetime join the existing job cgroup as a
   child leaf rather than creating a new one.

If cgroup setup fails and `[cgroup] required = false` (the default), the job
runs without isolation and a warning is logged.

### Device cgroup filter

`device_cgroup.rs` builds a BPF_PROG_TYPE_CGROUP_DEVICE program in-process --
no eBPF toolchain needed. The program:

- Default-denies all device access.
- Allows base pseudo-devices (`/dev/null`, `/dev/zero`, `/dev/full`,
  `/dev/random`, `/dev/urandom`, `/dev/tty`, `/dev/console`, `/dev/ptmx`,
  `/dev/pts/*`).
- Allows host infra devices (`/dev/fuse`, `/dev/infiniband/*`,
  `/dev/nvidia-caps/*`).
- Allows only the allocated GPU render nodes (by major:minor from the
  `DeviceRegistry`).

The program is loaded with the `bpf()` syscall and attached to the job's
cgroup.

### Fork paths

**Non-container (tokio::Command):** the job runs as `/bin/bash <script>` (or
`/usr/bin/unshare --pid --mount --fork /bin/bash <wrapper>` when namespace
isolation is active). `Command::new()` inherits the cgroup via `pre_exec`.
Privilege drop uses `setpriv --reuid --regid --init-groups` inside the
namespace wrapper.

**Container (raw fork):** `launch_container_job` calls `nix::unistd::fork()`
directly. The child:

1. Wires stdio (`raw_io.wire()`).
2. Joins the cgroup (`write(cgroup.procs, self_pid)`).
3. Raises `RLIMIT_MEMLOCK` while still root.
4. Calls `container_init()` (see section 5).
5. Signals the parent "OK" over a sync pipe, then `execv`s `/bin/bash`.

The parent reads the pipe; "OK" means success, anything starting with "E:" is
an error message. A `pidfd_open` on the child prevents PID recycling on
kernels >= 5.3.

### IO modes

- **File:** stdout/stderr are opened as regular files (append or truncate per
  `--open-mode`). stdin is optional.
- **PTY:** `openpty()` creates a master/slave pair. The slave becomes the
  child's controlling terminal via `setsid + TIOCSCTTY + dup2`. The master fd
  is returned to the caller for interactive session forwarding.

### SPANK and TaskProlog

Before the workload spawns, SPANK `Init` + `TaskInit` hooks run in the
supervisor process (root context). Then `TaskProlog` (if configured) runs as
the job user inside the step cgroup. Its `export`/`unset` edits fold into the
workload's environment; its `print` output is written to stdout before the
workload starts.

## 5. Container execution

Spur's container runtime is native -- no Docker, containerd, or external
daemon.

### Image format

Images are squashfs (`.sqsh`), the same format Enroot uses. OCI/Docker images
are imported via `spur image import`, which pulls from registries using the
Docker Registry HTTP API v2 and calls `mksquashfs`.

Image search order: `$SPUR_IMAGE_DIR` > `/var/spool/spur/images` >
`~/.spur/images` (job user, then agent).

### Rootfs setup

`setup_rootfs` extracts (or loop-mounts) the squashfs into
`/var/spool/spur/containers/job_<id>/`. The job script is copied into
`<rootfs>/tmp/spur_job_<id>.sh`.

### container_init sequence

Called post-fork in the child, before exec:

1. Resolve supplementary GIDs (while host `/etc/group` is still visible).
2. `unshare(CLONE_NEWNS | CLONE_NEWPID)` (root) or set up a user namespace
   (rootless).
3. Set mount propagation to private.
4. Fork into the PID namespace (`fork_into_pid_namespace`).
5. Mount filesystems (`/proc`, `/sys`, `/dev`, `/dev/shm`, `/dev/pts`).
6. Apply container device plan (bind-mount GPU nodes, ROCm libs).
7. Set up `/etc/shadow`, user bind mounts, DNS mounts, home mount.
8. Run OCI hooks (`environ.d`).
9. `pivot_root` into the rootfs.
10. Drop privileges (`setuid/setgid` + supplementary groups).

### GPU passthrough

For AMD GPUs, the container device plan bind-mounts:

- `/dev/kfd` (KFD compute interface)
- `/dev/dri/renderD<N>` (only the allocated render nodes)
- ROCm userspace libraries from the host

The `ROCR_VISIBLE_DEVICES` environment variable is set to the re-indexed
device ordinals (0-based within the allocation).

## 6. Device isolation

GPU isolation operates at three layers:

**BPF device cgroup (kernel enforcement):** the eBPF program attached to the
job's cgroup denies `open()` on any char device not in the allow-list. This is
the hard boundary -- even a root-capable container cannot access unallocated
GPUs.

**Namespace mount filtering (non-container):** the namespace wrapper
(`build_namespace_wrapper`) replaces `/dev/dri` with a tmpfs and copies back
only the allocated render nodes. Jobs see only their GPUs in the filesystem.

**Environment (userspace cooperation):** `ROCR_VISIBLE_DEVICES` (AMD),
`GPU_DEVICE_ORDINAL`, and `HIP_VISIBLE_DEVICES` are set so well-behaved
runtimes (ROCm, PyTorch) see the correct ordinals. Zero-GPU jobs get a deny
sentinel (`gpu_deny_visibility`) to prevent inheriting all-visible defaults.

## 7. Completion reporting

### Supervisor-side

After the workload exits, the supervisor (`spurstepd`) follows a durable
obligation log (`obligations.jsonl`):

1. `ExitObserved { exit_code, signal }` -- recorded before cleanup so a crash
   in the epilog does not erase the exit status. If the cgroup shows
   `oom_kill > 0`, the signal gets the `OOM_SIGNAL_FLAG` bit.
2. TaskEpilog runs as the job user inside the (still-live) step cgroup.
3. Cgroup cleanup (`cleanup_cgroup` -- recursive SIGKILL + rmdir).
4. Container rootfs cleanup.
5. Job spool cleanup.
6. Node epilog (`[hooks] epilog`) runs as root.
7. SPANK `TaskExit` + `JobEpilog` hooks.
8. `EpilogCompleted { failed }`.
9. `StepdCompleted` notification sent to the agent over the Unix socket at
   `<state_dir>/runtime/agent.sock`.
10. `CompletionAcknowledged` -- the agent forwarded the exit to the controller.
11. `ResourcesReleased` -- the agent released the allocation.

Obligations are appended to the JSONL log after their actions complete. On
recovery, `replay_unacknowledged_stepd_completions` scans the log and retries
completion reports that were not acknowledged, rather than replaying the
cleanup/hook sequence from a checkpoint.

### Agent-side

On receiving `StepdCompleted`, the agent:

1. Calls `ReportJobStatus` to the controller (with retry).
2. Releases the local resource allocation.
3. Removes the job from the `running` map.
4. Acknowledges back to the supervisor, which prunes its session directory.

If the controller is unreachable, the completion is deferred -- the durable
obligation log survives, and the next agent startup replays it.

## 8. Multi-node jobs (PMIx / MPI)

### PMIx dispatch

For `--mpi=pmix` jobs, the controller sends a `PmixLaunchPlan` alongside the
`LaunchJobRequest`. The plan contains:

- `peer_hosts` -- every node in the job.
- `local_procs` -- this node's rank-to-task mapping.
- `universe_size` -- total ranks across all nodes.

The supervisor starts its own PMIx server
(`MpiPluginHost::start_pmix_server`), independent of the agent. Each rank's
bootstrap environment (`PMIX_SERVER_URI`, `PMIX_RANK`, etc.) is built by
`pmix_setup_fork_env_for_node_tasks` and folded into a per-rank wrapper script.

### mpirun mode

For `--mpi=mpirun`, rank 0 gets a wrapper that invokes `mpirun` with the
user's command; companion nodes run a hold script that keeps the allocation
alive until the job completes.

### Step launch (srun inside batch)

An `srun` inside a batch script sends `RunStep` to the controller, which
dispatches `RunCommand` (via `RunCommandRequest`) to each participating node's
agent. Each node's agent spawns a
separate `spurstepd` for the step, which joins the batch job's cgroup as a
child leaf. The step's supervisor manages its own PMIx server for the step's
ranks.

## 9. Interactive sessions

### salloc (allocation-only)

`salloc` creates an allocation with `pty = true`. The agent launches an
allocation-only supervisor (`STEP_EXTERN`) -- no workload process, just a
held cgroup and resource reservation. The user's shell runs client-side.

### srun --pty (interactive step)

An interactive `srun` spawns a step with `io_mode = Pty`. The supervisor
opens a `pty(master, slave)` pair. The slave becomes the workload's
controlling terminal; the master fd is deposited with the supervisor via
the PTY custody protocol.

### PTY custody protocol

The supervisor listens on `<session_dir>/ptyfd.sock` for custody messages
using Unix `SCM_RIGHTS` fd passing:

| Opcode | Direction | Purpose |
|---|---|---|
| `D` (Deposit) | agent -> stepd | Store a dup of the master fd, keyed by session_id |
| `R` (Reclaim) | agent -> stepd | Retrieve a specific session's master |
| `A` (Reclaim Any) | agent -> stepd | Retrieve any orphaned master (recovery) |
| `X` (Release) | agent -> stepd | Terminal closed; stop holding the master |
| `O` (Found) | stepd -> agent | Reply with the master fd attached |
| `N` (Absent) | stepd -> agent | No matching master held |

This lets a terminal survive agent restarts: the supervisor holds the master
fd, and a restarted agent reclaims it via `CUSTODY_RECLAIM_ANY`.

## 10. Recovery after agent restart

On startup, the agent scans `<state_dir>/` for session directories:

1. Loads each `descriptor.json` to find live supervisors.
2. Validates liveness via `pid + process_start_ticks` (from
   `/proc/<pid>/stat`).
3. Re-adopts live sessions into the `running` map and `stepds` map.
4. Replays pending obligations from `obligations.jsonl`.
5. Translates legacy positional GPU ids to stable_ids if the descriptor
   predates the current scheme.
6. Reports held jobs to the controller via heartbeat so it can reconcile.

The supervisor is unaffected by the restart -- it runs in its own session
(via `setsid`), and the agent reconnects to its Unix socket.

## 11. Common pitfalls

**Cgroup root not delegated.** If `/sys/fs/cgroup/spur` cannot delegate
controllers (common in rootless dev runs), jobs run without memory/CPU
isolation. Set `[cgroup] required = true` to fail-fast instead.

**NFS root_squash.** The job script and namespace wrapper are written to
`/var/spool/spur/` (node-local), not the user's `work_dir`, because root
writes to an NFS root_squash mount silently fail.

**Stale cgroup from a previous run.** `claim_cgroup_dir` reaps (SIGKILL +
rmdir) any leftover processes before creating the new cgroup. A step's
cgroup is a child leaf, so reaping it cannot kill siblings.

**PTY buffer overflow.** TaskProlog `print` output to a PTY uses
non-blocking writes (`write_fd_nonblocking`) to avoid stalling the launch
when no reader is attached yet. Output exceeding the terminal buffer is
silently truncated.

**PMIx server ownership.** The PMIx server runs inside the supervisor, not
the agent. An agent restart does not disrupt the rendezvous. But a
supervisor crash takes the server down, and all ranks lose their connection.

**Container image not found.** Image resolution searches multiple
directories. A missing image produces a `failed_precondition` error before
any resource is allocated. Use `spur image import` to pull and convert.

**RLIMIT_MEMLOCK.** RDMA and GPU workloads need unlimited memlock. The
supervisor raises it while still root (`apply_memlock`), before privilege
drop. A non-root spurd can only raise soft to hard, which may be
insufficient.

**Zombie intermediate child.** The double-fork pattern creates an
intermediate child that exits immediately. The parent `waitpid`s it
synchronously to prevent zombie accumulation.

**Device filter on step vs job.** The BPF device filter is attached to the
job's cgroup, not each step's. A step inherits the job's filter by joining
the job cgroup as a child. Creating a step cgroup on a missing job cgroup
is refused when `[cgroup] required = true`.
