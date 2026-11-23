# AGENTS.md -- spur-metrics

Cluster metrics aggregation and OpenMetrics 1.0 text export for `spurctld`.

## Overview

`spur-metrics` collects point-in-time snapshots from the controller's in-memory
state (jobs, nodes, partitions, scheduler, RPC stats, k0s cluster) and encodes
them as OpenMetrics 1.0 text for Prometheus scraping. Two dependencies:
`spur-core` (domain types) and `prometheus-client` (registry/encoding).

Architecture: snapshot structs (`*Snapshot`) are built lazily on each scrape by
scanning Raft-backed maps. Export functions build a fresh `Registry`, register
gauges/counters from the snapshot, encode to text, and return a `String`. The
one exception is `K8sMetrics` -- a long-lived accumulator whose families are
registered once at construction (counters/histograms can't be rebuilt from a
point-in-time snapshot).

Content-Type for all responses: `application/openmetrics-text; version=1.0.0; charset=utf-8`

## Module layout

| Module | Purpose |
|---|---|
| `scheduler` | `SchedStatsSnapshot` -- cycle counts, timing, job lifecycle counters |
| `job` | `JobMetricsSnapshot` -- per-state counts, held-pending, running resource sums |
| `node` | `NodeMetricsSnapshot` + `PerNodeMetrics` -- state counts, resource totals, per-node telemetry |
| `partition` | `PartitionMetricsSnapshot` -- per-partition job/node state and resource rollups |
| `rpc` | `RpcStatsSnapshot` / `RpcOperationSnapshot` -- per-operation call count and timing |
| `user_acct` | `UserAcctMetricsSnapshot` -- per-user and per-account job breakdowns (high-cardinality, opt-in) |
| `karma` | `KarmaMetricsSnapshot` -- per-user karma counters (jobs, GPUs, walltime); reset on leadership gain |
| `k8s` | `K8sClusterMetricsSnapshot` (gauges) + `K8sMetrics` (lifecycle counters/histograms) |
| `checkpoint` | `CheckpointMetrics` (long-lived run-end/restored-run/preempt-grace/dropped-event counters+histograms) + `CheckpointJobSnapshot` (per-job gauges) + `CheckpointJobFailed` (per-job, per-reason invalid-event counter) |
| `export/` | OpenMetrics registration and encoding for each domain |

## Metric naming conventions

All metrics use the `spur_` prefix. Pattern: `spur_{domain}_{resource}[_{qualifier}]`.

- **Gauges** -- point-in-time values rebuilt each scrape. No `_total` suffix.
- **Counters** -- monotonic since process start (exception: karma counters reset
  on leadership gain). `prometheus-client` auto-appends `_total` on encode.
- **Histograms** -- `_seconds` suffix for durations; exponential buckets.

State-based metrics use a fixed suffix per enum variant (e.g. `spur_jobs_pending`,
`spur_nodes_idle`). Suffix functions: `job_state_metric_suffix()`, `node_state_metric_suffix()`.

## Exported metrics catalog

### Scheduler (`/metrics/scheduler`)

| Metric | Type | Description |
|---|---|---|
| `spur_scheduler_info{plugin}` | gauge | Active scheduler plugin (always 1) |
| `spur_scheduler_cycles` | counter | Scheduling cycles since reset |
| `spur_scheduler_cycle_last_time_us` | gauge | Most recent cycle wall time (us) |
| `spur_scheduler_cycle_total_time_us` | gauge | Cumulative cycle wall time (us) |
| `spur_scheduler_schedule_last_time_us` | gauge | Most recent schedule() time (us) |
| `spur_scheduler_schedule_total_time_us` | gauge | Cumulative schedule() time (us) |
| `spur_scheduler_jobs_submitted` | counter | Jobs submitted |
| `spur_scheduler_jobs_started` | counter | Jobs started |
| `spur_scheduler_jobs_finalized` | counter | Jobs reaching terminal state |
| `spur_scheduler_jobs_preempted` | counter | Jobs preempted (cancel/requeue) |
| `spur_scheduler_jobs_started_last_cycle` | gauge | Jobs started in most recent cycle |
| `spur_scheduler_exit_end` | counter | Cycles that considered all pending jobs |
| `spur_scheduler_exit_max_depth` | counter | Cycles stopped at max_jobs_per_cycle |

### Jobs (`/metrics/jobs`)

