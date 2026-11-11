# Spur Test Framework

## 1. Architecture overview

Spur uses two test tiers that cover the codebase at different levels:

| Tier | Language | Location | Daemons required | Purpose |
|---|---|---|---|---|
| **In-process** (unit/component) | Rust | `crates/spur-tests/` | No | Core types, scheduler logic, resource accounting, auth, GRES parsing. Runs entirely in-process with no network or running daemons. |
| **E2E native-host** | Python (pytest) | `tests/native_host/e2e/` | Yes (SSH to test VMs) | Full cluster lifecycle: deploy daemons over SSH, submit real jobs via the CLI, assert on output, tear down. |
| **E2E Kubernetes** | Python (pytest) | `tests/k8s/e2e/` | Yes (kubeconfig) | Spur operator, SpurJob CRDs, Raft HA under K8s, cross-namespace isolation. |

The in-process tests share no state with the E2E tests. They exercise
`spur-core`, `spur-sched`, `spur-proto`, and `spur-spank` crate APIs directly
and finish in seconds. The E2E tests deploy a real Spur cluster on remote VMs
(native-host) or inside a Kubernetes namespace (K8s) and take minutes.

## 2. In-process tests (Rust)

### 2.1. Crate and entry point

All in-process tests live in `crates/spur-tests/`. The crate root
(`src/lib.rs`) re-exports 18 test modules and `harness.rs`.

```bash
cargo test -p spur-tests            # all modules
cargo test -p spur-tests t39_gpu    # one module
cargo test -p spur-tests t39_1_parse_gpu_full   # one test
```

Dependencies: `spur-core`, `spur-sched`, `spur-proto`, `spur-spank`, `chrono`,
`jsonwebtoken`. No async runtime.

### 2.2. Test numbering convention

Test modules mirror Slurm's testsuite numbering where a Slurm equivalent
exists. Spur-specific groups start at 50. Within a module, individual tests
are named `tNN_M_description` where NN is the group and M is the sub-test
number. This keeps `cargo test` output greppable and maps to Slurm test IDs.

### 2.3. Module catalog

#### Slurm-equivalent groups (t01 -- t39)

| Module | Area | Slurm equivalent | What it tests |
|---|---|---|---|
| `t01_run` | srun / job steps | test1.x | Task distribution (block, cyclic), CPU/GPU binding parse, consumable TRES allocation and release, step state machine, `LOCAL_RANK` computation, GPU partitioning across tasks, `SPUR_JOB_ID` env detection. |
| `t05_queue` | squeue | test5.x | Job state classification, filtering, sort, queue visibility. |
| `t06_cancel` | scancel | test6.x | Hold, release, cancel, state transitions. |
| `t07_sched` | scheduling | test7.x | `BackfillScheduler` against `ClusterState`: single/multi-job scheduling, insufficient-resource rejection, priority ordering, `NodeTimeline` reservation windows, partition-scoped scheduling. |
| `t17_submit` | sbatch | test17.x | `test_script()` output with `#SBATCH` directives, `JobSpec` defaults (`num_nodes=1`, `cpus_per_task=1`, `requeue=false`), auto-incrementing job IDs, submit time, custom priority, hold flag, array spec, dependency storage, partition validation fields (`max_nodes`, `allow_accounts`, `deny_accounts`, `preempt_mode`, `max_time`), requeue/exclusive flags, license GRES format. |
| `t21_acctmgr` | sacctmgr | test21.x | TRES records, QOS limits, accounting data models. |
| `t24_priority` | priority | test24.x | Priority calculation, fair-share tree. |
| `t28_arrays` | job arrays | test28.x | Array spec parsing (`0-99%10`), task ID assignment, path resolution. |
| `t28b_array_expand` | array expansion | -- | Complement to t28: edge cases in array spec expansion. |
| `t39_gpu` | GPU / GRES | test39.x | `parse_gres()` for `gpu:mi300x:8`, `gpu:4`, `gpu` bare forms. `ResourceSet::can_satisfy()` for GPU type matching (accept, reject too-many, reject wrong-type, accept "any" type). Constraint scheduling via `BackfillScheduler` with feature lists (`mi300x`, `nvlink`), multi-feature `AND` requirement. |

