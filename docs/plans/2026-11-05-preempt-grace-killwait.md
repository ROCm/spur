# Preemption grace (`GraceTime`) and configurable kill waits

Reference for engineers extending this area. User-facing behaviour is documented in
`docs/admin-guide/accounting.rst` (grace), `docs/admin-guide/configuration.rst` (kill waits),
`docs/user-guide/monitoring-jobs.rst` and `docs/deployment/upgrading.rst`. Code is referenced by
file and function name; search for the name, line numbers drift.

## Problem

Before this change a preempted job received SIGTERM and, 5 seconds later, SIGKILL. The 5 seconds was fixed in
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
- Resolution: the victim's QOS value if set, else the highest value among the partitions it runs in
  (`partitions_running`), else 0. An explicit QOS `0` means no grace even when the partition has one.
  Placement is per node, so a `--partition=a,b` job records no single partition. "Runs in" is the
  requested partitions whose membership (`Node.partitions`) covers at least one allocated node; a
  node in both counts for both, and a job whose nodes are in none of its requested partitions gets no
  partition grace. A single requested partition needs no lookup.
- The effective grace is capped at the seconds left before the victim's own time limit. A victim with
  no time limit is not capped. A Requeue-mode victim with less time remaining than its grace therefore
  always requeues at the capped deadline and never ends `TIMEOUT`.
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
  reservation end and an out-of-memory kill. `scontrol suspend` of a job in its grace is refused.
- Losing a node during a grace ends the run as the preemption it was warned of (`evict_job_locked`):
  Cancel mode adopts the grace provenance and ends Cancelled; Requeue mode re-pends through
  `repend_after_preemption`, as the `JobComplete` path does. The warning already committed the run to
  ending as a preemption, so the node loss only brings the end forward. Routing it through the node-failure
  requeue would charge `requeue_count` and lose a Requeue-mode victim that has no `--requeue`. The nodes
  that had not reported are earmarked for the preemptor in both modes.

### Kill waits

Both settings are `[scheduler]` fields, live on `scontrol reconfigure`, valid `0`-`65533`, with a
floor of 5 seconds. A job that exits on SIGTERM is gone as soon as it exits; the wait is a ceiling.

| Setting | Default | Used for |
|---|---|---|
| `kill_wait_secs` | 5 | `scancel`, `scontrol requeue`, the final kill of a preemption, idle-fill reclaim, reservation end, cleanup after a failed dispatch, and other graceful stops |
| `timeout_kill_wait_secs` | 30 | stops that end `TIMEOUT`: the time limit and the inactivity limit |

Slurm's single `KillWait` covers both; to get that, set both to the same value. A cluster that sets
nothing keeps the timings from before this change (5 seconds for `scancel` and preemption, 30 at a
time limit).
`scontrol show config` prints both effective values as `KillWait` and `TimeoutKillWait`.

### COMPLETING at time and inactivity limits

At a time or inactivity limit the job moves to `COMPLETING` (`CG`) straight away, holds its nodes
for the wait, and then ends `TIMEOUT` with reason `TimeLimit`. `--requeue` on timeout still works.
`Job::is_stopping_for_time_limit` (`Completing` with `time_limit_signaled_at` set) keeps such a job
counting as running for the singleton dependency and the array `%N` throttle (`running_array_counts`);
other `COMPLETING` jobs (`scancel`, preemption) do not count, as before.

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
  is set and differs from the job's. `None` (entries from older controllers) applies to any run. The
  stamp is the run the caller decided to preempt: `preempt_job`, `preempt_job_with_provenance` and
  `reclaim_job_with_provenance` take it as `expected_run_attempt` (from the snapshot the scheduler decided on) and
  refuse a job that has moved on before proposing anything. Apply reports `preempt_applied`; a caller
  that finds the entry was skipped returns an error instead of logging a preemption or earmarking
  nodes.
- `JobComplete` carries `controller_initiated` (`#[serde(default, skip_serializing_if = "Not::not")]`,
  so entries from older controllers read `false` and keep their behaviour). The in-grace override
  that turns a no-cancel `JobComplete` on a Running victim into the preemption outcome covers the
  completions it exists for: an srun or `--pty` client reporting its run's end and a force-finish after
  partial node reports. The controller's own `Failed` completions in `requeue_after_launch_failure`
  and `hold_job_at_max_requeue` (`WalOperation::job_failed_by_controller`) set the flag, so the
  `Failed -> Pending` backoff or hold that follows still finds the `Failed` job it expects. The
  nodes those completions free are still earmarked for the preemptor. An older follower ignores the
  flag, so do not trigger these paths on an in-grace victim until every controller is upgraded.
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
- `SignalJob` for a job the agent holds nothing of (`signal_scope`, `holds_job`) returns `NOT_FOUND`, and
  the controller logs it apart from a delivery (`signal_outcome`), so a lost warning is visible. A
  request with no attempt (`run_attempt` 0) resolves to the current run through `resolve_cancel_attempt`, as
  `send_explicit_signal` and `graceful_cancel` do, so a superseded run's lingering supervisor is not
  signalled. The controller skips an agent without `signal-job` for every target; there is no
  `CancelJob` fallback. `SignalTarget::Full` stays in the protocol and the agent.
