# AGENTS.md -- spur-k8s

K8s nodes appear to `spurctld` as compute nodes without running `spurd`: the
operator's virtual agent (`agent.rs`, `SlurmAgent`) turns `LaunchJob`/`CancelJob`
into Pod create/delete. See `docs/knowledge/deployment/operations.md` (K8s
operator deployment) for the user-facing picture.

## Invariants

- `SPUR_CPUS_ON_NODE` and the `--cpus-per-gpu`/`--mem-per-gpu` exports come from
  spur-core's `resolve_cgroup_budget` and per-GPU env helpers (`pod_cpus_on_node`,
  `SpurEnv::set_per_gpu`), shared with spurd so native and pod jobs size alike.
  Do not compute them locally. The core layout is not applied to Kubernetes-backed
  nodes (`spurctld` skips them in `derive_core_layout`).
- `LaunchJob` must refuse a `target_node` outside the job's recorded allocation
  (`validate_target_node`); `target_node` bypasses the scheduler.
- Submit phase re-reads the SpurJob from the API server before `SubmitJob`
  because the informer cache can be stale after the finalizer patch.
- Deletion (`spur.amd.com/cleanup` finalizer) keeps retrying a failed cancel
  instead of dropping the finalizer, so the Spur job is never orphaned.
- OOM is reported with the wire state left as non-OOM and the kill encoded via
  `OOM_KILL_SIGNAL` (`spur_core::job::OOM_SIGNAL_FLAG`); spurctld maps it to
  `OutOfMemory`. Keep both sides in sync.
- Quota objects are server-side applied with `force` (`quota_controller.rs`), so
  admin hand-edits are reverted. GPU is quota'd on `requests.*` only, never
  `limits.*` (extended resource).

## Pitfalls

- `is_terminal` in `job_controller.rs` omits `Deadline` and `OutOfMemory`, which
  spur-core treats as terminal. It lags core.
- The heartbeat sender re-registers a node on `NOT_FOUND` (controller state
  loss). The node watcher re-registers only on resource fingerprint change:
  retryable registration errors restart the relist, while a refusal is retried
  on the node's next event.
- Taint sync to spurctld (`NodeDown`/`NodeIdle`) is tracked per node with a
  `synced` flag; unsynced states retry on the next event.
- Auth: `[auth] mode` from `--config` wins when the config file exists;
  `--auth-mode` applies only without a config file.

## Testing

- `test_support.rs` provides `FakeApiServer`, a `tower::Service` that yields a
  `kube::Client` with canned responses and recorded requests. Use it instead of a
  real cluster.
