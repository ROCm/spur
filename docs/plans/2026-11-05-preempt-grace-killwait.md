# Preemption grace (`GraceTime`) and configurable kill waits

Reference for engineers extending this area. User-facing behaviour is documented in
`docs/admin-guide/accounting.rst` (grace), `docs/admin-guide/configuration.rst` (kill waits),
`docs/user-guide/monitoring-jobs.rst` and `docs/deployment/upgrading.rst`. Code is referenced by
file and function name; search for the name, line numbers drift.

## Problem

On `main` a preempted job received SIGTERM and, 5 seconds later, SIGKILL. The 5 seconds was fixed in
`spurd`, so neither the admin nor the workload could change it, and it is too short for a training job
to write a checkpoint to shared storage. Time limits and the inactivity limit were a second, separate
path with their own fixed waits.

This work adds:

- `GraceTime`: extra time a preempted job keeps running, after a warning, before the normal kill.
- `kill_wait_secs` and `timeout_kill_wait_secs`: configurable SIGTERM-to-SIGKILL waits that every
  graceful stop uses.

## User-facing behaviour

### `GraceTime`

- Set per QOS (`sacctmgr ... set GraceTime=<secs>`, `cleargracetime=1` to unset) and per partition
  (`grace_time` under `[[partitions]]`, `scontrol update partition GraceTime=` / `ClearGraceTime=1`).
- Resolution: the victim's QOS value if set, else the highest value among the victim's matched
  partitions, else 0. An explicit QOS `0` means no grace even when the partition has one.
- The effective grace is capped at the seconds left before the victim's own time limit. A victim with
  no time limit is not capped.
- Applies to `cancel` and `requeue` preemption only. Suspend preemption never gets a grace.
- The warning is SIGCONT then SIGTERM, delivered to `srun` steps only. The batch script is not
  signalled, matching Slurm; a script that does not use `srun` is warned of nothing and only sees the
  final kill.
- While the grace runs the job stays `RUNNING` (`R`) and keeps its nodes. `scontrol show job` prints
  `PreemptTime=` and `EndTime` is the deadline. At the deadline the normal preemption runs.
- There is no abort: once the grace starts the job is preempted at the deadline even if the preemptor
  has since been cancelled or placed elsewhere (Slurm behaves the same).
- A job that exits during its grace is recorded `PREEMPTED`, not as a normal completion.
- A node that finishes early frees its slice (earmarked for the preemptor) but does not end the job or
  signal the other nodes. The job ends when every node has reported or at the deadline, whichever is
  first. When the batch script's own node reports, the job has ended and the remaining nodes are
  stopped.
- Overrides: a user `scancel` during a grace wins and the run ends as that cancel; so do `scontrol requeue`,
  node failure, reservation end and an out-of-memory kill. `scontrol suspend` of a job in
  its grace is refused.

### Kill waits

Both settings are `[scheduler]` fields, live on `scontrol reconfigure`, valid `0`-`65533`, with a
floor of 5 seconds. A job that exits on SIGTERM is gone as soon as it exits; the wait is a ceiling.

| Setting | Default | Used for |
|---|---|---|
| `kill_wait_secs` | 5 | `scancel`, `scontrol requeue`, the final kill of a preemption, idle-fill reclaim, reservation end, cleanup after a failed dispatch, and other graceful stops |
| `timeout_kill_wait_secs` | 30 | stops that end `TIMEOUT`: the time limit and the inactivity limit |

Slurm's single `KillWait` covers both; to get that, set both to the same value. A cluster that sets
nothing keeps `main`'s timings (5 seconds for `scancel` and preemption, 30 at a time limit).
`scontrol show config` prints both effective values as `KillWait` and `TimeoutKillWait`.

### COMPLETING at time and inactivity limits

At a time or inactivity limit the job moves to `COMPLETING` (`CG`) straight away, holds its nodes
for the wait, and then ends `TIMEOUT` with reason `TimeLimit`. `--requeue` on timeout still works.

## Design

### Why the controller counts the grace

