# Scheduling Internals

Source: `spur-sched/src/` (backfill, priority, node_match, timeline, cons_tres) and
`spurctld/src/scheduler_loop.rs`.

---

## 1. Algorithm Overview

Spur uses a **backfill scheduler** with priority ordering. Each cycle:

1. Sort pending jobs: reservation holders first, then hoisted preemptors,
   then by descending effective priority, then by job_id. Idle-fill candidates
   are appended after all in-quota jobs.
2. For each job, compute its earliest start time across all suitable nodes.
3. If a job can start now, ask the pass's `StartAdmission` whether its limits
   hold on the nodes it got; if so, assign it and commit its resources to the
   timeline. A refused job starts nothing and holds nothing.
4. If a job cannot start now, create a **shadow reservation** (future slot) so
   lower-priority jobs cannot steal its projected capacity, unless
   `StartAdmission::may_reserve` refuses it as over its limits.
5. Lower-priority jobs backfill into capacity not held by running jobs or
   shadow reservations.

The scheduler loop (`scheduler_loop.rs`) is event-driven: it wakes on job
submission via `scheduler_notify` OR on a periodic tick (`interval_secs`).
Leadership is Raft-gated -- only the leader runs the scheduling pass.

Key invariant: a high-priority job that cannot start now holds a shadow
reservation (capacity slice) on the timeline. A lower-priority job may start
only if, for its whole duration, its request fits in capacity not held by
running jobs or shadow reservations.

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
can serve as a future-reservation candidate for the timeline. The backfill pass
gates on `matches_for_reservation()`; the plain `matches()` function feeds the
pending-reason classifier instead.

**Nodelist semantics** -- when `num_nodes > len(nodelist)`, the nodelist is
*additive*: the named nodes are required, and extra nodes are filled from the
partition. When `num_nodes <= len(nodelist)`, the nodelist is *restrictive*: it
acts as a candidate pool. An additive nodelist where a listed node is
unavailable (down, draining) blocks the entire job. A listed node inside
another active reservation is silently dropped from the candidate set.

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
scheduler inserts a shadow reservation (a capacity slice) at its
`earliest_start` on the timeline. A subsequent lower-priority job may start
only if, for its whole duration, its request fits in capacity not held by
running jobs or shadow reservations. This guarantees forward progress for
high-priority work.

**Convergence** -- for multi-node jobs, each node's earliest start is computed
independently, then the set converges to a common start (the max). This common
start is revalidated against reservation blockers on every node, iterating until
stable or hitting a 365-day horizon.

`UNLIMITED_JOB_DURATION` (365 days) is the assumed occupancy for jobs with no
`--time`.

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
tables from a fresh hardware scan. If a GPU stable ID or CPU core held by a
running job vanishes in the new inventory, the update is rejected
(`AllocatedDevicesLost`) so the caller can drive policy (drain, fail) before
mutating state.

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

### Victim selection

`scheduler_loop/preemption.rs` captures a coherent jobs/nodes
snapshot in the state machine's lock order and retains only placement/policy
metadata (including suspension timing and live node responsiveness) and still-held
resource slices. Both compact node copies preserve `not_responding`, so trials
reject unresponsive nodes through the same placement gate as normal scheduling.
`placement_job` retains `cpus_per_gpu`; `placement_node` retains `core_layout`
and `layout_blocked_cpus`. Allocation copies and trial resets preserve
`reserved_cpus` and `layout_fingerprint`. Release credit follows the node's
current layout, so an older-layout victim cannot release another holder's
reserved CPU charge. Immediate trials seed blocked GPUs and refresh timeline
CPU pools using the same rules as normal placement. Layout lists and blocked
GPU entries count toward the input and work bounds.
Protected, reserved-pending, suspended, completing, grace-period, and same-cycle
allocations remain charged. Unaccounted occupancy is retained; missing or
inconsistent slices grant no release credit.

For supported uniform or exclusive requests with single-node victims,
`direct_victims` uses the real `find_suitable_nodes` filter and calculates each
node's CPU, memory, typed/stable GPU and partition-cap shortfall. It examines
nodes sequentially, removes eligible allocations in QOS/configured order,
prunes unnecessary local victims, and ranks
the sufficient node groups with `VictimSearch`. The combined set gets one real
placement verification. Protected allocations and charges stay present. A
zero-victim result also needs verification.

#### Direct-selection eligibility

`direct_victims` checks these conditions after candidate validation and before
building local groups. Any matching condition chooses fallback:

| Condition | Scope |
| --- | --- |
| Unequal per-node CPU or GPU counts | Non-exclusive incoming jobs only |
| Positive `GpuDemand::Total` across multiple nodes | Non-exclusive incoming jobs, even when the total divides evenly |
| A candidate victim holds allocations on multiple nodes | Any job in the validated eligible list, even if it would not be selected |
| `NodePlacement::nodelist_is_additive()` | The incoming job requires named nodes plus additional nodes |
| A partition has a memory-policy field listed below | Any partition in the supplied scheduler state, including unrelated partitions |

