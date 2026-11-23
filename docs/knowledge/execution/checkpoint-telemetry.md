# Checkpoint Telemetry

Jobs report checkpoint events over a per-step Unix socket. Spur observes only:
no event changes scheduling, preemption, or job state. The path crosses four
components; each crate's `AGENTS.md` holds its own constants and invariants.

## 1. Job to spurstepd

- spurstepd binds the step's socket before launching the workload, at
  `/run/spur/<cluster_name>/checkpoint/<job>.<attempt>.<step>.sock`
  (`spurd::checkpoint_dir::socket_path`). spurd hardens the directory at
  startup (`checkpoint_dir::harden`): mode 0711, owned by the user spurd runs
  as, never a symlink; an existing `run_root` or `<cluster>` ancestor that is
  group- or world-writable (mode & 0o022, sticky bit or not) is refused before
  anything beneath it is touched. `validate_socket_parent` (spurstepd's own
  pre-bind check) applies the same ancestor check to the `<cluster>` dir.
  `<cluster_name>` is `SlurmConfig.cluster_name`, not the auth `cluster_id`.
- spurd clears `SPUR_CHECKPOINT_SOCK` from the launch spec's environment
  before ever looking at `checkpoint_socket`, so a nested `srun` (or a step
  relaunched from a forwarded environment) never starts carrying a parent
  step's value. `SPUR_CHECKPOINT_SOCK` is set again only after this step's own
  listener starts: directly in the environment for a native step, or (for a
  container) as a separate `checkpoint_socket` field the container setup
  bind-mounts best-effort after the user's own binds -- never through
  `config.mounts`, whose errors abort the whole launch. A failed container
  mount only `warn!`s and leaves the variable unset; the step still runs.
- One v1 JSON event per connection, parsed by
  `spur_core::checkpoint::parse_event`; the reply is always the byte `k`.
  `job_id` is required, `step_id` optional. A sender is attributed by
  `SO_PEERCRED` uid plus its cgroup against the step's expected cgroup, with a
  uid-only fallback when the cgroup cannot be read.
- Rejected events are counted per `DropReason`. `DropReason::is_invalid`
  separates a malformed message (the application's fault) from a delivery or
  attribution drop.

## 2. spurstepd to spurd

- The step's `CheckpointRecord` lives in `<session_dir>/checkpoint.json`,
  written atomically and debounced, with a final copy before `StepdCompleted`.
  The stepd protocol is unchanged (`PROTOCOL_VERSION` 1); the file is the only
  hand-off, so spurstepd keeps recording while spurd is down.
- spurd's `CheckpointCollector` reads the file on its report tick, on
  `StepdCompleted`, and on re-adoption. A finished session is pruned only after
  the controller acknowledges its final record (`checkpoint.acked`), or once
  it is abandoned past `step_completion::ORPHAN_RETENTION`.
- The collector never applies a read whose mtime is no newer than its
  entry's own, and never lets a non-final read replace an entry already
  holding a final record -- both guard a periodic refresh racing a
  `collect_now` landing concurrently. It also evicts a tracked entry from
  memory (independent of the on-disk session prune above) once its session
  directory is confirmed gone, or once the step is known finished
  (`mark_finished`, from `StepdCompleted`/a stale-session settle) and either
  no `checkpoint.json` was ever written, or its final record has sat
  unacknowledged past `ORPHAN_RETENTION` with the directory still present.

## 3. spurd to spurctld

- Records travel only on the batched `ReportCheckpointTelemetry` RPC, never on
  the heartbeat. A retryable RPC failure (`spur_proto::controller_rpc_retryable`)
  keeps records dirty for the next attempt; an `Unimplemented` reply (older
  controller) keeps them too, behind its own back-off. Any other non-retryable
  status (a permanent rejection such as `InvalidArgument`) is logged once per
  status code and the batch is acknowledged (dropped) rather than resent
  forever.
- spurctld keeps a leader-local `CheckpointStore` (not Raft-replicated). It
  accepts a record only from a node allocated to the job's current or
  just-ended run, and finalizes a run from the `JobFinalized` side effect
  (`run_attempt`, `grace_started_at`, and `nodes` are apply-response only,
  captured before any requeue/cancel path clears the job's own allocation)
  always `FINALIZE_GRACE` after the run ended, never earlier, so a late
  record a node sends within the grace still counts.

## 4. Metrics

- Finalized runs feed the aggregate families on `/metrics/jobs`; a run with no
  checkpoint event is not recorded. Per-job series on
  `/metrics/jobs-users-accts` (behind `metrics.high_cardinality`) share the
  identity labels `{job_id, user, account, job_name}`. The per-job invalid-event
  counter is cumulative by `job_id` across every run attempt (a requeue adds to
  it, never resets it).
- Family names, labels, and retention: `crates/spur-metrics/AGENTS.md` and
  `docs/admin-guide/checkpoint-metrics.rst`.
