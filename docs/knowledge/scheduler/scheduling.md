# Scheduling Internals

Source: `spur-sched/src/` (backfill, priority, node_match, timeline, cons_tres) and
`spurctld/src/scheduler_loop.rs`.

---

## 1. Algorithm Overview

Spur uses a **backfill scheduler** with priority ordering. Each cycle:

1. Sort pending jobs by effective priority (highest first).
2. For each job, compute its earliest start time across all suitable nodes.
3. If a job can start now, assign it and commit its resources to the timeline.
4. If a job cannot start now, create a **shadow reservation** (future slot) so
   lower-priority jobs cannot steal its projected capacity.
5. Lower-priority jobs backfill into gaps that do not delay any shadow reservation.

The scheduler loop (`scheduler_loop.rs`) is event-driven: it wakes on job
submission via `scheduler_notify` OR on a periodic tick (`interval_secs`).
Leadership is Raft-gated -- only the leader runs the scheduling pass.

Key invariant: a high-priority job that cannot start now holds a future
reservation on the timeline. A lower-priority job may only start if it finishes
before that reservation begins.

The cycle cap (`max_jobs_per_cycle`) bounds the number of pending jobs evaluated
per tick, preventing O(n^2) blowup on large queues.

---

## 2. Priority Calculation

Effective priority is a multiplicative composition of four factors
(`priority.rs`):

```text
effective_priority = base_priority * fair_share_factor * age_factor * partition_tier
```

| Factor | Range | Semantics |
|---|---|---|
| `base_priority` | u32 | Set at submission (user or admin) |
| `fair_share` | 0.0 -- 10.0 (capped) | Ratio of entitled share to actual usage |
| `age_factor` | 1.0 -- 2.0 | Linear ramp over 7 days (`age_minutes / 10080`) |
| `partition_tier` | >= 1 | Partition-level multiplier for tiered priority |

The result is floored at 1 (never zero). The fair-share cap of 10.0 is shared
between `priority.rs`, `fairshare.rs`, and the `sshare`/`sprio` CLI output so
all three agree on the effective range.

---

## 3. Node Matching

`NodePlacement` (`node_match.rs`) parses a job's placement constraints once and
evaluates them against every candidate node. The evaluation chain:

1. **Name filter** -- `--nodelist` (allow-list or additive requirement) and
   `--exclude` (deny-list). Hostlist bracket syntax (`node[001-003]`) is expanded.
2. **Partition membership** -- node must be in one of the job's `--partition`
   entries (OR semantics across partitions).
3. **Feature constraints** -- every `--constraint` feature must be present on the
   node (AND semantics: `mi300x,nvlink` requires both).
4. **Reservation access** -- if a reservation is active on the node, only jobs
   that name that reservation AND are authorized (user or account) may use it.
   Jobs without `--reservation` are blocked from reserved nodes.
5. **Runtime state** -- node must be schedulable (up, not draining), not claimed
   by the managed k0s cluster, and (for exclusive jobs) completely idle.

`matches_for_reservation()` relaxes the idle check so a busy-but-healthy node
can serve as a future-reservation candidate for the timeline.

**Nodelist semantics** -- when `num_nodes > len(nodelist)`, the nodelist is
*additive*: the named nodes are required, and extra nodes are filled from the
partition. When `num_nodes <= len(nodelist)`, the nodelist is *restrictive*: it
acts as a candidate pool. An additive nodelist where a listed node is
unavailable (down, draining, reserved) blocks the entire job.

**Preferred nodes** -- QOS/account admission can write `preferred_nodes` on a
job. `NodePlacement::new()` falls back to this field when no explicit
`--nodelist` was provided, so scheduling honors the admission credit.
`new_ignoring_preferred_nodes()` suppresses this fallback for admission's own
internal reuse computation to prevent cross-gate leakage.

---

## 4. GPU-First Scheduling

GPU demand is resolved into one of three shapes (`GpuDemand`):

| Shape | Source | Per-node behavior |
|---|---|---|
| `None` | No `--gres gpu` or `--gpus` | No GPU allocation |
| `PerNode` | `--gres gpu:N` or `--gpus-per-node` | Each node gets exactly N GPUs |
| `Total` | `--gpus=N` (job-wide total) | Greedy packing across nodes |

**PerNode** -- positionally aligned to the assigned node list. Uneven
`--gpus-per-task` across an uneven task split (e.g., 5 tasks / 2 nodes = [3,2]
tasks, 2 GPUs/task = [6,4] GPUs) produces non-uniform per-node counts.

**Total** -- capacity-sorted greedy: nodes are sorted by descending free GPU
count, and the total is packed onto the most-available nodes first (via
`distribute_total`). This means a `--gpus=8 -N4` job over four 8-GPU nodes
yields a [5,1,1,1] distribution, not [2,2,2,2].

Node sorting for placement: earliest start time first, then most free GPUs
(descending), then highest node weight, then least allocated CPUs. This ensures
GPU jobs land on the nodes with the most available accelerators.