| Metric | Type | Description |
|---|---|---|
| `spur_jobs` | gauge | Total jobs |
| `spur_jobs_{state}` | gauge | Per-state count (pending, running, completing, completed, failed, cancelled, timeout, node_fail, preempted, suspended, deadline, out_of_memory, requeued) |
| `spur_jobs_hold` | gauge | Pending jobs with a scheduling hold |
| `spur_jobs_cpus_alloc` | gauge | CPUs allocated to Running/Completing |
| `spur_jobs_memory_alloc_bytes` | gauge | Memory allocated to Running/Completing |
| `spur_jobs_gpus_alloc` | gauge | GPUs allocated to Running/Completing |

### Nodes (`/metrics/nodes`)

Aggregate gauges: `spur_nodes`, `spur_nodes_{state}` (idle, alloc, mixed, down, drain, draining, error, unknown, suspended), `spur_nodes_cpus`, `spur_nodes_cpus_alloc`, `spur_nodes_memory_bytes`, `spur_nodes_memory_alloc_bytes`, `spur_nodes_gpus`, `spur_nodes_gpus_alloc`, `spur_nodes_not_responding`, `spur_nodes_liveness_breaker_tripped` and `spur_nodes_silent_answering` (both set by spurctld, not `collect`).

Per-node families (label `node`): `spur_node_cpus`, `spur_node_cpus_alloc`, `spur_node_memory_bytes`, `spur_node_memory_alloc_bytes`, `spur_node_gpus`, `spur_node_gpus_alloc`, `spur_node_cpu_load`, `spur_node_free_memory_bytes`.

### Partitions (`/metrics/partitions`)

Scalar: `spur_partitions`. Per-partition families (label `partition`): `spur_partition_jobs`, `spur_partition_jobs_{job_state}`, `spur_partition_nodes`, `spur_partition_nodes_{node_state}`, `spur_partition_cpus`, `spur_partition_cpus_alloc`, `spur_partition_memory_bytes`, `spur_partition_memory_alloc_bytes`.

### RPC (`/metrics/rpc`)

Per-operation families (label `operation`): `spur_rpc_stats` (call count), `spur_rpc_stats_avg_time` (avg us), `spur_rpc_stats_total_time` (cumulative us).

### Auth (`/metrics/auth`)

Counters: `spur_auth_verify_ok`, `spur_auth_verify_fail`, `spur_auth_replay_reject`, `spur_exec_verify_ok`, `spur_exec_verify_fail`, `spur_rbac_role_deny`.

### Audit (`/metrics/audit`)

Counters: `spur_audit_rows_written`, `spur_audit_rows_dropped`.

### Per-user/account (`/metrics/jobs-users-accts`)

Opt-in via `metrics.high_cardinality`. Per-user (label `username`): `spur_user_jobs`, `spur_user_jobs_{state}`, `spur_user_jobs_cpus_alloc`, `spur_user_jobs_memory_alloc_bytes`, `spur_user_jobs_gpus_alloc`. Per-account (label `account`): same pattern with `spur_account_` prefix.

### Checkpoint telemetry (appended to `/metrics/jobs`, per-job gauges on `/metrics/jobs-users-accts`)

Long-lived `CheckpointMetrics` accumulator (same pattern as `K8sMetrics`, but registered fresh
per encode from cloned `Family` handles via `export::encode_registered` rather than a persistent
`Registry`): `spur_job_runs_ended{reason,checkpoint}` counter, checkpoint ∈ `none|completed|in_progress`;
`spur_job_runs_restored{reason}` counter, incremented once per finalized run that reported a
restore (any record with `restore_step` set), regardless of whether it had any other event;
`spur_job_run_end_seconds_since_last_checkpoint{reason}` and `spur_job_run_checkpoints_completed{reason}`
histograms; `spur_preempt_grace_checkpoint{outcome}` counter, outcome ∈ `completed|failed|none`;
`spur_checkpoint_events_dropped{reason}` counter for delivery/attribution drops
(`not_job_process`, `uid_mismatch`, `read_timeout`, `rate_limited`, `busy`); `spur_job_run_checkpoints_failed{reason}`
counter for events rejected as invalid (`unknown_kind`, `unsupported_version`, `invalid_status`,
`unknown_field`, `invalid_field`, `malformed_json`, `oversize`, `job_mismatch`) --
`spur_core::checkpoint::DropReason::is_invalid` is the classifier spurctld uses to route a wire
reason name to one family or the other. No series is pre-seeded -- only observed
`(reason, checkpoint)` pairs, outcomes, and reasons appear. `CheckpointMetrics::add_dropped` /
`add_invalid` are no-ops for `n == 0` so a reason with zero events never creates a series. A run
that never reported a single checkpoint event is not recorded in `spur_job_runs_ended` (or either
histogram, or `spur_preempt_grace_checkpoint`) at all -- a failed-only run still counts, labeled
`checkpoint="none"` like today.

