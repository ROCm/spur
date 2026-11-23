# AGENTS.md -- spur-sched

Algorithm, node matching, timeline, priority, GPU placement, agent-side ledger invariants, het jobs and
preemption trial bounds are described in
[docs/knowledge/scheduler/scheduling.md](../../docs/knowledge/scheduler/scheduling.md). This file
holds only crate-local constraints not stated there.

`NodeAllocation` (`cons_tres.rs`) is not used by the controller's planning pass: it is the
per-node ledger on each agent (spurd). Changes to it are agent-side behavior changes.

## Invariants

- The scheduler trusts the pending order from spurctld and does not re-sort (it only moves a het
  job's components up behind the first one). Wrong input order lets lower-priority jobs consume
  slots before higher-priority ones get shadow-reserved.
- `CpuPools::admits` is the single CPU fit rule on nodes with `restricted_cores_per_gpu`. spurctld
  reaches it through `Node::can_satisfy_request` / `Node::can_ever_satisfy`; do not add a second
  fit check. A job's `reserved_share = min(cpus, gpus x R)` is always free (one job holds a GPU at
  a time), so only `cpus - reserved_share` must fit the free general pool.
- The reserved share must never spill into general CPUs. The controller does not charge it against
  general, so a ledger that let a short reserved pool borrow general CPUs would over-commit
  general. `allocate_for_job` fails with `ReservedCpusHeld` instead (all or nothing, ledger
  untouched); spurd maps it to the `REFUSAL_RESERVED_CPUS_HELD` trailer and spurctld requeues
  without spending `max_batch_requeue`.
- Backfill stamps each allocation with `reserved_cpus` and the layout fingerprint
  (`stamp_allocation` / `CoreLayout::stamp`) only on nodes with per-GPU reservations.
- GPUs blocked by a layout change (`Node.layout_blocked_cpus`, reported by spurd from
  `blocked_reserved_cpus`) are reserved on the timeline via `Node::layout_blocked_alloc()` beside
  `alloc_resources`. CPU-only jobs and `can_ever_satisfy` ignore it.
- `job_resource_request()` requires a one-node job's entire GPU total during candidate filtering;
  multi-node totals use a one-GPU minimum per candidate until allocation resolves the split.
- `UNLIMITED_JOB_DURATION` is shared with spurctld's `busy_until` estimates and preemption; change
  both sides together.
- `restore_for_job()` and `update_capacity()` check held core ids for membership in the current
  online set, not a count. Callers drop contested cores with `claimable_cpu_ids()` first.
- A successful `update_capacity()` clears spec CPU exclusions and per-GPU reserved CPUs; the caller
  must re-apply both.
## Pitfalls

- Nodes without a `busy_until` entry get a 24-hour placeholder in backfill. Populate `busy_until` from running job time limits or small jobs get deferred.
- Uniform vs heterogeneous allocation is selected by `homogeneous_per_node` and
  `cpu_is_heterogeneous`. A wrong selection silently over- or under-allocates.
- The k0s gate is in `matches()`, not `eligible()`, and `matches_ignoring_k0s()` exists so the
  pending-reason classifier can say "would match if not k0s-reserved". Likewise
  `eligible_on_any_agent()` omits the batch-job K8s exclusion so the classifier can tell when
  K8s nodes alone would have fit.
- Node filtering also drops agents that would misplace a job's batch script
  (`without_misplacing_agents()`, `None` when an additive `--nodelist` names one). Semantics:
  `docs/knowledge/execution/job-execution.md` (script placement).
- The launching TTL passed to `reconcile()` must cover spurd's maximum launch latency: too short
  reclaims slow launches, too long pins resources from dropped launches.

## Testing

- `tests/pool_invariant.rs` enumerates small node shapes and job sequences (including layout
  changes and heartbeat-reported blocking) to keep `admits` and core picking in agreement. Extend it
  when changing either side.