For `--spread-job`, the tertiary sort is inverted: least-loaded (ascending
allocation) is preferred over node weight, distributing work across the cluster.

**Heterogeneous allocation path** -- when per-node CPU or GPU counts are
non-uniform, `plan_per_node_alloc()` resolves concrete allocations against each
node's free capacity at the job's actual start time. If any node cannot hold its
share, the entire placement is deferred.

---

## 5. Resource Timeline

`NodeTimeline` (`timeline.rs`) tracks per-node resource availability over time
as a sorted list of allocation intervals `[start, end)` with attached
`ResourceAllocations`.

**Key operations:**

- `accumulated_at(time)` -- sum of all allocations active at `time`.
- `earliest_start(request, duration, after)` -- find the first contiguous window
  of `duration` length where `request` fits, starting no earlier than `after`.
  Uses an `AllocationSweep` (min-heap of ending times) for incremental
  evaluation.
- `reserve_for_job()` -- commit an interval attributed to a specific job,
  enabling `planned_starts()` queries.

**Starvation prevention** -- when a high-priority job cannot start now, the
scheduler inserts a shadow reservation at its `earliest_start` on the timeline.
Subsequent lower-priority jobs must fit before that shadow's start or wait.
This guarantees forward progress for high-priority work.

**Convergence** -- for multi-node jobs, each node's earliest start is computed
independently, then the set converges to a common start (the max). This common
start is revalidated against reservation blockers on every node, iterating until
stable or hitting a 365-day horizon.

`UNLIMITED_JOB_DURATION` (365 days) is the assumed occupancy for jobs with no
`--time-limit`.

---

## 6. Consumable TRES

`NodeAllocation` (`cons_tres.rs`) tracks per-node resource ownership at
core/GPU granularity. This is the agent-side counterpart to the scheduler's
timeline -- the source of truth for what is actually allocated on each node.

**GPU tracking** uses **stable IDs** (durable across reboots, driver reloads)
rather than positional device IDs. This prevents a GPU renumbering event from
orphaning a live job's allocation.

**Lifecycle of an allocation:**

1. `allocate_for_job(job_id, run_attempt, cpus, mem, gpu_stable_ids)` -- reserves
   resources. Marked `launching` until committed.
2. `commit_job(job_id, run_attempt)` -- clears the launching marker; the
   allocation is now tracked as a committed, running reservation.
3. `release_job(job_id)` -- frees all resources held by the job.

**Duplicate and supersession guards:**

- A second `allocate_for_job` while the first is still `launching` returns
  `DuplicateJob`.
- A newer `run_attempt` supersedes a committed older attempt (handles
  preempt-then-requeue). A stale attempt trying to supersede a newer one returns
  `Superseded`.
- `release_job_if(job_id, run_attempt)` prevents a stale teardown from freeing a
  newer attempt's allocation.

**Reconciliation** -- `reconcile(live_jobs, now, ttl)` reclaims allocations whose
owning job is neither in the live set nor within the launching TTL. Recovers
resources stranded by failed teardowns or dropped launches.

**Capacity updates** -- `update_capacity(fresh_inventory)` rebuilds CPU/GPU
tables from a fresh hardware scan. If a stable ID held by a running job vanishes
in the new inventory, the update is rejected (`AllocatedDevicesLost`) so the
caller can drive policy (drain, fail) before mutating state.

---

## 7. Preemption

Preemption is gated on `preempt_type = qos_priority` in the scheduler config.
It runs after the backfill pass, only for unplaced jobs that have QOS preemption
rights.

**Eligibility** (`qos_preempt_allowed`):

- The pending job's QOS must explicitly list the victim's QOS in its
  `preempt` allow-list.
- If the two jobs share the same QOS, submit order breaks the tie (later
  submission wins).
- Otherwise, the pending QOS must have strictly higher priority.

**Exempt time** -- a running job is immune to preemption for its first N seconds
of execution (`preempt_exempt_time`). Resolution order: QOS override >
partition override > global config.

**Reservation protection** -- a job running inside an active reservation cannot
be preempted by a job from the same or lower partition tier.

**Victim selection** -- candidates are sorted by ascending QOS priority (cheapest
first). The scheduler proves that evicting the selected victim set would
actually place the pending job before committing.

**Reclaim vs preempt** -- reclaim (recovering idle-fill borrowed capacity) runs
before preemption. A job whose shortfall reclaim closes never reaches
`try_preempt`. This preserves the invariant that preemption only targets jobs
holding a legitimate claim.

**Double-eviction guard** -- nodes freed by a prior eviction are held out of
dispatch for the agent's kill grace period. `reclaim_in_flight` tracks which
reclaimer has capacity in flight to prevent a second victim being taken for a
shortfall already closed.

---

## 8. Fairshare

Fairshare factors are computed in `accounting/fairshare.rs` and cached in
`FairshareCache` (refreshed on a configurable interval from the accounting DB).

**Computation:**

