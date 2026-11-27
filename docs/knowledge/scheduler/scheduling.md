# Scheduling Internals

Cross-crate scheduling semantics: `spur-sched` (backfill, node matching, timeline, cons_tres,
immediate trials) driven by `spurctld/src/scheduler_loop.rs`. Crate-local constraints are in
`crates/spur-sched/AGENTS.md` and `crates/spurctld/AGENTS.md`. User-facing config: preemption in
`docs/admin-guide/accounting.rst` ("QOS preemption hierarchy"), idle-fill in
`docs/admin-guide/idle-fill-scheduling.rst`.

## Pass structure and ordering

- Only the Raft leader runs a pass. The loop wakes on `scheduler_notify` (submission) or the
  `interval_secs` tick; `max_jobs_per_cycle` bounds how many pending jobs one pass evaluates.
- spurctld sorts pending jobs: active-reservation holders, then hoisted preemptors, then
  descending effective priority, then `job_id`. Idle-fill candidates are appended after every
  in-quota job.
- A job that can start now must also pass `StartAdmission`; a refused job starts nothing and
  holds nothing.
- A job that cannot start now holds a **shadow reservation** at its earliest start (unless
  `StartAdmission::may_reserve` refuses it as over its limits). A lower-priority job may start
  only if, for its whole duration, its request fits in capacity not held by running jobs or
  shadow reservations. This is the starvation guarantee for high-priority work.