#### Spur-specific groups (t50 -- t60)

| Module | Area | What it tests |
|---|---|---|
| `t50_core` | Core types | Job state machine (Pending -> Running -> Completed/Failed/Cancelled/Timeout/NodeFail/Preempted/Suspended, requeue from terminal states, admin-guarded `requeue_to_pending`). Node states (Idle/Mixed/Allocated from allocation, Drain/Draining/Error/Suspended preserved by `update_state_from_alloc`). `ResourceSet::can_satisfy()` and `can_satisfy_with_allocated()`. GRES parsing. Partition states. Path resolution (`%j`, `%x`, `%u`, `%N`). Run time computation. Het-job fields. Step constants (`STEP_BATCH=0xFFFF_FFFB`). PMIx env vars. Federation config TOML roundtrip. Reservation field. Container image fallback. Scheduler edge cases (`num_nodes=0`). K8s address resolution priority. |
| `t51_hostlist` | Hostlist | Expansion (`node[001-010]`) and compression. |
| `t52_config` | Config | TOML loading, time parsing, partition building. |
| `t55_format` | CLI format | `config::format_time`, `JobState`/`NodeState`/`PendingReason` display strings. |
| `t56_deps` | Dependencies | Dependency spec parsing (`afterok:100`, `afterany:200`) and satisfaction checking. |
| `t57_auth` | Auth | JWT token generate/verify, admin flag, wrong-secret rejection, expired-token rejection, authorization checks (own-job, admin). |
| `t58_spank` | SPANK | Plugin loading, hook invocation, plugstack config parsing. |
| `t60_suspend` | Suspend/resume | Suspend/resume guards, run-time accounting adjustments. |

### 2.4. The harness (`harness.rs`)

The harness provides fixture builders. It uses no daemon processes and no
network. All fixtures create in-memory Rust structs.

| Function | Purpose |
|---|---|
| `make_job(name)` | Job with auto-incrementing ID (global `AtomicU32`), user `testuser`, partition `default`, 1 node / 1 task / 1 cpu, work_dir `/tmp`. |
| `make_job_with_resources(name, nodes, tasks, cpus_per_task, time_limit)` | Same defaults but with explicit resource requirements. |
| `make_nodes(count, cpus, memory_mb)` | Batch of idle compute nodes named `node001`..`nodeNNN`, each in partition `default`. |
| `make_partition(name, node_count)` | Partition with a hostlist. `is_default=true` when `name == "default"`. |
| `assert_job_state(job, expected)` | Panic on mismatch with a descriptive message. |
| `assert_transition_ok(job, to)` / `assert_transition_err(job, to)` | State machine transition assertions. |
| `test_script(directives, body)` | Builds a `#!/bin/bash` script with `#SBATCH` directive lines. |

6 of 18 test files use the harness (t05, t06, t07, t17, t50, t60). The
remaining 12 exercise crate types directly without fixture helpers.

Scheduler tests build a `ClusterState` struct with nodes, partitions, and
pending jobs, call `sched.schedule()`, and assert on assignments.

### 2.5. Patterns

```rust
#[cfg(test)]
mod tests {
    use crate::harness::*;

    #[test]
    fn tNN_M_description() {
        let nodes = make_nodes(4, 64, 256_000);
        let partitions = vec![make_partition("default", 4)];
        let pending = vec![make_job_with_resources("train", 2, 64, 1, Some(60))];

        let cluster = ClusterState {
            nodes: &nodes,
            partitions: &partitions,
            // ...
        };
        let assignments = sched.schedule(&pending, &cluster);
        assert_eq!(assignments.len(), 1);
    }
}
```