1. Build an effective-shares tree via BFS from root accounts, splitting parent
   share proportionally by sibling `fairshare_weight`.
2. Per-user weight within an account further subdivides the account's share.
3. Historical usage (CPU + GPU seconds) is decay-weighted with a configurable
   half-life: the half-life is first converted to seconds
   (`Duration::days(halflife_days).num_seconds()`), then
   `decay = exp(-ln(2) / halflife_seconds * age_seconds)`.
4. The fair-share factor = `entitled_share / actual_share`, capped at 10.0.
5. Users with near-zero usage get capped at `share / epsilon`, preventing
   infinite priority.

The cache returns 1.0 (neutral) for unknown user/account pairs so new users
are not penalized.

**Association tree** -- accounts form a hierarchy (`parent` field). Root accounts
(no parent or dangling parent reference) are treated as top-level. Shares flow
down the tree proportionally.

---

## 9. Idle-Fill (Borrowed Capacity)

When `idle_fill_enabled` is set in the scheduler config, the scheduler loop
appends idle-fill candidates after all in-quota pending jobs.

**Semantics:**

- Idle-fill jobs are marked with `idle_fill = true` before entering the backfill
  pass.
- They may only start if capacity is free RIGHT NOW. Unlike normal jobs, they
  never hold a future reservation -- doing so would publish a contradictory
  `StartTime` for a job that simultaneously reports being over-quota.
- Idle-fill jobs are excluded from preemption eligibility checks: they have no
  claim on the capacity they occupy.
- Reclaim can terminate idle-fill jobs to recover capacity for in-quota work.
  The exempt period (`idle_fill_exempt_secs`) provides a short grace.
- When dispatched, the `borrowed` flag is threaded through to the assignment so
  the controller can set appropriate job annotations.

---

## 10. Heterogeneous Jobs

Het (heterogeneous) jobs are groups of linked job components that must be
co-scheduled. Components are linked by `het_job_id` (pointing to the anchor
component) and `het_group` (0 for anchor, 1+ for followers).

**Gang scheduling** -- the scheduler pre-checks all components of a het group.
If ANY component cannot find sufficient suitable nodes, ALL components are
skipped (`HetGroupIncomplete`). This prevents partial placement of a group
where one component runs and the others wait indefinitely.

Single-component het groups (only the anchor) are treated as normal jobs.

The pre-check verifies node suitability (features, partitions, capacity) but not
simultaneous availability. Actual scheduling may still defer a component if the
timeline cannot accommodate it alongside its siblings.

---

## 11. Edge Cases and Pitfalls

**Uneven task distribution** -- `-N2 -n7` produces a [4,3] task split across
nodes (block distribution). The scheduler reserves exact per-node CPU counts,
not the rounded-up [4,4] that would over-allocate. Memory-per-CPU requests scale
with each node's actual CPU share.

**Exclusive jobs and timeline sizing** -- exclusive jobs time their
earliest-start against the node's FULL capacity (not their own requested share),
so a 1-CPU exclusive job on a 64-CPU node blocks the entire node on the timeline.

**Additive nodelist after reduction** -- `sbatch -N4 -n1` triggers submit-time
reduction to 1 node. If a 4-entry `--nodelist` was also specified, it becomes
restrictive (candidate pool), not additive, because `num_nodes(1) <=
len(nodelist)(4)`.

**Topology-aware placement** -- for multi-node jobs with `--topology=tree` or
`--topology=block`, the scheduler reorders candidates by switch-tree locality
after the initial sort. Additive nodelist members are re-pinned after topology
reordering to prevent displacement.

**Future reservation convergence** -- a multi-node job's per-node earliest
starts are independently computed, then converged to a common start. The
common start must be revalidated against reservations on EVERY node, not just
the slowest, because shifting forward can push into a reservation window on
an otherwise-clear node.

**Prospective reservation overlap** -- a job whose `[start, start+duration)`
overlaps an unauthorized future reservation on any candidate node is blocked,
even if the reservation has not started yet. This prevents a long job from
being dispatched only to be killed when the reservation activates.

**Leadership transitions** -- after a Raft leadership change, the new leader
holds back dispatch for 2x the dispatch timeout to let in-flight RPCs from the
old leader land or time out. `relinquish_leadership()` clears planned
reservations and outcome memory. `should_sweep_orphaned_placements()` gates
the cleanup on the grace period having elapsed under the live term.

**GPU stable ID vs device ID** -- allocations key on stable IDs, not DRM device
IDs. A node whose GPUs renumber (driver reload, hot-add) preserves running
allocations as long as the stable IDs survive. If a held stable ID vanishes,
`update_capacity` rejects the update to prevent silent resource leaks.

**Reconcile TTL** -- a launching reservation (reserved but not committed) is
spared by reconcile for `launching_ttl` seconds. Past that, it is reclaimed as
an abandoned launch. A commit arriving after reclaim returns false, telling the
caller not to treat the job as backed by an allocation.