The alternative was to commit the preemption at once and let `spurd` count the grace. Rejected:
holding nodes needs no new mechanism if the victim simply stays `RUNNING`; the deadline lives in the
Raft log and survives `spurd` restarts and leader changes; the controller needs the deadline anyway
for scheduling; it matches Slurm. Costs: about one second of precision, and a job runs over its
deadline if every controller is down at that moment.

### Key types and state

- `PreemptPending` (`spur-core/src/job.rs`): `signaled_at`, `deadline`, `preempted_by`,
  `preempt_qos`, `mode`, `run_attempt`, `requeue_hold_secs`. Stored as `Job.preempt_pending`
  (`#[serde(default)]`). `requeue_hold_secs` is computed by the leader so a replay re-pends with the
  same hold.
- `Job::preempt_grace()` returns the pending grace only if its `run_attempt` matches the job's, so a
  warning left by an earlier run never applies to the current one.
- `Job::held_nodes()` returns allocated nodes that have not yet reported (`node_completions`). Every
  kill sent after partial exits goes only to these.
- `Job.time_limit_signaled_at` marks a run stopped by a time or inactivity limit and selects the
  timeout wait.
- QOS: `QosLimits.grace_time: Option<u32>` (`spur-core/src/accounting.rs`), DB column
  `qos.grace_time` accepting NULL, `optional uint32 grace_time` in the protobuf. Partition:
  `Partition.grace_time: Option<u32>`, `PartitionConfig.grace_time`, and
  `WalOperation::PartitionUpdate.grace_time: Option<Option<u32>>` (outer `None` leaves it unchanged,
  `Some(None)` clears it).

### WAL operations

- `JobPreemptSignaled { job_id, pending }` records the grace. Apply sets `preempt_pending` only for a
  `Running` job at the exact `run_attempt` with none pending, and reports `preempt_signal_applied`.
  Proposer: `ClusterManager::signal_preemption`.
- `JobTimeLimitExpired { job_id, run_attempt, at }` records `time_limit_signaled_at` and
  `cancel_signaled_at` and moves the run to `COMPLETING`. It acts only on a `Running` or `Suspended`
  run at the matching attempt, so a late expiry cannot relabel a run already completing for another
  reason. Proposer: `ClusterManager::expire_time_limit`. The older `JobTimeLimitSignaled` stays for
  replay.
- `JobPreemptRequeue` and `JobPreemptCancel` carry an optional `run_attempt`; apply does nothing when it
  is set and differs from the job's. `None` (entries from older controllers) applies to any run.
- Every transition that ends or replaces a run clears `preempt_pending`: the final preemption
  entries, user and admin cancel, admin requeue, eviction and node failure, time-limit expiry and
  force-complete.

### Agent protocol

- `SignalJob` (`AgentSignalJobRequest { job_id, run_attempt, signal, target }`) delivers one signal and
  nothing else: no escalation, timer, reservation release or cgroup reaping, and it never marks steps
  cancelled. `SignalTarget`: `STEPS` (every step except the batch script), `BATCH` (defined, rejected
  as unimplemented), `FULL`; unspecified is rejected. Handler: `signal_job` in
  `spurd/src/agent_server.rs`. The controller sends SIGCONT then SIGTERM as two calls per node.
- `CancelJob` stays "terminate" and gains `kill_wait_secs` (0 = the old agent default, 5 seconds).
- Capability: agents advertise `"signal-job"` (`spur_core::node::CAP_SIGNAL_JOB`) in
  `RegisterAgentRequest.capabilities`, stored as `Node.capabilities`. A victim on any node without it
  gets no grace and a warning is logged naming the node.
- Wait selection: `graceful_kill_wait_secs` (`spurctld/src/scheduler_loop.rs`) returns the timeout
  wait for a run with `time_limit_signaled_at` set and the kill wait otherwise. Every graceful
  `CancelJob`, including the heartbeat re-cancel, uses it, so a re-sent cancel carries the same wait.
  The agent does not know which setting a number came from.

### Scheduler flow

1. **Start.** `try_preempt` picks victims. `victim_grace_secs` returns 0, meaning preempt now as on
   `main`, for a zero or time-capped grace, suspend mode, any Kubernetes-operator node, or any node
   lacking `signal-job` (`grace_eligible`). Otherwise `start_preemption_grace` commits
   `JobPreemptSignaled` and then `send_preemption_warning` signals the steps. A propose timeout
   warns later through `follow_late_preemption`; if the entry did not apply (the run changed) no
   warning is sent.