No shared mutable state across tests (the atomic job ID counter is the only
exception). Tests are deterministic and independent.

## 3. E2E native-host tests (Python)

### 3.1. Environment variables

The E2E harness requires SSH access to test VMs. Core env vars:

| Variable | Required | Purpose |
|---|---|---|
| `SPUR_TEST_NODES` | Yes | Comma-separated list of node hostnames (or `user@host`). |
| `SPUR_TEST_SSH_USER` | Yes | Default SSH user for all nodes. |
| `SPUR_TEST_SSH_PASSWORD` | No | SSH password (also used for `sudo -S`). |
| `SPUR_TEST_SSH_KEY` | No | Path to SSH private key. |
| `SPUR_TEST_BINARIES_DIR` | No | Local directory with `spurctld`, `spurd`, `spur`, `spurstepd`, `spurauthd`. Defaults to `target/release/`. |
| `SPUR_TEST_REMOTE_BIN_DIR` | No | Fixed remote binary path (for AppArmor). Ephemeral if unset. |
| `SPUR_TEST_CONTROLLER_PORT` | No | Override controller port (default 6817). |
| `SPUR_TEST_AGENT_PORT` | No | Override agent port (default 6818). |
| `SPUR_TEST_HOSTNAME_STYLE` | No | `short` (default) or `fqdn`. |
| `SPUR_TEST_GPU_VENV` | No | Pre-existing venv with PyTorch on the nodes. |
| `SPUR_TEST_MPI_PLUGIN` | No | Path to `spur_mpi_pmix.so`. |
| `SPUR_TEST_WG_ADDRESSES` | No | Per-node WireGuard mesh addresses. |

### 3.2. Cluster lifecycle (`cluster.py`)

`SpurCluster` manages the full daemon lifecycle on remote nodes via
`paramiko` SSH:

1. **provision()** -- creates remote directories (`etc/`, `state/`, `log/`),
   resolves hostnames.
2. **start()** -- writes TOML config, kills stale daemons, starts `spurctld`
   then `spurd` on each node, polls `sinfo` until all nodes are `idle`.
3. **deploy()** -- `provision()` + `start()` in one call.
4. **stop()** -- kills daemons but leaves the working directory.
5. **teardown()** -- kills all daemons, removes the remote directory.

Binary upload (`ensure_bins()`) compares file sizes and skips re-upload when
unchanged. CLI symlinks (`sbatch`, `srun`, `squeue`, etc.) are created via
`ln -sf spur`.

Config is built from a default template (`_default_config()`) with
`deep_merge()` of caller-supplied overrides. The default config has:

- `cluster_name = "e2e-test"`
- scheduler interval 1s, backfill plugin
- auth plugin `none`
- a single `default` partition covering all nodes
- 64 CPUs, 256 GiB memory per node

### 3.3. Fixtures (`conftest.py`)

Fixtures are pytest-scoped and handle setup/teardown:

| Fixture | Scope | Purpose | Min nodes |
|---|---|---|---|
| `ssh_nodes` | session | SSH connections to all configured nodes | 1 |
| `remote_bin_dir` | session | Remote binary directory (ephemeral or fixed) | 1 |
| `_ensure_bins` | session (auto) | Uploads binaries to all nodes once | 1 |
| `cluster` | per-test | Default running cluster, torn down after each test | 1 |
| `unstarted_cluster` | per-test | Provisioned but not started; test calls `cluster.start()` | 1 |
| `multi_node_cluster` | per-test | Skips if < 2 nodes | 2 |
| `cgroup_cluster` | per-test | Rootful agent for cgroup enforcement; checks cgroup v2 | 1 |
| `gpu_cluster` | per-test | Skips if no GPU device nodes (`/dev/kfd`, `/dev/dri/card*`) | 1 |
| `gpu_pmix_cluster` | per-test | GPU cluster with MPI plugin deployed | 1 |
| `ha_cluster` | per-test | 3-controller Raft HA cluster with real election | 3 |
| `accounting_cluster` | per-test | Cluster with Postgres on node 0 (Docker container) | 1 |
| `label_cluster` | class | Cluster with node labels and partition selectors | 2 |
| `mpi_cluster` | per-test | Single-node MPI with PMIx plugin | 1 |
| `mpi_multi_node_cluster` | per-test | Multi-node MPI | 2 |
| `k8s_multicp_cluster` | per-test | k0s enabled, 3 nodes for etcd quorum | 3 |
| `k8s_calico_direct_cluster` | per-test | Single-CP k0s with calico CNI | 2 |
| `wg_k0s_cluster` | per-test | k0s over WireGuard mesh | 3 |
| `wg_login_cluster` | per-test | k0s-over-mesh with login-node reachability topology | 3 |
| `raw_wg_mesh` | per-test | WireGuard mesh without Spur daemons | 2 |