The victim-width gate concerns the eligible list, not the incoming job's node
count. A protected multi-node holder does not trigger that gate, but its slices
remain allocated. `--exclusive` bypasses only the uneven-resource and GPU-total
shape gates; it does not bypass the victim-width, additive-list or memory-policy
gates. Node inventory must still satisfy the request.

#### Partition memory-policy gates

The memory-policy gate tests field presence, even when the job already specifies
memory or needs no adjustment. `Partition::has_memory_policy()` covers the first
four fields; `direct_victims` also checks `max_mem_per_node`. Values are in MiB.

| Partition field | Placement effect |
| --- | --- |
| `def_mem_per_node` | Supplies default memory per node when memory is omitted |
| `def_mem_per_cpu` | Supplies default memory per CPU when memory is omitted |
| `def_mem_per_gpu` | Supplies default memory per GPU when memory is omitted |
| `max_mem_per_cpu` | Can raise the job's CPU requirement to satisfy the memory-per-CPU limit |
| `max_mem_per_node` | Limits one job's memory request on one node |

An explicit job memory request and the cumulative partition cap
`max_tres_per_node.mem` do not trigger fallback by themselves. Local selection
already checks cumulative partition charges. The presence-based check over all
partitions is broader than checking whether the incoming request needs adjustment
in one of its candidate partitions.

#### Fallback search

Too few sufficient local groups or a failed final placement verification also
choose fallback. It checks unchanged and all-eligible projections, bisects an
ordered prefix while retaining a proven sufficient set, then repeatedly prunes
unnecessary victims. Placement is not assumed monotone under scheduler heuristics;
every accepted set has its own placement proof. The node-ranking heap supplies the
preferred group using QOS priority, `scheduler.preempt_victim_order`, then node
name. Suspension-aware work facts come from held jobs without accounting queries.
There is no globally minimum-victim guarantee.

#### Trial state and bounds

`PlacementProjection` owns the bounded `PlacementRequest`, compact node copies
and partition usage. Its static inputs cannot change between trials; only held
allocations and charges can decrease. `BackfillScheduler::place_releasing` reuses
prepared sizing and the production partition/allocation helpers without future
convergence or trial logs. Trial timelines are reused. `PlacementScope` reuses
name/partition/feature matching for overlap checks, including preferred-node
fallback. Injectable GPU counts and expanded placeholder type text are bounded
before allocation.

`MAX_NODES` (4096) bounds the node domain; `MAX_VICTIMS` (1024) bounds the final
selected set, not the eligible candidate pool. Candidate scanning and sorting
consume the work budget, and candidate references count toward copied metadata.
`InputSize` counts entries and text, with `MAX_ENTRIES` (327680, 64 per
node/victim allowance) and `MAX_TEXT` (5 MiB, 1024 bytes per node/victim allowance).
These are aggregate metadata limits, not an RSS estimate or a guarantee for every
possible node description. `MAX_HOSTS` (8192) and `MAX_NAME` (255 bytes) bound
hostlist expansion. A 4096-node, eight-GPU fixture exercises capture and selection
under the default limits.

Held-allocation traversal uses temporary membership sets for jobs wider than two
nodes, avoiding repeated long node-list scans. Snapshot construction, ownership
validation, and partition charges retain only allocated nodes without completion
reports. No additional persisted allocation index is introduced.

Each reclaim and QOS-preemption pass has its own `SearchBudget` with `MAX_TRIALS`
(2064, `2 * MAX_VICTIMS + 16`) placement calls and `MAX_WORK` (2000000) work units.
Requests within one pass share that budget. Snapshot/request rejection is logged;
`SelectionDeferred` distinguishes incomplete selection from a proven no-op and is
logged with the job ID. Incomplete searches evict nothing, even if pruning had a
sufficient intermediate set. Complex requests can still defer at these limits.
Reclaim skips snapshot construction when no pending request or eligible borrower
exists. Heterogeneous job groups and GPU property/fabric-binding requests defer.

#### Performance assessment

All releases in this comparison are hypothetical; only a verified final victim
set can authorize eviction. The eligibility gates above decide the current path.

```text
Earlier linear search                 Current search
---------------------                 --------------
Full trial: none, then all             Direct-path gates pass?
  |                                     | yes                | no
Add one victim                          v                    |
  -> full trial                       Per-node local checks  |
Repeat until fit                      + local pruning        |
  |                                   (sequential)           |
Restore each selected victim            |                    |
  -> full trial                       Rank enough groups     |
Repeat pruning until stable             |                    |
  |                                   One full verification  |
Accept verified set                     | fits     | fails   |
                                        v          v         v
                                      Accept      Bounded fallback:
                                                  none/all full trials
                                                  + prefix bisection
                                                  + full-trial pruning
```

