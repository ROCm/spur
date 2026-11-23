# Spur Test Framework

## 1. Where tests belong

- **In-process component tests**: `crates/spur-tests/`. No daemons, no
  network, no async runtime; fixtures are in-memory structs. See section 2.
- **Crate-local unit tests**: in each crate. Non-default requirements (serial
  env tests, PTY hangs, paused tokio time, root) are in that crate's
  `AGENTS.md` Testing section.
- **E2E native-host**: `tests/native_host/e2e/` (pytest). Deploys real daemons
  over SSH to pre-provisioned nodes; cluster-only.
- **E2E Kubernetes**: `tests/k8s/e2e/` (pytest, kubeconfig). Deploys the
  operator and SpurJob CRD into a namespace.

Prerequisites, environment variables, and run commands for both E2E suites
are in `docs/developer/building.rst`.

## 2. In-process tests (`crates/spur-tests`)

### 2.1. Test naming

Modules mirror Slurm's testsuite numbering where a Slurm equivalent exists
(`t39_gpu` for Slurm test39.x). Spur-only groups start at 50. Tests are named
`tNN_M_description` (NN = group, M = sub-test), which keeps `cargo test`
output greppable and traceable to Slurm test IDs.

### 2.2. Harness fixtures

`harness.rs` builds in-memory `Job`, `Node`, and `Partition` fixtures plus
state-transition assertions (`assert_transition_ok`, `assert_transition_err`).

- `make_job` / `make_job_with_resources` take IDs from the shared counter (see
  section 2.3) and default to user `testuser`, partition `default`.
- `make_nodes` names nodes `node001`.. and always puts them in partition
  `default`; `make_partition` builds the matching `node[001-NNN]` hostlist.
- Scheduler tests build a `ClusterState` from these fixtures, call
  `schedule()`, and assert on the returned assignments.

### 2.3. Shared state

The only shared mutable state is the global `NEXT_JOB_ID` counter in
`harness.rs`. Job IDs are unique within a run but not predictable, so never
assert an absolute job ID. Otherwise tests must stay
deterministic and independent.

## 3. Native-host E2E conventions

- **Fixture scope**: SSH connections and binary upload are session-scoped;
  cluster fixtures are per-test or per-class.
- **Teardown**: every cluster fixture tears down on exit (kill daemons, remove
  remote dirs), including when deploy fails (`_deploy_cluster` wraps deploy in
  try/teardown/raise).
- **Skip, never fail, on missing prerequisites**: fixtures call
  `pytest.skip()` when GPUs, enough nodes, Docker, cgroup v2, WireGuard, sudo,
  or the MPI plugin are missing.
- **Config overrides**: override the `cluster_config_overrides` fixture per
  test or class; the dict is `deep_merge()`d into the default config before
  `spurctld` starts.
- **GPU marking**: tests using `gpu_cluster` or `gpu_container_cluster` are
  auto-marked `gpu` by `pytest_collection_modifyitems`. CI routes the GPU and
  `not gpu` pools by this marker, so do not rely on manual tags.
- **Daemon env canary**: `SPUR_DAEMON_ENV_CANARY` is injected into spurd's
  environment; tests assert it does not leak into job sessions.

## 4. Native-host E2E pitfalls

- **Port conflicts**: stale daemons are killed by process pattern and by
  `fuser -k` on the ports. Before restarting spurd, `_wait_port_free()`
  ensures the killed spurd has released its listener.
- **k0s state leakage**: `spur k8s down --reset` only flags the phase; the
  controller resets k0s asynchronously and fixture teardown cuts it off. k0s
  fixtures therefore follow `k8s_down(reset=True)` with
  `_reset_k0s_all_nodes()`, which resets every node directly and warns if
  `/var/lib/k0s` survives. Without it the next test inherits stale etcd
  membership.
- **HA leader stability**: `_wait_leader_elected()` requires the same leader for 5s (more than
  openraft's 3s leader lease); writes submitted during a re-election fail with
  "not the Raft leader".
- **HA and port override**: `start_ha()` rejects a non-default
  `SPUR_TEST_CONTROLLER_PORT` because spurctld hardcodes `:6817` for
  follower forwarding.
- **Accounting is async**: job history is recorded after the job ends; poll
  with `wait_sacct_row()` instead of reading `sacct` once.
- **QOS cache lag**: the controller's QOS cache refreshes on a timer
  (`fairshare_refresh_secs`), not on write. Submit with
  `sbatch_when_qos_ready()` or `retry_until_qos_ready()` after `sacctmgr`
  creates a QOS.
- **Rootful agents**: cgroup, k0s, and WireGuard tests need spurd as root
  (`agent_as_root=True` wraps it in sudo). Call `root_agent_preflight()` first
  so missing sudo skips instead of failing.
- **Container images**: `build_container_image()` needs `mksquashfs` on the
  runner; `build_imported_container_image()` goes through the real
  `spur image import` path. Both skip when prerequisites are missing.