The `cluster_config_overrides` fixture can be overridden per test class to
customize the cluster config before `spurctld` starts.

GPU tests are auto-marked `gpu` by `pytest_collection_modifyitems` when they
use `gpu_cluster` or `gpu_container_cluster` fixtures, so CI can route them
with `-m gpu` without manual tags.

### 3.4. CLI wrappers

`SpurCluster` exposes wrappers that build remote command lines with correct
`PATH`, `SPUR_CONTROLLER_ADDR`, and `SPUR_CONF`:

- `cli(args)` / `cli_allow_fail(args)` / `cli_with_exit(args)`
- `cli_as_user(run_as, args)` -- sudo to a specific UNIX user for
  owner-vs-non-owner tests
- `sbatch(args)` / `srun_with_exit(args)` / `squeue(args)` / `scancel(job_id)`
  / `scontrol(args)` / `sinfo()` / `sacct(args)` / `sacctmgr(args)`
- `salloc_run(shell_body)` -- interactive allocation with custom shell
- `k8s_up()` / `k8s_down()` / `k8s_status()` -- native k0s wrappers

### 3.5. Job helpers

`cluster.py` provides utilities used by almost every test file:

| Function | Purpose |
|---|---|
| `parse_job_id(sbatch_output)` | Extracts the integer job ID from `"Submitted batch job 42"`. |
| `job_state(squeue_output, job_id)` | Parses the `ST` column from `squeue -t all` output. Handles truncated codes (e.g., `OO` -> `OOM`). |
| `wait_job(cluster, job_id, timeout)` | Polls `squeue -t all` until the job reaches a terminal state (`CD`, `F`, `CA`, `TO`, `NF`, `DL`, `OOM`). Returns the state code. |
| `wait_job_state(cluster, job_id, state)` | Polls until a specific state is reached (e.g., `R` for running). |
| `wait_node_state(cluster, node, states)` | Polls `sinfo` for a node state. |
| `wait_until(predicate, timeout_s)` | Generic polling helper with configurable tolerance for transient exceptions. |
| `wait_sacct_row(cluster, job_id, fmt)` | Polls `sacct` for a job's accounting row (asynchronous recording). |

### 3.6. E2E test file catalog

87 test files covering:

| Category | Files | Notes |
|---|---|---|
| **Basic job lifecycle** | `test_single_node.py`, `test_multi_node.py`, `test_sbatch_empty.py`, `test_sbatch_wait.py`, `test_time_limit.py` | Submit, run, complete, fail, custom output paths. |
| **Scheduling** | `test_exclusive_blocking.py`, `test_idle_fill.py`, `test_begin_time.py`, `test_partitions.py`, `test_held_job_blocks_node.py`, `test_node_labels.py`, `test_node_resource_caps.py` | Exclusive mode, backfill, deferred start, partition targeting. |
| **Preemption** | `test_preemption.py`, `test_preemption_modes.py`, `test_preemption_grace.py`, `test_preemption_fairshare.py`, `test_preemption_handoff.py`, `test_preemption_hoist.py`, `test_preemption_multinode.py`, `test_preemption_qos_hierarchy.py`, `test_preemption_same_qos.py`, `test_preemption_satisfiability.py`, `test_preemption_exempt_precedence.py` | Requeue, suspend, grace period, fair-share, multi-node. |
| **QOS / accounting** | `test_accounting.py`, `test_burst_qos.py`, `test_qos_preemption.py`, `test_qos_walltime_policy.py`, `test_spur268_fairshare_live.py`, `test_karma_counters.py`, `test_karma_time_limit.py`, `test_srun_qos.py` | TRES tracking, QOS limits, fair-share. |
| **GPU / devices** | `test_gpu.py`, `test_devices.py`, `test_device_isolation.py`, `test_gpu_deny.py`, `test_gpu_identity_change.py` | HIP tests, CDI auto-detect, GPU isolation, deny lists. |
| **Containers** | `test_container.py`, `test_srun_container.py` | squashfs rootfs, image import, Docker image env. |
| **MPI / RCCL** | `test_mpi.py`, `test_rccl.py`, `test_srun_multi_node.py` | PMIx plugin, multi-node collectives. |
| **Auth** | `test_auth_dispatch.py`, `test_salloc_step_auth.py`, `test_private_data.py`, `test_admission.py`, `test_job_submit_hook.py` | JWT dispatch, step auth, job_submit Lua hooks. |
| **Cgroups** | `test_cgroup_limits.py`, `test_cgroup_cleanup_quarantine.py`, `test_step_cgroups.py`, `test_old_attempt_cgroup_cleanup.py` | Resource limits, cleanup, quarantine. |
| **Steps** | `test_concurrent_steps.py`, `test_srun_step_output.py`, `test_srun_pty.py` | Overlapping steps, PTY sessions. |
| **HA / consensus** | `test_controller_failover.py`, `test_controller_graceful_shutdown.py`, `test_leadership_blip_health.py`, `test_raft_write_timeout.py` | Leader election, failover, blip recovery. |
| **Node lifecycle** | `test_deregistration.py`, `test_inventory_convergence.py`, `test_node_resume.py`, `test_suspend_resume.py`, `test_health_check.py`, `test_health_check_live.py` | Drain, deregister, suspend/resume, health. |
| **Requeue / recovery** | `test_requeue.py`, `test_requeued_run_reclaim.py`, `test_run_attempt_recovery.py`, `test_cancel_completing.py`, `test_completing_timeout.py`, `test_dispatch_abort_release.py` | Requeue logic, supervisor recovery. |
| **Supervisor** | `test_stepd_restart_survival.py`, `test_stepd_step_supervision.py`, `test_wedged_stepd_reclaim.py` | spurstepd survives spurd restart. |
| **WireGuard** | `test_wg_mesh.py`, `test_wg_k0s.py` | Mesh bring-up, k0s-over-mesh. |
| **k0s** | `test_k8s_multicp.py`, `test_k8s_membership.py`, `test_k8s_scheduling.py`, `test_k8s_calico_direct.py`, `test_k8s_auth.py`, `test_upgrade_k0s.py`, `test_upgrade_k0s_wg.py` | Native k0s cluster lifecycle. |
| **Prolog / epilog** | `test_prolog_epilog.py`, `test_core_spec.py`, `test_native_plugin.py` | Pre/post scripts, core spec reservation. |
| **Reservations** | `test_reservations.py`, `test_reserve_before_dispatch.py`, `test_inactive_limit.py` | Time-bounded reservations. |

## 4. GPU / device test patterns

### In-process (t39)

GPU tests in `t39_gpu.rs` exercise the GRES parser and resource matching
without hardware. They construct `ResourceSet` structs with `GpuResource`
entries and verify `can_satisfy()` logic for type matching, count limits,
and "any" type wildcards. Constraint scheduling tests build a
`BackfillScheduler`, a `ClusterState` with nodes tagged by features
(`mi300x`, `nvlink`, `h100`), and assert that the scheduler only assigns
jobs to nodes matching the `--constraint` spec.