2. **During the grace.** In-grace jobs are not preemption or reclaim candidates. The preemptor waits
   for them and stays at the front of the queue, earmarks refresh, and backfill frees their nodes at
   the earlier of the estimate and the deadline plus the kill wait. The time-limit and inactivity
   watchdogs skip in-grace jobs. New `srun` steps are placed only on nodes the job still holds.
3. **Deadline.** `finalize_expired_graces` runs each pass and preempts every `Running` job past its
   deadline at the matching attempt through the normal path (`preempt_victim_now`), using the stored
   mode and provenance. The kill goes to `held_nodes()` only. The final entries free and wait only
   for nodes that have not reported, so no node is freed twice.
4. **Partial and full exit.** `JobNodeComplete` on an in-grace run records the node and frees its
   slice without leaving `RUNNING`; when the last node reports the job finalizes as preempted
   (cancel mode: `CANCELLED` with provenance; requeue mode: back to `PENDING` with the logged hold;
   accounting `PREEMPTED` either way, counted once, not a karma completion). For a batch script that
   launches ranks with `srun`, only the batch script's node reporting stops the other nodes
   (`report_job_status`, `batch_script_uses_step_launch`). Standalone `srun` allocations, with or
   without `--pty`, get the same preempted outcome via `JobComplete`.

### Timers that follow the wait

The heartbeat re-cancel interval (`is_reclaimable`, `cancel_reclaim_grace`) scales with the wait the
job got. Backfill estimates for running jobs add `timeout_kill_wait_secs`. The requeue hold is
`max(2 x interval + 3, kill_wait_secs)` (`requeue_hold_secs`). The reclaim cool-down follows
`kill_wait_secs`. `complete_wait_secs` should exceed the larger wait plus
`[cgroup] cleanup_timeout_secs` (`SlurmConfig::complete_wait_floor_secs`); the controller logs a
warning at load and reconfigure when it does not (`complete_wait_too_short`) but never rejects the config, so deployed
configurations keep loading.

### Suspend refusal

`scontrol suspend` on a job in its grace is refused after the ownership check, so a non-owner learns
nothing about the job's state.

## Compatibility and upgrade notes

- New Raft entries `JobTimeLimitExpired` (every time or inactivity limit) and `JobPreemptSignaled`
  (only when a grace is configured) cannot be read by older controllers: upgrade the leader last, and
  do not roll back once they have been written.
- `PartitionUpdate.grace_time` is a new `#[serde(default)]` field on an existing entry. An old
  follower ignores it, so set partition `GraceTime` only after every controller is upgraded.
- Protobuf changes are additive: `SignalJob`, `SignalTarget`, `AgentSignalJobRequest`,
  `AgentCancelJobRequest.kill_wait_secs`, `RegisterAgentRequest.capabilities`, `grace_time` and
  `clear_grace_time` on the QOS and partition messages, `preempt_time` on the job message, and the
  optional `kill_wait_secs` and `timeout_kill_wait_secs` on `PingResponse`. `QosInfo.grace_time` was
  made `optional` at its existing tag, which is wire-compatible for a scalar.
- Defaults reproduce `main` (no grace, 5 seconds for `scancel` and preemption, 30 at a time limit),
  except that time and inactivity limited jobs now show `COMPLETING` instead of `RUNNING` for up to
  the timeout wait, and unsupervised `srun` steps get the full wait instead of 2 seconds.
- New controller with an old `spurd`: no `signal-job` capability so no grace, and `kill_wait_secs` is
  ignored so the old fixed 5 seconds applies, including at a time limit.
- Kubernetes-operator nodes: `spur-k8s` returns unimplemented for `SignalJob` and deletes Pods on any
  cancel, so victims there get no grace and the kill waits do not apply.

## Known limitation

The kill waits do not yet reach `srun` steps: when a job is stopped (time limit, `scancel`, inactivity
limit or preempt-cancel), `spurd` reports the job finished on the node within milliseconds while its
`srun` steps are still running or already killed, so the controller ends the job and frees the node
before the steps get their wait. Tracked in ROCm/spur#1180. Two e2e tests assert the intended
behaviour and are marked `xfail` until it is fixed (see the test inventory).