- Wait selection: `graceful_kill_wait_secs` (`spurctld/src/scheduler_loop.rs`) returns the timeout
  wait for a run with `time_limit_signaled_at` set and the kill wait otherwise. Every graceful
  `CancelJob`, including the heartbeat re-cancel, uses it, so a re-sent cancel carries the same wait.
  The agent does not know which setting a number came from.

### Scheduler flow

1. **Start.** `try_preempt` picks victims. `victim_grace_secs` returns 0, meaning preempt now as before
   this change, for a zero or time-capped grace, suspend mode, any Kubernetes-operator node, or any node
   lacking `signal-job` (`grace_eligible`). Otherwise `start_preemption_grace` commits
   `JobPreemptSignaled` and then `send_preemption_warning` signals the steps. A propose timeout
   warns later through `follow_late_preemption`; if the entry did not apply (the run changed, or it
   is already in a grace) no warning is sent and `start_preemption_grace` returns
   `PreemptStart::Superseded`, which `victim_step` treats like an already-gone victim: the rest of the
   victim set is still preempted. Only a failure with the victim still running abandons the set.
2. **During the grace.** In-grace jobs are not preemption or reclaim candidates. The preemptor waits
   for them and stays at the front of the queue, earmarks refresh, and backfill frees their nodes at
   the earlier of the estimate and the deadline plus the kill wait. The time-limit and inactivity
   watchdogs skip in-grace jobs. New `srun` steps are placed only on nodes the job still holds.
3. **Deadline.** `finalize_expired_graces` runs each pass and preempts every `Running` job past its
   deadline (`ClusterManager::jobs_past_grace_deadline` filters under the read lock and clones only the
   due jobs, so a pass with no grace running clones nothing; there is no index or persisted counter) at the matching attempt through the normal path (`preempt_victim_now`), using the stored
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

### Watchdog backstop

The time-limit and inactivity watchdogs leave a job in its grace to `finalize_expired_graces`, so a
second kill sequence does not race it, but only while `now < deadline + kill wait`
(`inside_preemption_grace`). After that the grace-end pass has failed, or the limit was lowered mid-grace
(the cap is applied only when the warning is sent), and the watchdogs expire the job normally.

### Suspend refusal

`scontrol suspend` on a job in its grace is refused after the ownership check, so a non-owner learns
nothing about the job's state.

The refusal is repeated when the `JobSuspend` entry applies, for operator and preemption suspends
alike: a grace can be committed between the early check and the apply, and a Suspended job would
never reach its deadline (the deadline scan covers Running jobs only). The apply reports whether it
suspended the job (`ClientResponse.suspend_applied`); a caller that finds it did not returns the same
grace error, and suspend-mode preemption reports the victim as not preempted without signalling its
agents. `JobResume` clears `preempt_pending`, so a resumed run is plainly Running and a preemptor
that still needs its nodes starts a fresh grace.

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
- Defaults reproduce the behaviour before this change (no grace, 5 seconds for `scancel` and
  preemption, 30 at a time limit), except that time and inactivity limited jobs now show `COMPLETING` instead of `RUNNING` for up to
  the timeout wait, and unsupervised `srun` steps get the full wait instead of 2 seconds.
- New controller with an old `spurd`: no `signal-job` capability so no grace, and `kill_wait_secs` is
  ignored so the old fixed 5 seconds applies, including at a time limit.
- Kubernetes-operator nodes: `spur-k8s` returns unimplemented for `SignalJob` and deletes Pods on any
  cancel, so victims there get no grace and the kill waits do not apply.

## Known limitation

The kill waits do not yet reach `srun` steps: when a job is stopped (time limit, `scancel`, inactivity
limit or preempt-cancel), `spurd` reports the job finished on the node within milliseconds while its
`srun` steps are still running or already killed, so the controller ends the job and frees the node
before the steps get their wait. Two e2e tests assert the intended
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
  `qos_limits_with_numeric_grace_time_still_deserializes`, `preempt_run_attempt_wire_format_is_frozen`
  (`JobPreemptRequeue` and `JobPreemptCancel` with `run_attempt`), and the legacy-entry checks for
  the same two entries without it (the `*_from_a_pre_upgrade_entry_applies_to_any_run` tests below).