### E2E (test_gpu.py, test_devices.py)

E2E GPU tests use the `gpu_cluster` fixture, which probes for
`/dev/kfd` and `/dev/dri/card*` on the nodes and skips if absent.
Tests:

- Compile and run a HIP fixture (`gpu_test.hip`) via hipcc on the node.
- Submit via `sbatch --gres=gpu:1`, wait for completion, assert `ALL PASS`.
- `test_devices.py` validates CDI auto-detect: the agent discovers GPUs at
  startup (`cdi_devices=N` in spurd logs) and exposes them in `scontrol show
  node` as GRES entries.
- `test_device_isolation.py` asserts that `ROCR_VISIBLE_DEVICES` is set
  correctly inside the job, restricting GPU visibility.

The cluster also provides helpers: `gpu_preflight(min_nodes)` for
prerequisite checks, `compile_hip_fixture()` for shipping and building HIP
source, `assert_spurd_registry()` for CDI log assertions, `node_gpu_count()`
for GRES count from `scontrol show node`.

## 5. K8s E2E tests

### 5.1. Infrastructure (`k8s_cluster.py`)

K8s E2E tests use the Python `kubernetes` client. `k8s_cluster.py` provides:

- `SuiteContext` -- session-level setup: verifies kubeconfig, loads manifests
  from `examples/k8s/`, creates the test namespace.
- `ClusterFixture` -- deploys a Spur cluster inside K8s by applying the
  operator manifests (StatefulSet for `spurctld`, DaemonSet for `spurd`).
  Supports `FixtureConfig.single_node()` and `FixtureConfig.raft_ha()`.
- `wait_spurjob_state()` -- polls a SpurJob CR until it reaches a given state.
- `simple_spurjob()` / `spurjob_with_env()` / `multinode_spurjob()` --
  factory functions for SpurJob CRD manifests.

### 5.2. Fixtures (`tests/k8s/e2e/conftest.py`)

| Fixture | Scope | Purpose |
|---|---|---|
| `k8s_suite` | session | Verifies kubeconfig, loads manifests, creates namespace. |
| `cluster` | class | Single-node cluster via operator manifests. |
| `ha_cluster` | class | Multi-replica Raft HA cluster. |
| `_cleanup_between_tests` | per-test (auto) | Deletes test workloads, ensures controllers are ready. |

### 5.3. Test files

| File | What it tests |
|---|---|
| `test_spurjob.py` | SpurJob CRD lifecycle (submit, complete, env passthrough, multi-node). |
| `test_raft_ha.py` | Raft HA under K8s: leader election, pod restart recovery. |

Constants: `SPUR_JOB_GROUP = "spur.amd.com"`, `SPUR_JOB_VERSION = "v1alpha1"`,
`SPUR_JOB_PLURAL = "spurjobs"`. The operator image defaults to
`ghcr.io/rocm/spur:ci` (overridable via env).

### 5.4. Environment

`SPUR_TEST_NS` overrides the namespace (defaults to `spur-test-{GITHUB_RUN_ID}`
or an ephemeral `spur-ci-{pid}-{ts}`).

## 6. Native-host k0s tests

Several native-host E2E tests exercise Spur's native k0s integration (the
ability to bring up a k0s Kubernetes cluster managed by the Spur controller).
These are distinct from the K8s operator tests above.

Fixtures:

- `k8s_multicp_cluster` -- 3-node etcd-quorum k0s cluster.
- `k8s_calico_direct_cluster` -- 2-node single-CP k0s with calico.
- `wg_k0s_cluster` -- k0s over WireGuard mesh.