## Follow-ups

- Route `scancel --signal`, `-b` and `--full` through `SignalJob` and retire the signal form of
  `CancelJob`.
- Grace on `spur-k8s` nodes, and `scancel --signal` there currently deleting the job's Pods.
- Honour a user `--signal` as the warning signal (Slurm's `send_user_signal`).
- Expose `preempt_time` and the grace `EndTime` through REST.
- Reservation end like Slurm: end `TIMEOUT`, stop immediately and use the timeout wait.
- Change `update_partition` from positional arguments to a patch type; it already takes several
  `Option<Option<_>>` parameters.
- Requeue-mode preemption and `scontrol requeue` free the node when the requeue commits, while the
  old run is still in its kill wait; release the nodes without completing the run instead.
- The "one node finished, stop the others" step in `report_job_status` is fire-and-forget with no
  retry outside a grace.

## Test inventory

Unit tests call real code, use explicit fixtures and have no timeouts.

### Rust unit tests

- Configuration (`spur-core/src/config.rs`): defaults and flooring of both waits,
  `timeout_kill_wait_above_max_is_rejected`, `complete_wait_too_short_uses_the_larger_kill_wait`,
  `config_partition_grace_time_loads`.
- Persisted compatibility (`spur-core/src/wal.rs`, `partition.rs`, `accounting.rs`, `node.rs`):
  frozen-JSON tests `partition_update_without_grace_time_replays`,
  `partition_update_grace_time_wire_format_is_frozen`, `job_time_limit_expired_has_a_frozen_wire_form`,
  `partition_snapshot_without_grace_time_loads`, `qos_limits_without_grace_time_deserializes`,
  `qos_limits_with_numeric_grace_time_still_deserializes`, and the legacy-entry checks for
  `JobPreemptRequeue` and `JobPreemptCancel` without `run_attempt`.
- Job helpers (`spur-core/src/job.rs`): `preempt_grace_ignores_a_warning_recorded_for_an_earlier_run`,
  `held_nodes_leaves_out_the_nodes_that_reported`.
- Grace resolution and eligibility (`spurctld/src/scheduler_loop.rs`): `grace_time_prefers_the_qos_value`,
  `grace_time_falls_back_to_the_highest_partition`, `qos_zero_overrides_a_partition_grace`,
  `grace_time_defaults_to_zero`, the `effective_grace_*` cap tests,
  `grace_is_never_eligible_for_suspend`, `grace_is_not_eligible_on_a_kubernetes_node`,
  `grace_needs_every_allocated_node_to_support_signal_job`, `grace_capped_to_zero_preempts_immediately`,
  `try_preempt_with_grace_signals_instead_of_killing`, `try_preempt_uses_partition_grace_when_qos_unset`.
- Deadline, reclaim and timers (`scheduler_loop.rs`): `grace_does_nothing_before_the_deadline`,
  `grace_preempts_at_the_deadline_with_the_stored_mode`, `grace_requeues_at_the_deadline_when_that_was_the_stored_mode`,
  `grace_finalizer_skips_a_stale_attempt`, `a_grace_that_applies_late_still_warns_the_victim`,
  `reclaim_skips_an_in_grace_job`, `preemptor_waits_for_its_in_grace_victim`,
  `time_limit_watchdog_skips_an_in_grace_job`, `busy_until_uses_the_grace_deadline`,
  `graceful_wait_is_timeout_wait_for_a_timed_out_run`, `graceful_wait_is_kill_wait_otherwise`,
  `an_expired_time_limit_sends_one_graceful_cancel_and_no_sigkill`.
- State machine (`spurctld/src/cluster.rs`): `signal_preemption_records_the_grace_and_reports_it_applied`,
  `a_grace_for_an_earlier_run_does_not_preempt_the_current_one`,
  `a_partial_exit_in_grace_keeps_the_job_running`, `a_replayed_partial_exit_in_grace_frees_nothing_twice`,
  `all_nodes_exiting_in_grace_ends_preempted`, `cancel_mode_exit_during_grace_ends_cancelled_and_preempted`,
  `requeue_mode_exit_during_grace_repends_with_hold`, `exit_during_grace_is_counted_once_when_requeued`,
  `a_user_cancel_after_a_partial_exit_in_grace_wins`, `an_oom_kill_reported_in_a_grace_ends_out_of_memory`,
  `a_reservation_ending_cancels_a_job_in_a_cancel_grace`, `suspend_refuses_a_job_in_preemption_grace`,
  `losing_a_node_still_running_ends_a_partially_exited_grace_victim`,
  `partition_grace_time_round_trips_through_update_and_clear`, and the `run_attempt` gate on the final
  preemption (`preempt_cancel_decided_for_an_earlier_run_is_a_noop`,
  `preempt_requeue_decided_for_an_earlier_run_is_a_noop`,
  `preempting_a_running_job_stamps_the_run_it_was_decided_for`).
- RPC layer (`spurctld/src/server.rs`): `a_partial_exit_in_grace_does_not_signal_the_other_nodes`,
  `the_batch_scripts_node_reporting_in_grace_sends_the_graceful_cancel`,
  `job_to_proto_reports_grace_preempt_time_and_end_time`, `ping_reports_the_effective_kill_waits`.
- Agent (`spurd/src/agent_server.rs`): the `signal_job_*` tests (target filtering, never escalates,
  stale attempt dropped, unspecified and batch targets rejected, non-controller callers rejected) and
  the graceful-cancel tests that assert the request's wait is honoured. `spur-k8s` has
  `virtual_agent_signal_job_is_unimplemented`.
- CLI (`spur-cli`): `sacctmgr` tests for `gracetime` and `cleargracetime` parsing and the `GraceTime`
  column, `scontrol` tests for `update partition` (`GraceTime`, `ClearGraceTime`, malformed value),
  `show partition`, `show job` (`PreemptTime`) and `show config` (`KillWait`, `TimeoutKillWait`, absent
  against an older controller).

### Accounting database tests

`#[ignore]`d unless `DATABASE_URL` points at PostgreSQL: `qos_grace_time_unset_vs_zero_round_trips`
(`spurctld/src/accounting/db.rs`) checks unset is NULL, an explicit `0` is kept, and `cleargracetime`
returns to NULL. The gRPC patch logic is covered without a database by the `grace_time_patch_*` tests
in `accounting/grpc.rs`.

### End-to-end tests (`tests/native_host/e2e/`)

`test_preemption_grace.py`:

- `test_grace_cancel_mode`: victim warned, stays running through the grace and the preemptor waits,
  victim ends cancelled with `PreemptedBy`, `sacct` shows `PREEMPTED`, `sdiag` counts one preemption.
- `test_grace_requeue_mode`: the same for requeue mode, ending with the victim pending again.
- `test_grace_survives_spurd_restart`: restarting `spurd` mid-grace does not reset or cut it short.
- `test_exit_during_grace_is_preempted`: a victim that exits cleanly on the warning is recorded
  `PREEMPTED`.
- `test_grace_partial_exit_multi_node`: two nodes, one rank exits on the warning; skipped below two
  nodes.
- `test_grace_partial_exit_keeps_other_rank_running`: two nodes, rank 0 leaves on the warning and
  rank 1 keeps writing its checkpoint past the kill wait; the job stays `R` and rank 1 finishes.
- `test_partition_grace_time`: a partition `GraceTime` with the QOS unset gives a grace; QOS
  `GraceTime=0` gives immediate preemption.
- `test_time_limit_uses_timeout_kill_wait`: a job that ignores SIGTERM sits in `CG` for the timeout
  wait and ends `TIMEOUT`.

`test_inactive_limit.py`: `test_a_live_salloc_survives_a_controller_restart_under_native_auth`
predates this work. The class `TestInactiveLimitKillWait` adds two tests marked
`xfail(strict=True)` because of the known limitation above: `test_inactive_limit_warns_then_times_out`
and `test_inactive_limit_reconnect_does_not_save_the_job` expect the job to reach `CG` and spend the
timeout wait there, but `spurd` reports the job finished while the step is still running, so the job
goes `R` to `TO` without `CG`. Being strict, they start failing the suite as soon as the limitation is
fixed, which is the cue to drop the marker.