**CPU trade-off:** local checks reduce full placement calls when the common path
succeeds, especially with many necessary victims. They still scan candidate
nodes and rank groups. Bisection reduces fallback growth checks for large
necessary prefixes, but can need more full calls than linear growth when one
early victim suffices. Incoming node count alone does not predict the winner:
eligible victim widths and partition policies determine which path runs.
For a supported one-node request needing all 64 victims, the loop counts are
130 full trials versus 130 local checks plus one full trial. Those operations
have different costs; this is not a measured 130-fold speedup.

**Memory trade-off:** selection uses temporary snapshots, per-node groups and
indices; it adds no persisted controller allocation map or Raft record. Trial
state is reused, so more calls do not mean more simultaneous snapshots. All
preparation and pruning remain bounded; metadata limits and selected payload
sizes are not total RSS limits or measurements of the net memory increase.

**Performance verdict:** fewer full placement calls are established; an overall
latency win for mostly one-/two-node workloads is not. Existing mixed-workload
measurements compare the later candidate/index processing within the current
algorithm, use fallback, and show similar selection latency and process RSS.
They exclude snapshot construction and dispatch, use an unoptimized build, and
do not compare the original linear search against the current method. A direct
comparison still needs identical victim layouts and partition policies, including
cases needing only one or two victims.

The [worked search-cost comparisons](../../plans/2026-11-16-resource-aware-victim-selection.md#worked-search-cost-comparisons),
[temporary-memory calculations](../../plans/2026-11-16-resource-aware-victim-selection.md#worked-temporary-memory-example),
and [recorded measurement scope](../../plans/2026-11-16-resource-aware-victim-selection.md#recorded-measurements-of-the-later-indexing-follow-up)
separate analytic operation counts, build-specific layouts and observed timings.

#### Reclaim ordering

**Reclaim vs preempt** -- reclaim (recovering idle-fill borrowed capacity) runs
before preemption. When reclaim frees nodes, the scheduler skips dispatching
assignments already computed that cycle (not just `try_preempt`) so the freed
capacity is re-evaluated cleanly. A job whose shortfall reclaim closes never
reaches `try_preempt`. This preserves the invariant that preemption only
targets jobs holding a legitimate claim.

**Double-eviction guard (reclaim-only)** -- nodes freed by a reclaim eviction
are held out of dispatch for the agent's kill grace period.
`reclaim_in_flight` tracks which reclaimer has capacity in flight to prevent a
second victim being taken for a shortfall already closed.

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
  The exempt period (`idle_fill_exempt_secs`) provides a short grace. Reclaim
  also serves idle-fill candidates: a higher-priority idle-fill job can
  displace strictly-lower-priority opportunistic work (the QOS priority
  ceiling prevents it from touching jobs with a real claim).
- When dispatched, the `borrowed` flag is threaded through to the assignment
  so the controller marks the job as running on borrowed capacity (visible in
  job annotations and accounting).

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

**Queue order** -- backfill places a group's components one after another, at
its highest-ranked component's place in the pending order, as Slurm does with
`bf_hetjob_prio=max`. spurctld's tiers count: a reservation or a hoisted
preemptor on one component moves the others up with it. No other job is planned
while a placed component's nodes wait on the rest, so a group refused at start
frees its nodes before the next job is planned.

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
`--topology=block`, the scheduler prefers switch-tree locality among nodes
available by the earliest sufficient start time. Future nodes cannot displace
a set that can start now, so immediate victim trials and normal placement use
the same locality preference. Remaining candidates stay available for additive
nodelist pinning: required nodes still force the job to wait when busy.

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
resumes scheduling immediately once `ensure_leader()` confirms leadership.
`relinquish_leadership()` clears planned reservations and outcome memory.
Orphan cleanup is the only operation held back: `should_sweep_orphaned_placements()`
gates the sweep on a 2x dispatch-timeout grace period under the live term,
preventing the new leader from prematurely reclaiming the previous leader's
in-flight placements. Dispatch itself is not paused.

**GPU stable ID vs device ID** -- allocations key on stable IDs, not DRM device
IDs. A node whose GPUs renumber (driver reload, hot-add) preserves running
allocations as long as the stable IDs survive. If a held stable ID vanishes,
`update_capacity` rejects the update to prevent silent resource leaks.

**Dispatch failure detail** -- the job reason includes the failure categories,
agent reasons, and the nodes that did not confirm the launch (including missing
agent addresses). Names are sorted and deduplicated, limited to eight with a count
of any remainder. Nodes that confirmed are excluded. This uses the existing
replicated launch-failure detail rather than adding a job field.

**Reconcile TTL** -- a launching reservation (reserved but not committed) is
spared by reconcile for `launching_ttl` seconds. Past that, it is reclaimed as
an abandoned launch. A commit arriving after reclaim returns false, telling the
caller not to treat the job as backed by an allocation.