- Effective priority multiplies base priority, fair-share, age, and partition tier. The
  fair-share cap of 10.0 is duplicated in `priority.rs`, `fairshare.rs`, and the `sprio`/`sshare`
  CLI output; change all of them together. Fair-share computation:
  [accounting.md](../accounting/accounting.md#fair-share). Unknown user/account pairs get a
  neutral 1.0 from `FairshareCache` so new users are not penalized.

## Node matching

- `NodePlacement` (`node_match.rs`) checks name filters, partition membership (OR), features
  (AND), reservation access, then runtime state. Jobs without a matching, authorized
  `--reservation` cannot use a node inside an active reservation.
- Backfill gates on `matches_for_reservation()`, which admits busy-but-up nodes as future
  candidates. Plain `matches()` (idle check for exclusive jobs) feeds the pending-reason
  classifier only.
- **Nodelist:** if `num_nodes > len(nodelist)` it is *additive* (listed nodes required, the rest
  filled from the partition), otherwise *restrictive* (a candidate pool). An additive list holds
  the job when a listed node is busy, down, or draining; it never substitutes another node. A
  listed node inside another active reservation is silently dropped instead.
- Submit-time reduction (`sbatch -N4 -n1` becomes one node) can turn a 4-entry additive nodelist
  restrictive.
- **Preferred nodes:** QOS/account admission writes `preferred_nodes`; `NodePlacement::new()`
  uses it when no explicit `--nodelist` exists, so scheduling honors the admission credit.
  Admission's own reuse computation must use `new_ignoring_preferred_nodes()`, otherwise the
  account gate's credit leaks into the QOS gate as a nodelist restriction.

## Placement

- Candidate sort: earliest start, then most free GPUs, then highest node weight, then least
  allocated CPUs. `--spread-job` drops the weight key so least-loaded wins after GPUs.
- `--gpus=N` (`GpuDemand::Total`) packs greedily onto nodes with the most free GPUs
  (`distribute_total`), so `--gpus=8 -N4` on four 8-GPU nodes yields [5,1,1,1], not [2,2,2,2].
  Per-task GPU and CPU counts over an uneven task split (`-N2 -n7` is [4,3]) give non-uniform
  per-node allocations; `plan_per_node_alloc()` resolves them at the actual start and defers the
  whole placement if any node cannot hold its share.
- Exclusive jobs time their earliest start against the node's full capacity, so a 1-CPU
  exclusive job blocks the whole node on the timeline.
- Topology (`--topology=tree|block`) prefers locality only among nodes available by the earliest
  sufficient start: a later node never displaces a set that can start now. Immediate victim
  trials use the same rule. Additive nodelist pinning still applies after reordering.
- **Common-window convergence:** per-node earliest starts converge to their max, and the common
  start is revalidated against reservations on every node (shifting forward can enter a
  reservation window on an otherwise-clear node), iterating until stable or a 365-day horizon.
- A job whose `[start, start + duration)` overlaps an unauthorized future reservation on a node
  is blocked there even before the reservation starts, so it is not dispatched only to be killed.
- `UNLIMITED_JOB_DURATION` is the assumed occupancy for jobs without `--time`.

## Agent-side allocation ledger

`NodeAllocation` (`cons_tres.rs`) runs on spurd, not in the planning pass.

- GPUs key on stable IDs, not DRM device IDs, so renumbering does not orphan a live allocation.
  Details: [gpu-discovery.md](../devices/gpu-discovery.md).
- `run_attempt` guards requeue races: a newer attempt supersedes an older committed one, an older
  attempt gets `Superseded`, and teardown uses `release_job_if` so a stale release cannot free a
  newer attempt. A second allocate while one is still launching is `DuplicateJob`.
- `reconcile` spares launching allocations for `launching_ttl`, then reclaims them. A later
  `commit_job` returns false and the caller must not treat the job as backed by an allocation.
- `update_capacity` rejects an inventory that drops a held GPU stable ID or CPU core
  (`AllocatedDevicesLost`) and mutates nothing, so the caller decides policy (drain, fail) first.

## Preemption and reclaim

- Preemption requires `preempt_type = qos_priority` and runs after the backfill pass, only for
  unplaced in-quota jobs. Fabric-binding requests are excluded.
- Reclaim (recovering idle-fill borrowed capacity) runs first: recovering lent capacity is always
  preferable to evicting a job with a claim. When reclaim frees nodes, the cycle skips dispatching
  all already-computed assignments (not just `try_preempt`) so freed capacity is re-evaluated. A
  job whose shortfall reclaim closes never reaches `try_preempt`.
- Freed nodes are cooled down for the kill wait so the reclaimer does not land on a node whose
  previous occupant still holds GPU memory. `reclaim_in_flight` stops a second victim being taken
  for a shortfall already closed.

### Victim selection

**Eligibility** (`qos_preempt_allowed`): the pending QOS must list the victim's QOS in `preempt`
and either have strictly higher priority or be the same QOS with a later submission. A victim in
an active reservation needs a strictly higher pending partition tier. `preempt_exempt_time`
resolves QOS, then partition, then global. Full rules: `docs/admin-guide/accounting.rst`.

**Snapshot:** `scheduler_loop/preemption.rs` captures jobs and nodes together through
`with_allocation_records` (placement apply's lock order) and keeps compact copies
(`placement_job`, `placement_node`). Those copies must keep every field placement reads
(`not_responding`, `cpus_per_gpu`, `core_layout`, `layout_blocked_cpus`, `reserved_cpus`,
`layout_fingerprint`, and the node's `source` and `capabilities`), otherwise trials diverge from
real placement. The `PlacementRequest` copy keeps what the
[script placement](../execution/job-execution.md#script-placement) agent check reads: the
requested placement, `mpi`, and the launcher scan as `ResolvedPlacement::stand_in_script`, never
the script. Release credit follows the
node's current layout, so an older-layout victim cannot release another holder's reserved CPU
charge.

**Charges:** protected, reserved-pending, suspended, completing, grace-period, and same-cycle
allocations stay charged. Unaccounted occupancy is retained; missing or inconsistent slices grant
no release credit.

**Direct path:** for supported requests with single-node victims, `direct_victims` uses the real
`find_suitable_nodes` filter, computes each node's CPU, memory, GPU, and partition-cap shortfall,
removes eligible victims in QOS/configured order, prunes unnecessary ones, and ranks sufficient
node groups with `VictimSearch`. The combined set (even an empty one) gets one real placement
verification.

**Ranking:** QOS priority, then `scheduler.preempt_victim_order`, then node name. The ranking
credit is an estimate; only a verified final set can authorize eviction. There is no global
minimum-victim guarantee.

#### Direct-selection eligibility

Any of these sends `direct_victims` to fallback:

- Unequal per-node CPU or GPU counts (non-exclusive incoming jobs only).
- `GpuDemand::Total` across multiple nodes (non-exclusive only, even if it divides evenly).
- Any job in the validated eligible list holds allocations on multiple nodes, even if it would
  not be selected. A protected multi-node holder does not trigger this, but stays charged.
- `NodePlacement::nodelist_is_additive()`.
- Any partition in scheduler state has a memory-policy field (see below).

`--exclusive` bypasses only the first two.

#### Partition memory-policy gates

The gate tests field presence on every partition in scheduler state, including unrelated ones,
even when the job specifies memory. Fields: `def_mem_per_node`, `def_mem_per_cpu`,
`def_mem_per_gpu`, `max_mem_per_cpu` (via `Partition::has_memory_policy()`), plus
`max_mem_per_node`. An explicit job memory request and `max_tres_per_node.mem` do not trigger
fallback; local selection already checks cumulative partition charges.

#### Fallback search

Also chosen when there are too few sufficient local groups or the direct set fails verification.
It tries no victims, then all eligible, bisects an ordered prefix while keeping a proven
sufficient set, then repeatedly prunes unnecessary victims. Placement is not assumed monotone
under scheduler heuristics, so every accepted set has its own placement proof.

#### Trial state and bounds

- `PlacementProjection` owns the bounded request, compact node copies, and partition usage. Its
  static inputs never change between trials; only held allocations and charges decrease.
- `BackfillScheduler::place_releasing` reuses prepared sizing and production partition/allocation
  helpers, without future convergence. Trial timelines are reused, so more trials do not mean
  more simultaneous snapshots.
- Bounds live in `spur_sched::immediate` (`MAX_NODES`, `MAX_VICTIMS`, `MAX_ENTRIES`, `MAX_TEXT`,
  `MAX_TRIALS`, `MAX_WORK`, `MAX_HOSTS`, `MAX_NAME`). `MAX_VICTIMS` bounds the selected set, not
  the candidate pool. The input limits bound aggregate metadata, not RSS.
- Each reclaim pass and each QOS-preemption pass has its own `SearchBudget`; requests within one
  pass share it.
- `SelectionDeferred` distinguishes an incomplete search from a proven no-op. An incomplete search
  evicts nothing, even if pruning had found a sufficient intermediate set. Het jobs and GPU
  property requests always defer.
- Selection adds no persisted allocation index or Raft record.

#### Performance assessment

The direct path reduces full placement calls when it succeeds; an overall latency win for mostly
one- and two-node workloads is not established. Eligible victim widths and partition policies,
not incoming node count, decide which path runs. Worked operation counts, memory examples, and
measurement scope are in
[the victim-selection plan](../../plans/2026-11-16-resource-aware-victim-selection.md#worked-search-cost-comparisons).

## Idle-fill

- Idle-fill candidates start only if capacity is free now and never hold a future slot: a slot
  would publish a `StartTime` for a job that also reports being over quota.
- They are excluded from `try_preempt` (no claim on capacity). Reclaim can evict them after
  `idle_fill_exempt_secs`, and a higher-priority idle-fill job can displace strictly
  lower-priority opportunistic work.
- The `borrowed` flag rides with the assignment so the controller marks the run as borrowed.

## Heterogeneous jobs

- Gang pre-check: if any component lacks enough suitable nodes, all are skipped
  (`HetGroupIncomplete`). The check covers suitability, not simultaneous availability, so a
  component can still be deferred. Single-component groups are normal jobs.
- Components are placed consecutively at the highest-ranked component's position (Slurm
  `bf_hetjob_prio=max`). A reservation or hoisted preemptor on one component lifts the others. No
  other job is planned while placed components wait on the rest, so a group refused at start
  frees its nodes before the next job is planned.

## Leadership transitions

The new leader schedules and dispatches as soon as `ensure_leader()` confirms.
`relinquish_leadership()` clears planned reservations, planned starts, and outcome memory. Only
the orphaned-placement sweep waits (`should_sweep_orphaned_placements()`, see
`crates/spurctld/AGENTS.md`) so it does not reclaim the previous leader's in-flight placements.
