# AGENTS.md -- spur-sched

GPU-first job scheduling for the Spur cluster manager. This crate contains the
scheduling algorithm, node matching, resource timeline, priority calculation,
and per-node allocation tracking. It is a library crate consumed primarily by
`spurctld` (the controller daemon) via `scheduler_loop.rs`.

## Overview

`spur-sched` implements a **backfill scheduler** modeled after Slurm's
`sched/backfill` + `select/cons_tres` plugins. The controller calls
`Scheduler::schedule()` each cycle with the pending job queue (sorted by
effective priority, highest first) and a snapshot of cluster state. The
scheduler returns a list of `Assignment` values -- each binding a job to
specific nodes with concrete per-node CPU core IDs, GPU stable IDs, and memory.

The scheduling loop in spurctld (`scheduler_loop.rs`) owns the `BackfillScheduler`
instance, builds a `ClusterState` snapshot each cycle, feeds pending jobs in
priority order, and dispatches the resulting assignments to agents.

## Directory structure

```
src/
  lib.rs          -- crate root; re-exports UNLIMITED_JOB_DURATION
  traits.rs       -- Scheduler trait, Assignment, ClusterState
  priority.rs     -- effective_priority() multi-factor calculation
  node_match.rs   -- NodePlacement: constraint/nodelist/exclude/partition/reservation matching
  backfill.rs     -- BackfillScheduler: the main scheduling algorithm (~850 lines + ~2600 lines of tests)
  timeline.rs     -- NodeTimeline: per-node resource interval tracking with sweep-line queries
  cons_tres.rs    -- NodeAllocation: per-node consumable TRES (core/GPU/memory) allocation ledger
```

## Scheduling algorithm

### Priority calculation (`priority.rs`)

```
effective_priority = base_priority * fair_share_factor * age_factor * partition_tier
```

- **age_factor**: linear ramp from 1.0 to 2.0 over 7 days (10080 minutes).
- **fair_share**: capped at 10.0 to prevent runaway boosts.
- **partition_tier**: multiplier per partition (minimum 1).
- Result is floored at 1 (never zero).

spurctld sorts pending jobs by effective priority before handing them to the
scheduler. The scheduler itself does not re-sort; it trusts the input order.

### Backfill pass (`backfill.rs`)

For each pending job in priority order:

1. **Find suitable nodes** -- `find_suitable_nodes()` filters by partition,
   features, nodelist/exclude, reservation authorization, node state, k0s
   reservation, and total resource capacity (`can_satisfy`).

2. **Compute earliest start per node** -- `earliest_valid_start()` combines
   the timeline's resource availability with reservation conflict checks,
   searching forward from `now` until a contiguous `[start, start+duration)`
   window is free of both resource and reservation conflicts.

3. **Sort candidates** -- by earliest start time, then free GPUs (descending),
   then node weight (descending), then least allocated CPUs. `--spread-job`
   replaces the default sort with: earliest start, then most free GPUs
   (descending), then least allocated CPUs (ascending) — dropping node weight.

4. **Topology reordering** -- for multi-node jobs with `--topology=tree` or
   `block`, candidates are reordered by `TopologyTree::select_local_nodes()`
   to prefer nodes under the same switch.

5. **Additive nodelist pinning** -- if `--nodelist` names fewer nodes than
   `--nodes` requests (additive mode), listed nodes are forced to the front
   of the assignment, and the job waits if any listed node is unavailable.

6. **Common-window convergence** -- for multi-node jobs, the per-node earliest
   starts are unified into a single start time where ALL assigned nodes are
   simultaneously free. This is an iterative convergence loop (re-validating
   each node at the shifted time against both resources and reservations).

7. **Per-node allocation** -- two paths:
   - **Uniform (fast path)**: all nodes get identical CPU/GPU counts.
     `build_node_allocation()` picks concrete device IDs.
   - **Heterogeneous**: uneven task splits (e.g. `-N2 -n7` -> [4,3] CPUs) or
     `--gpus=total` require `plan_per_node_alloc()`, which resolves concrete
     per-node counts against free capacity at the actual start time.

8. **Start or reserve** -- if `earliest <= now`, the job starts (assignment
   emitted, timeline updated). Otherwise, a shadow reservation is placed on
   the timeline so lower-priority jobs cannot steal the slot.

9. **Idle-fill (borrow) jobs** -- jobs marked `idle_fill` (from borrowed
   capacity) never get a future slot; they only start if capacity is free now.

### Heterogeneous (het) jobs

Het job components (linked by `het_job_id`) are gang-scheduled: if any
component cannot find enough suitable nodes, the entire group is skipped.
The pre-check runs `find_suitable_nodes()` for each component independently;
actual timeline contention may still prevent placement even if the pre-check
passes.

### GPU placement modes

GPU demand is resolved via `spur_core::gpu_request::resolve_gpu_demand()`:

- **`GpuDemand::PerNode`** -- `--gres=gpu:N` or `--gpus-per-task` scaled by
  tasks-per-node. Each node gets its positional count.