Every per-job checkpoint series uses the same identity label set --
`job_id, user, account, job_name` -- nothing is a label unless it identifies the job or the
application: `namespace` is redundant with `account` (it's derived from it), and `partition`/`qos`
describe where a job ran, not what it is. `job_name` is sanitized before use as a label -- control
characters become `_`, then the value is truncated to 64 chars on a char boundary
(`export::checkpoint::sanitize_job_name`); the OpenMetrics encoder separately escapes any quote or
backslash that survives sanitization.

Per-job gauges (label `job_id, user, account, job_name`), built fresh per scrape from a
`&[CheckpointJobSnapshot]` of jobs with a live (not yet finalized) run:
`spur_job_checkpoints_completed`, `spur_job_checkpoint_in_progress` (0/1),
`spur_job_checkpoint_events_dropped` (always present), plus `spur_job_checkpoint_last_step`
and `spur_job_checkpoint_seconds_since_last` (sample omitted when the snapshot field is `None`).

Per-job, per-reason invalid-event counter `spur_job_checkpoints_failed{job_id,user,account,
job_name,reason}`, built fresh per scrape from a `&[CheckpointJobFailed]`
(`export::checkpoint::register_checkpoint_job_failed_metrics`; replaces the old live-only
`spur_job_checkpoint_events_failed` gauge). Only `(job, reason)` pairs with a nonzero count ever
appear. Values are cumulative per `job_id` across every run attempt (monotonic through a requeue,
never reset by a new attempt) -- `spurctld`'s `CheckpointStore::job_failed_snapshots` is what
decides, per job, whether to report that cumulative total from a live run or from a retained
finalized run's bookkeeping entry -- see that crate's `AGENTS.md`.

### Kubernetes (`/metrics/k8s`)

Cluster gauges (labels `distribution`, `cluster`): `spur_k8s_cluster_phase{phase}` (one-hot), `spur_k8s_cluster_up`, `spur_k8s_control_plane_replicas`, `spur_k8s_nodes_total`, `spur_k8s_nodes_by_role{role}`.

Lifecycle counters/histograms (long-lived `K8sMetrics` accumulator): `spur_k8s_provision_attempts`, `spur_k8s_provision_failures`, `spur_k8s_phase_transitions{from,to}`, `spur_k8s_reconcile_errors`, `spur_k8s_reconcile_duration_seconds`, `spur_k8s_drift_detected{kind}`, `spur_k8s_drift_corrected{kind}`, `spur_k8s_drift_refused{reason}`, `spur_k8s_drift_recurred`, `spur_k8s_start_failures`.

Per-node (label `node`): `spur_k8s_node_up`, `spur_k8s_node_restart`, `spur_k8s_install_duration_seconds`.

### Karma (appended to `/metrics/jobs-users-accts`)

Karma counters are appended to the `/metrics/jobs-users-accts` response body
(via `concat_encoded`), gated by `metrics.high_cardinality`. There is no
separate `/metrics/karma` route.

Per-user counters (label `username`): `spur_karma_jobs_submitted`, `spur_karma_jobs_completed`, `spur_karma_jobs_failed`, `spur_karma_jobs_timeout`, `spur_karma_jobs_node_fail`, `spur_karma_jobs_cancelled`, `spur_karma_gpus_requested`, `spur_karma_overflow_borrows`, `spur_karma_walltime_requested_seconds`, `spur_karma_walltime_actual_seconds`.

Karma counters are reset on leadership gain (`scheduler_loop.rs`): when a
follower becomes leader, `reset_karma_stats()` clears all user counters.
Prometheus `increase()` handles the reset transparently, but the counters
are not monotonic since process start.

## Endpoint behavior

- `/metrics` is an alias for `/metrics/jobs`.
- `/metrics/jobs-users-accts` returns 404 when `metrics.high_cardinality` is off
  (the check runs before the leader check).
- Every endpoint except `/metrics/auth` and `/metrics/audit` returns a not-leader error on followers. Scrape only the leader.

## Key implementation details

- `concat_encoded()` merges independently-encoded bodies by stripping intermediate `# EOF` markers and appending exactly one.
- State indices use proto wire discriminants (`to_proto_i32`) for stable array indexing.
- Memory is stored as MB internally; exported as bytes (`saturating_mul(1024 * 1024)`).
- `K8sMetrics::ensure_series()` pre-creates all drift/refusal counters at zero so alerting rules find them before the first event fires.
- `K8sMetrics::remove_node()` evicts per-node series to prevent stale data from decommissioned nodes.