- WAL (`spur-core/src/wal.rs`): `a_job_complete_written_before_controller_initiated_existed_replays_as_the_runs_own_end`,
  `a_controller_initiated_job_complete_has_a_frozen_wire_form`.
- Job helpers (`spur-core/src/job.rs`): `only_a_completing_run_signalled_for_a_limit_is_stopping_for_the_time_limit`,
  `preempt_grace_ignores_a_warning_recorded_for_an_earlier_run`,
  `held_nodes_leaves_out_the_nodes_that_reported`.
- Grace resolution and eligibility (`spurctld/src/scheduler_loop.rs`): `grace_time_prefers_the_qos_value`,
  `grace_time_comes_from_the_partition_the_job_runs_in`,
  `grace_time_takes_the_highest_among_the_partitions_hosting_the_job`,
  `grace_time_is_zero_when_no_requested_partition_hosts_the_job`,
  `a_single_requested_partition_needs_no_node_lookup`,
  `a_multi_partition_victim_gets_the_grace_of_the_partition_it_runs_in`,
  `qos_zero_overrides_a_partition_grace`,
  `grace_time_defaults_to_zero`, the `effective_grace_*` cap tests,
  `grace_is_never_eligible_for_suspend`, `grace_is_not_eligible_on_a_kubernetes_node`,
  `grace_needs_every_allocated_node_to_support_signal_job`, `grace_capped_to_zero_preempts_immediately`,
  `try_preempt_with_grace_signals_instead_of_killing`, `try_preempt_uses_partition_grace_when_qos_unset`,
  `a_suspend_preemption_the_apply_refuses_leaves_the_victim_running`.
- Deadline, reclaim and timers (`scheduler_loop.rs`): `grace_does_nothing_before_the_deadline`,
  `grace_preempts_at_the_deadline_with_the_stored_mode`, `grace_requeues_at_the_deadline_when_that_was_the_stored_mode`,
  `grace_finalizer_skips_a_stale_attempt`, `only_running_jobs_whose_grace_deadline_has_passed_are_due`,
  `a_grace_recorded_for_an_earlier_run_is_not_due`, `warning_a_victim_already_in_grace_is_superseded_not_an_error`,
  `a_superseded_victim_does_not_end_the_victim_set`, `a_failed_preemption_abandons_the_set_unless_the_victim_is_gone`, `a_grace_that_applies_late_still_warns_the_victim`,
  `reclaim_skips_an_in_grace_job`, `preemptor_waits_for_its_in_grace_victim`,
  `time_limit_watchdog_leaves_a_capped_grace_to_the_finalizer_until_its_kill_wait_ends`,
  `time_limit_watchdog_expires_a_capped_grace_the_finalizer_never_ended`,
  `time_limit_watchdog_expires_a_grace_whose_time_limit_was_lowered_after_it_started`,
  `inactive_watchdog_skips_an_in_grace_job_until_its_kill_wait_ends`,
  `inactive_watchdog_expires_a_grace_the_finalizer_never_ended`, `busy_until_uses_the_grace_deadline`,
  `graceful_wait_is_timeout_wait_for_a_timed_out_run`, `graceful_wait_is_kill_wait_otherwise`,
  `an_expired_time_limit_sends_one_graceful_cancel_and_no_sigkill`.
- Dependencies (`spur-core/src/dependency.rs`): `singleton_waits_for_a_predecessor_still_inside_its_time_limit_kill_wait`,
  `singleton_ignores_a_cancelled_completing_predecessor`, `singleton_waits_for_a_running_predecessor`.