- **`GpuDemand::Total`** -- `--gpus=N` (job-total). GPUs are packed greedily
  onto the most-available nodes via `distribute_total()`.
- **`GpuDemand::None`** -- no GPU request.

The `homogeneous_per_node()` helper determines whether the fast path (same GPU
count on every node) or the heterogeneous allocation path is needed.

## Resource matching (`node_match.rs`)

`NodePlacement` parses a job's placement constraints once and evaluates them
against candidate nodes:

| Check | Method | Notes |
|---|---|---|
| Name allow/deny | `allows_name()` | `--nodelist` and `--exclude`, hostlist-expanded |
| Partition membership | `in_partition()` | OR across `--partition` values |
| Feature constraints | `has_features()` | AND across `--constraint` features |
| Reservation authorization | `reservation_ok()` | Active reservations block unreserved jobs |
| k0s cluster exclusion | `matches()` | Nodes with a k0s role are never scheduled |
| Schedulable state | `matches()` | Node must be Idle/Mixed/Allocated, not Down/Drain |
| Exclusive idle check | `matches()` | `--exclusive` jobs require zero allocated resources |

`eligible()` = name + partition + features + reservation (load-independent).
`matches()` = eligible + schedulable + exclusive-idle + k0s gate.
`matches_for_reservation()` = eligible + up-state (admits busy nodes for future slots).

### Additive vs restrictive nodelist

When `--nodelist` names fewer nodes than `--nodes` requests, the nodelist is
**additive**: those nodes must be included, and the scheduler fills the
remainder from the general pool. When the nodelist covers or exceeds the
request, it is **restrictive**: only named nodes are candidates. The
`preferred_nodes` field (set by QOS/account group-node admission) provides
a fallback nodelist when the user did not specify `--nodelist`.

`new_ignoring_preferred_nodes()` exists for admission's own reuse-credit
computation, preventing the account gate's credit from leaking into the QOS
gate's independent placement.

## Timeline (`timeline.rs`)

`NodeTimeline` tracks when resources become available on a single node using
a sorted list of `Interval` values (start, end, allocated resources, optional
job ID).

Key operations:

- **`accumulated_at(time)`** -- sum of resources held by all intervals active
  at `time` (half-open: `start <= time < end`).
- **`earliest_start(request, duration, after)`** -- sweep-line search for the
  first contiguous `[t, t+duration)` window where `total - accumulated >= request`.
  Uses `AllocationSweep` (a min-heap of interval end times) for O(n log n)
  incremental evaluation instead of O(n^2) brute force.
- **`reserve_for_job()`** -- insert an interval (shadow reservation for a
  pending job, or a real allocation for a running job).
- **`gc(horizon)`** -- drop intervals ending at or before `horizon`.

`UNLIMITED_JOB_DURATION` (365 days) is the assumed occupancy for jobs with no
`--time` limit, shared between backfill and spurctld's busy_until estimates.

## Consumable TRES (`cons_tres.rs`)

`NodeAllocation` is the per-node allocation ledger on each agent (spurd). It
tracks which specific CPU cores and GPU stable IDs are allocated, with per-job
ownership for release and reconciliation.

### Allocation lifecycle

1. **`allocate_for_job()`** -- reserve CPU cores (first-fit bitmap), memory,
   and GPU stable IDs. Marks the job as `launching` (exempt from reconcile
   until committed or TTL-expired). Rejects duplicates (`DuplicateJob`),
   unknown/in-use GPUs (`GpusUnavailable`), and stale attempts (`Superseded`).

2. **`commit_job()`** -- mark a launch as committed (process is tracked).
   Clears the launching exemption so reconcile can reclaim if the job
   disappears. Returns false if the reservation was reclaimed or superseded.

3. **`release_job()` / `release_job_if()`** -- free the job's cores, memory,
   and GPUs. `release_job_if()` guards against releasing a different attempt's
   reservation (stale caller safety).

4. **`restore_for_job()`** -- replay an allocation after agent restart. Cores
   are replayed verbatim (narrowed with `claimable_cpu_ids()`). Committed
   outright (not launching) since the job is already running.

5. **`reconcile()`** -- reclaim orphaned allocations (job not in live set and
   not within launching TTL). Recovers from failed teardowns or dropped launches.

### run_attempt ordering

Each allocation carries a `run_attempt` counter. A newer attempt supersedes
an older one (preempted-then-requeued job); an older attempt cannot clobber a
committed newer one (`Superseded` error). This prevents a stale LaunchJob from
stealing a live job's resources.

### Capacity updates

`update_capacity()` applies a fresh hardware inventory (e.g. GPU hot-add/remove).
If any held GPU stable ID is missing in the new inventory, the update is rejected
(`AllocatedDevicesLost`) and capacity is left untouched -- the caller must drain
or fail the affected jobs first.

## Key types