These fixtures call `c.start(config_overrides={"cluster": {"enabled": True}})`,
then `c.k8s_up()` / `c.wait_k8s_phase("ready")` to bring k0s up. Teardown
uses `c.k8s_down(reset=True)` followed by a per-node `k0s reset` to ensure
no k0s state leaks between tests.

`_reset_k0s_all_nodes()` forcibly resets k0s on every node independent of
the controller to prevent dirty state (stale etcd membership, API refusals)
from persisting across tests.

## 7. Testing conventions and pitfalls

### Conventions

- **Naming**: In-process tests use `tNN_M_description` for grep-friendliness
  and Slurm traceability. E2E tests use standard pytest `test_` naming inside
  classes grouped by feature.

- **Fixture scoping**: `ssh_nodes` and `remote_bin_dir` are session-scoped
  (one SSH connection pool for all tests). Cluster fixtures are per-test or
  per-class. Binary upload happens once per session.

- **Skip semantics**: Fixtures skip gracefully via `pytest.skip()` when
  prerequisites are missing (no GPU, fewer nodes than required, no Docker,
  no cgroup v2, no WireGuard, no MPI plugin). Tests never fail on missing
  hardware -- they skip.

- **Teardown**: Every cluster fixture tears down on exit (kill daemons,
  remove remote dirs). Failures during deploy also trigger teardown via
  try/except/raise patterns.

- **Config overrides**: The `cluster_config_overrides` fixture can be
  overridden per test to customize config before daemons start. Overrides are
  `deep_merge()`d into the default config.

- **Daemon environment canary**: `SPUR_DAEMON_ENV_CANARY` is injected into
  spurd's launch environment. Tests verify it does NOT leak into job sessions.

### Pitfalls

- **Atomic job IDs**: The `NEXT_JOB_ID` counter in `harness.rs` is global
  across all in-process tests. Job IDs are unique within a test run but not
  predictable. Do not hard-code expected job IDs.

- **Port conflicts**: E2E tests kill stale daemons by both process pattern
  and `fuser -k` on the agent/controller ports to handle leftover processes
  from previous sessions. `_wait_port_free()` ensures a killed spurd has
  released its listener before restarting.

- **k0s state leakage**: k0s data directories (`/var/lib/k0s`) survive daemon
  kills. Without `_reset_k0s_all_nodes()`, the next test inherits stale etcd
  membership. The fixture warns if reset leaves dirty state behind.

- **HA leader stability**: `_wait_leader_elected()` requires the same leader
  to hold for 5 seconds (more than openraft's 3s leader lease) before
  proceeding. Without this, agents starting during a re-election fail to
  register.

- **Controller port override with HA**: `start_ha()` does not support
  non-default `SPUR_TEST_CONTROLLER_PORT` because spurctld hardcodes `:6817`
  for follower-forwarding.

- **Accounting async**: Job history is recorded asynchronously after the job
  ends, so `sacct` assertions must use `wait_sacct_row()` to poll rather than
  read immediately.

- **QOS cache lag**: `sacctmgr` creates a QOS, but the controller's QOS cache
  refreshes on a timer (`fairshare_refresh_secs`), not on write. Use
  `sbatch_when_qos_ready()` or `retry_until_qos_ready()` to handle the gap.

- **GPU fixture auto-marking**: Tests using `gpu_cluster` are automatically
  marked `gpu` by `pytest_collection_modifyitems`, so `-m gpu` selects them
  without explicit `@pytest.mark.gpu` decorators.

- **Rootful agents**: Some tests (cgroup, k0s, WireGuard) require `spurd` to
  run as root. The `agent_as_root=True` parameter wraps the launch in
  `sudo`. `root_agent_preflight()` checks passwordless sudo before proceeding.

- **Container image tests**: `build_container_image()` builds a minimal
  squashfs rootfs locally (needs `mksquashfs`) and uploads it to all nodes.
  `build_imported_container_image()` goes through the real `spur image import`
  path. Both skip when prerequisites are missing.