- State machine (`spurctld/src/cluster.rs`): `array_throttle_counts_a_task_stopped_by_its_time_limit_but_not_a_cancelled_one`, `signal_preemption_records_the_grace_and_reports_it_applied`,
  `a_grace_for_an_earlier_run_does_not_preempt_the_current_one`,
  `a_partial_exit_in_grace_keeps_the_job_running`, `a_replayed_partial_exit_in_grace_frees_nothing_twice`,
  `all_nodes_exiting_in_grace_ends_preempted`, `cancel_mode_exit_during_grace_ends_cancelled_and_preempted`,
  `requeue_mode_exit_during_grace_repends_with_hold`, `exit_during_grace_is_counted_once_when_requeued`,
  `a_user_cancel_after_a_partial_exit_in_grace_wins`, `an_oom_kill_reported_in_a_grace_ends_out_of_memory`,
  `a_reservation_ending_cancels_a_job_in_a_cancel_grace`, `suspend_refuses_a_job_in_preemption_grace`,
  `an_operator_suspend_applied_during_a_grace_leaves_the_job_running`,
  `a_preemption_suspend_applied_during_a_grace_leaves_the_job_running`,
  `a_suspend_applied_outside_a_grace_reports_it_applied`,
  `suspend_reports_a_grace_that_lands_after_its_check`,
  `suspend_mode_preemption_refused_at_apply_is_not_a_preemption`,
  `resuming_a_job_that_carries_a_grace_drops_the_grace`,
  `losing_a_node_still_running_ends_a_partially_exited_grace_victim`,
  `a_launch_failure_requeue_during_a_cancel_grace_repends_the_job_with_backoff`,
  `a_held_launch_failure_during_a_cancel_grace_holds_the_job`,
  `holding_at_the_requeue_cap_during_a_cancel_grace_holds_the_job`,
  `a_node_that_reported_during_a_grace_is_no_longer_busy`,
  `losing_a_node_in_a_cancel_grace_earmarks_the_freed_nodes_for_the_preemptor`,
  `losing_a_node_in_a_requeue_grace_repends_the_victim_as_preempted`,
  `partition_grace_time_round_trips_through_update_and_clear`, and the `run_attempt` gate on the final
  preemption: the apply side (`preempt_cancel_decided_for_an_earlier_run_is_a_noop`,
  `preempt_requeue_decided_for_an_earlier_run_is_a_noop`,
  `preempt_cancel_decided_for_the_current_run_applies`,
  `preempt_requeue_decided_for_the_current_run_applies`,
  `preempt_cancel_from_a_pre_upgrade_entry_applies_to_any_run`,
  `preempt_requeue_from_a_pre_upgrade_entry_applies_to_any_run`), the proposer
  (`preempting_a_running_job_stamps_the_run_it_was_decided_for`,
  `preempting_a_run_the_job_has_moved_past_proposes_nothing`,
  `reclaiming_a_run_the_job_has_moved_past_proposes_nothing`) and the applied report
  (`a_final_preemption_that_applies_reports_it_applied`,
  `a_final_preemption_for_another_run_does_not_report_it_applied`,
  `a_final_preemption_the_apply_skips_is_not_reported_as_preempted`).
- RPC layer (`spurctld/src/server.rs`): `a_partial_exit_in_grace_does_not_signal_the_other_nodes`,
  `the_batch_scripts_node_reporting_in_grace_sends_the_graceful_cancel`,
  `job_to_proto_reports_grace_preempt_time_and_end_time`, `ping_reports_the_effective_kill_waits`.
- Agent (`spurd/src/agent_server.rs`): `signal_job_for_a_job_this_node_does_not_hold_is_not_found`,
  `a_signal_for_no_particular_attempt_is_scoped_to_the_current_run`,
  `a_signal_for_a_superseded_attempt_has_no_scope`, `a_signal_for_an_orphaned_step_keeps_the_wildcard_scope`,
  and the controller-side `signals_reach_each_node_in_order_and_skip_agents_without_signal_job` and
  `a_not_found_signal_reply_is_reported_apart_from_a_delivery_and_a_failure`. Also the `signal_job_*` tests (target filtering, never escalates,
  stale attempt dropped, unspecified and batch targets rejected, non-controller callers rejected) and
  the graceful-cancel tests that assert the request's wait is honoured. `spur-k8s` has
  `virtual_agent_signal_job_is_unimplemented`.
- Partition `GraceTime` update: `update_partition_clear_grace_time_accepts_only_boolean_values`,
  `update_partition_rejects_a_grace_time_together_with_a_clear` and
  `update_partition_flags_reject_a_grace_time_together_with_a_clear` (through the real inline and clap
  parsing), and `update_partition_rejects_a_grace_time_together_with_a_clear` in `spurctld/src/server.rs`
  (the handler, via `partition_grace_time_update`). `ClearGraceTime=` shares `boolean::parse_bool` with
  `sacctmgr`.
- CLI (`spur-cli`): `sacctmgr` tests for `gracetime` and `cleargracetime` parsing and the `GraceTime`
  column, `scontrol` tests for `create partition` (`create_partition_reads_a_grace_time`,
  `create_partition_rejects_a_malformed_grace_time`), `update partition` (`GraceTime`,
  `ClearGraceTime`, malformed value),
  `show partition`, `show job` (`PreemptTime`) and `show config` (`KillWait`, `TimeoutKillWait`, absent
  against an older controller).

### Accounting database tests

`#[ignore]`d unless `DATABASE_URL` points at PostgreSQL: `qos_grace_time_unset_vs_zero_round_trips`
(`spurctld/src/accounting/db.rs`) checks unset is NULL, an explicit `0` is kept, and `cleargracetime`
returns to NULL. The gRPC patch logic is covered without a database by the `grace_time_patch_*` tests
in `accounting/grpc.rs`, and the audit record by `grace_time_audit_records_a_value_a_clear_and_nothing` and
`grace_time_audit_records_what_was_stored_when_a_value_and_a_clear_are_both_given`: the record carries the
stored seconds, or null for a clear, and a clear given together with a value records the null that was stored.

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
