# AGENTS.md -- spur-core

Shared types for every other crate. General WAL schema rules are in root
`AGENTS.md` (Breaking Changes) and
[raft.md](../../docs/knowledge/consensus/raft.md#breaking-change-sensitivity).

## Persisted state

- Every new field on a type reachable from `WalOperation` or the Raft snapshot (`Job`, `Node`, `JobSpec`, `ResourceSet`, `ResourceAllocations`, `Device`, `Partition`, `Reservation`, and everything they embed) must deserialize when absent: `#[serde(default)]` or a plain `Option<T>`. An `Option` with `deserialize_with` still needs `default`. This is the most common review finding.
- `ResourceSet` serde still writes the deprecated GPU-only `gpus` list next to `devices` (and reads it from older entries) for one release, so older controllers keep replaying. The legacy shapes live in `device.rs` (`legacy_gpu_serde`); do not drop them before that release window closes.
- Frozen-payload tests guard this (`job_submit_v0_5_1_payload_still_deserializes` in `wal.rs`, `a_node_snapshot_without_k0s_last_error_still_deserializes` in `node.rs`). A failure means the field needs a default, not a regenerated fixture.
- Never rename or remove a variant of `WalOperation`, `JobState`, `PendingReason`, `NodeState`, or any type nested in them: the variant name is the JSON discriminant. `grp_wall_reason_has_a_frozen_wire_name` pins a `PendingReason` wire name; add one for any new variant with a non-obvious name. Removing a `JobState` variant also breaks `job_state_proto_discriminants_match_core`.
- `PendingReason` display strings are byte-exact Slurm output (scraped by monitoring tools). Add a new variant together with its enforcement logic, never ahead of it, so dead variants do not accrete in the snapshot contract.
- Ops that date something a user reads back carry `at`, stamped by the proposing leader in `stamp_occurred_at()`. Followers and replays use it, never their own clock. Its exhaustive match makes a new dateable variant that omits `at` a compile error.
- `#[serde(skip)]` fields are leader-local or per-pass and must stay unpersisted: `Job::preferred_nodes`, `Job::last_sched_eval` (replicating it would be a Raft write storm), `Node::layout_blocked_cpus` (from heartbeats).
- `Node::reserved_picks` is snapshot-persisted because `core_layout` is not, so every replica and replay recomputes sticky per-GPU picks from the same history.
- `truncate_step_name()` must be applied before proposing, never in apply/restore.

## Job state pitfalls

- `Preempted` is not terminal. It finalizes a run and returns to Pending (requeue) or goes through Completing (cancel). Code checking `is_terminal()` misses it; `is_finalized()` covers terminal plus `Preempted` and `Requeued`.
- `Running -> Requeued` is not a `transition()` edge. Admin requeue goes through `requeue_to_pending()`, kept off the general state machine so nothing else can resurrect a finished run.
- WAL apply uses `apply_transition()` (same-state move is `NoOp` for replay); live paths use `transition()`.
- Use `occupies_resources()` for quota/occupancy checks, not `state == Running`. A Pending job with a reserved placement also occupies capacity.
- `begin_time` holds serve three purposes: user `--begin`, preemption requeue cool-off, and launch-failure backoff. Know which one you are clearing.
- `run_attempt` is a per-dispatch epoch that stops stale completion reports from discharging a requeued job's new run. WAL ops that interact with a run should carry and gate on it. Not all do: `JobComplete`, `JobStepComplete`, `JobLaunchFailureDetail`, `JobCancelSignaled`, `JobSuspend`, `JobResume`, `JobUserRequeue`, and `JobEvict` do not. `JobTimeLimitSignaled` is legacy (no live proposer); the live path `JobTimeLimitExpired` carries it.
- "Reported" and "charged" differ on a per-node run. `allocated_nodes` is the whole allocation until the run resets; a node that reported may already be handed back. To ask whether the job occupies a node now, use `is_held_on` / `held_node_names` (charged, includes `retained_nodes`), not `allocated_nodes`. `still_runs_on` / `running_nodes` (not yet reported) and `released_nodes` (slice returned) exclude retained nodes. `clear_node_reports` clears `retained_nodes` with `node_completions`, so free their slices first.
- `reported_outcome()` is the single source of a run's state, exit code, and signal: `first_failed_node` if set, else the first allocated node. Both the last report and the completing timeout's force-finish use it. Once `first_failed_node` is recorded only its OOM counts; without one any node's OOM counts, as older controllers apply it, so replayed logs agree.
- `Job::script_placement()` falls back to the `auto` text scan when `resolved_placement` is unset (a run dispatched before placements were recorded), matching what agents without the `script-placement` capability do. `Job::clear_placement` clears it with the rest of the placement.

## Node state pitfalls

- `Draining -> Drain` is driven by `update_state_from_alloc()` when the last allocation is released, not by `NodeState::transition()`. `update_state_from_alloc()` leaves admin hold states untouched except `Draining -> Drain`.
- `admin_locked` blocks `HeartbeatRecovered` auto-recovery on a node an operator explicitly drained.

## Resources and core layout

- Device allocation keys on `stable_id`, never the positional `Device.index` (see [gpu-discovery.md](../../docs/knowledge/devices/gpu-discovery.md)). Comparing positions across allocation boundaries double-books devices after a mid-set removal.
- A gres a node reports devices for is allocated by id (`allocates_by_id`); any other gres draws on the countable `generic` pool. In `ResourceSet::holds`, a count-only `{device_id: 0}` hold from builds before NICs were reported by id counts as anonymous; the GPU unknown-id rule is in [gpu-discovery.md](../../docs/knowledge/devices/gpu-discovery.md#5-repartition-and-renumbering-safety).
- At most one GPU request form (total, per-node, per-task) may be set; a `gpu:` entry in `gres` counts as implicit per-node (`resolve_gpu_demand()`).
- `CoreLayout::compute()` keeps a GPU's previous `reserved_picks` while still valid; if that sticky fill cannot fit every GPU, all GPUs are re-picked fresh.
- `CoreLayout::fingerprint()` is a wire value the agent compares against its held layout, so its encoding must stay stable across builds. `0` means "no layout held" and must never be produced by a layout.
- `restart_only_node_changes()` makes live `reconfigure` reject `[[nodes]]` changes that alter a layout (spec cores, `restricted_cores_per_gpu`): applied live, leader and followers would differ until each recomputed, so isolation could change on failover. Such changes need a full controller restart.

## Config

- `preempt_victim_order` is reloaded live by `scontrol reconfigure`, including when cleared. Ranking semantics: [victim selection](../../docs/knowledge/scheduler/scheduling.md#victim-selection).
- `AuthConfig::resolved_jwt_key()` is resolved once at controller startup and is not re-read by `reconfigure`.
- `is_valid_cidr` / `cidrs_overlap` are IPv4-only because the k0s provisioning paths are all `Ipv4Addr`; an IPv6 CIDR would pass validation and fail at runtime.

## Security

- `bind_job_spec()` overwrites spec `user`/`uid`/`gid` from the verified identity; never trust client-sent values.
- Exceeding a QOS submit-count limit in `check_qos_submit_limits()` denies at submit regardless of `DenyOnLimit`, because admitting the job would itself increment the counted quantity.
- Hook scripts (`run_hook()`) must be absolute paths and are ownership/permission-checked via fstat on the open handle, closing the check-then-exec race.
- The job_submit hook may only modify whitelisted fields (`SUBMIT_HOOK_WHITELIST`). Identity and resource-count fields are absent by construction so a hook cannot forge them.