| Type | Module | Role |
|---|---|---|
| `Scheduler` | `traits` | Trait for pluggable scheduler implementations |
| `Assignment` | `traits` | Job-to-nodes binding with per-node allocations |
| `ClusterState` | `traits` | Snapshot of nodes, partitions, reservations, topology, busy_until |
| `BackfillScheduler` | `backfill` | The production scheduler implementation |
| `PlannedJobStarts` | `backfill` | Future slot plan: job -> (nodes, projected start) |
| `NodePlacement` | `node_match` | Parsed job placement constraints |
| `NodeTimeline` | `timeline` | Per-node resource interval list |
| `Interval` | `timeline` | Time window with allocated resources |
| `NodeAllocation` | `cons_tres` | Per-node CPU/GPU/memory bitmap ledger |
| `AllocationResult` | `cons_tres` | Concrete core IDs, GPU stable IDs, memory |
| `AllocError` | `cons_tres` | Why an allocation was rejected |
| `CapacityChange` | `cons_tres` | Result of applying a fresh inventory |

## Testing

The crate is heavily unit-tested (each module has `#[cfg(test)] mod tests`).
`backfill.rs` alone has ~2600 lines of tests covering:

- Single/multi-node scheduling, GPU placement (per-node and total modes)
- Disjoint GPU device ID assignment across same-cycle jobs
- Nodelist (restrictive, additive, hostlist expansion, bracket ranges)
- Exclude lists, feature constraints, multi-partition OR matching
- Reservation enforcement (authorized/unauthorized users, future reservations,
  prospective overlap blocking)
- Het job gang scheduling (all-or-nothing, insufficient nodes)
- Spread-job least-loaded preference, node weight ordering
- Shadow reservation correctness (large job blocks overlapping smaller jobs,
  does not block non-overlapping ones, busy_until realistic durations)
- Uneven CPU splits (block distribution), memory-per-cpu scaling
- Topology-aware placement with additive nodelists
- Common-window convergence (multi-node future reservations respect reservation
  windows across both uniform and heterogeneous allocation paths)

Integration tests live in `spur-tests`:
- `t07_sched.rs` -- end-to-end scheduling with real cluster state
- `t24_priority.rs` -- effective priority calculation
- `t39_gpu.rs` -- GPU scheduling with typed GPUs and multi-node placement
- `t50_core.rs` -- core allocation with scheduler
- `t01_run.rs` -- cons_tres allocation/release lifecycle

Run unit tests: `cargo test -p spur-sched`

## Common pitfalls

1. **Input order matters.** The scheduler does not re-sort pending jobs. If
   spurctld passes them in wrong priority order, higher-priority jobs may get
   shadow-reserved after lower-priority ones consume their slots.

2. **busy_until vs flat placeholder.** Nodes without a `busy_until` entry get
   a 24-hour placeholder. This is conservative but can cause small jobs to be
   unnecessarily deferred when a large exclusive job is pending. Always populate
   `busy_until` from running job time limits for accurate backfill.

3. **GPU allocation uses stable IDs, not device IDs.** The `device_id` field
   is positional (can change across reboots); `stable_id` is durable. All
   allocation, release, and capacity-update operations key on stable IDs.
   Confusing the two causes silent double-allocation or leaked GPUs.

4. **Heterogeneous vs uniform path.** The scheduler takes a fast uniform path
   when all nodes get the same CPU and GPU counts. An uneven task distribution
   (`-N2 -n7`) or `--gpus=total` forces the heterogeneous path, which resolves
   per-node counts against actual free capacity. A bug in the path selection
   (`homogeneous_per_node` or `cpu_is_heterogeneous`) silently over- or
   under-allocates.

5. **Common-window convergence for multi-node jobs.** The per-node earliest
   starts are computed independently, then unified. The convergence loop must
   re-validate each node at the shifted time, including reservation conflicts.
   Without this, a multi-node future reservation can land inside an
   unauthorized reservation window on one of the nodes.

6. **Additive nodelist semantics.** When `num_nodes > nodelist.len()`, the
   listed nodes are mandatory (the job waits if any is unavailable), and extra
   nodes are filled from the general pool. If a listed node is eligible but
   not schedulable (e.g. Draining), the entire job is held -- it does not
   substitute from the pool. This is intentional (matches Slurm behavior).

7. **launching TTL in cons_tres.** A reserved-but-uncommitted allocation is
   exempt from reconcile until the TTL expires. If the TTL is too short,
   reconcile reclaims a slow launch; too long, and a dropped launch pins
   resources. The caller (spurd) must set this to match the maximum expected
   launch latency.

8. **k0s reservation gate.** Nodes assigned a k0s role (Worker, Controller,
   Single) are unconditionally excluded from scheduling via `is_k0s_reserved()`.
   This gate is in `matches()` but not `eligible()`, so pending-reason
   classification can distinguish "would match if not k0s-reserved" from
   "doesn't match at all."

9. **Exclusive jobs time against full node capacity.** When computing
   `earliest_start` for an `--exclusive` job, the timeline query uses the
   node's total resources, not the job's own modest request. This prevents
   a small exclusive job from starting while another job still occupies part
   of the node.

10. **Idle-fill (borrow) jobs never hold future slots.** A borrowed job that
    cannot start now is silently dropped (no shadow reservation, no
    StartTime/SchedNodeList published), since it has no claim on capacity.
