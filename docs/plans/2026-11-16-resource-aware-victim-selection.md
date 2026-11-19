# Resource-aware victim selection

One PR addresses protected-job shielding and excessive victim selection in both
QOS preemption and idle-fill reclaim. A small request on a shared node should
keep protected and unnecessary co-residents running with their allocations intact.

## Implementation

The controller captures a coherent, compact view of currently held resources.
It subtracts only selected eligible jobs' exact held slices from a private
projection, and checks immediate placement through the scheduler's existing
CPU, memory, GPU, partition, reservation, exclusivity, and handoff logic.

Uniform requests with single-node victims calculate sufficient per-node groups
from actual allocations and partition charges, then verify their union once with
the real placer. The fallback uses bisection to find a sufficient ordered prefix
and repeatedly prunes unnecessary victims. QOS priority and configured victim
costs retain deterministic ordering. Only a verified final set may be acted on;
neither path guarantees a global minimum or exhaustively proves impossibility.

Protected, suspended, completing, reserved-but-pending, and unknown residual
allocations remain charged. Missing slices, inconsistent aggregate ownership,
stale inventory generations, conflicting same-cycle reservations, and exhausted
budgets supply no release credit. Suspend mode cannot qualify as a capacity
victim because it retains the allocation.

## New and updated data structures

| Structure | Lifetime and purpose | Storage bound |
| --- | --- | --- |
| `preemption::Snapshot` | One reclaim/preemption pass; compact nodes, held job slices, and partition usage captured under jobs-then-nodes read locks | Input sizing precedes cloning; no scripts, environment, execution history, CPU binding, or GPU topology metadata |
| `VictimPool` | Borrowed references to holders plus eligible IDs and QOS rank | At most 1,024 eligible victims |
| `PlacementRequest` | Compact, canonicalized request; resource and placement sizing for immediate trials | Bounded hostlist expansion, GPU demand, and repeated GPU type text before allocation |
| `PlacementScope` | Prepared set of nodes matching names, exclusions, preferred-node fallback, partitions, and static features; reused across holder overlap checks | At most 4,096 names; no per-victim hostlist expansion |
| `SearchBudget` | One reclaim or QOS-preemption pass; independent budgets | `MAX_TRIALS` (2064) calls and `MAX_WORK` (2000000) charged work units |
| `InputSize` | Temporary metadata entry/text accounting | `MAX_ENTRIES` (327680), `MAX_TEXT` (5 MiB); no allocator-size estimate |
| `PlacementProjection` | One prepared request, fixed static inputs and reusable allocation/charge copies | Release-only trials avoid repeating input preparation |
| `BackfillScheduler` timelines | Reused immediate-placement workspace | Containers reset for each trial; regular scheduling flags restored afterward |

The node domain is limited to 4,096 nodes. Hostlist expansion is limited to
8,192 names of at most 255 bytes; its parser bounds nesting. Topology requests
validate parent chains within 64 hops before invoking locality selection.
Heterogeneous job groups and GPU property/binding requests defer victim search
until their complete placement semantics are supported by this bounded path.

No Raft/WAL, snapshot schema, protobuf, configuration, CLI, or REST fields change.
The allocation snapshot above is an in-memory read view, not persisted state.
It retains scalar suspension timing so compact captures preserve effective
deadlines and preemption grace. GPU allocation entries must have injectable
count one before hypothetical placement; malformed counts cannot wrap into
insufficient GPU assignments. GPU type text is charged for every possible
placeholder before capacity probes can expand it; excessive count-by-name
products defer without allocating those placeholders.

## Additional performance burden

1. **CPU:** each search adds a compact allocation read, eligibility ordering,
   projection reset, and several immediate-placement trials. It avoids future
   release/convergence searches and stops on call/work exhaustion. Nonlinear
   placement/device charges use the prepared scope; validation, scans, copies,
   and partition charge seeding still account for the full domain. Aggregate
   node metadata and partition charges are charged once per trial; only request
   and global policy matching costs multiply by candidate count. Task-count
   calculation is now arithmetic O(nodes), replacing O(tasks) materialization.
2. **Memory:** one snapshot and one mutable projection/workspace, with O(nodes +
   held slices + eligible jobs + charges) storage inside fixed input limits.
   Entry and text caps bound metadata, not process RSS. Scripts and environments
   are excluded. Node and victim counts do not bound arbitrary metadata alone.
3. **Disk:** hypothetical trials do not write WAL entries, database records,
   files, or per-trial logs. Actual evictions retain the existing persistence
   path and typically decrease when unnecessary victims are spared.
4. **Worst-case behavior:** budget exhaustion returns `SelectionDeferred` and
   logs the affected job, including when pruning cannot complete. The pending job retries on later cycles; there is no
   exponential subset enumeration or persistent accumulation between cycles.

The current regressions count placement calls rather than measuring wall-clock
performance: requests requiring 60, 64 and 1024 co-resident victims use one full
placement verification. A 4096-node fixture with eight GPUs per node fits capture
and selection within default limits. These checks do not measure RSS, production
throughput or end-to-end latency.

## Required coverage and validation

Rust tests must exercise the production selector and immediate-placement
boundary. Controller lifecycle/component fixtures must carry real per-node
allocation slices rather than an ID-only occupant simulation.

- Protected CPU/memory and typed/stable GPU allocation retention.
- Exact one/two CPU victims, memory-driven selection, total GPU demand,
  irrelevant cheap-victim pruning, and zero eviction when free capacity fits.
- Multi-node exact slices, uneven tasks, additive/restrictive/ranged node lists,
  exclusions, preferred-node fallback, reservations, partition caps, typed
  charges, and handoffs. Pinned requests at 64/128/512/1,024 nodes and a
  feature-only request at 512 nodes retain progress under default budgets.
  Unpinned requests at 64/128 nodes retain progress with capped-partition
  charges, both with and without GPU inventory.
- Topology victim trials and normal scheduling agree after exact slice release.
  Locality prefers candidates within the earliest sufficient start window;
  required additive nodes and future planning retain their timing constraints.
- Missing/completed-node slices, stale generations, inconsistent/unknown
  ownership, reserved pending holders, same-cycle assignment accounting,
  malformed injectable GPU counts, expanded GPU type text in all four request
  forms, and resumed-victim snapshot/grace timing.
- Entry/text/work/call limits, explicit deferral versus no-fit results, independent
  pass budgets, no-borrower snapshot bypass, huge task/GPU requests, and rejection
  of changed oversized placement inputs.
- Controller shared-node eviction keeps a protected tenant and an unnecessary
  eligible tenant running with unchanged allocations and run attempts.
- Existing grace, cancel/requeue, release, eligibility, dispatch, and accounting
  lifecycle tests remain applicable.

Pytest E2E uses isolated deployments and real submission, eviction, payload,
and accounting boundaries. Cancel and requeue modes cover protected sharing,
CPU and memory shortfalls, exclusive/protected-capacity no-action controls with
positive controls, multi-node placement, and idle-fill reclaim. Tests assert
victim provenance and incoming completion plus spared process identity and
unchanged request/allocation parameters. Real GPU E2E checks device visibility
and skips explicitly when hardware is unavailable.

The topology follow-up adds a three-node pytest fixture for tree/block locality
in cancel/requeue mode. It checks that the incoming job starts across groups,
completes successfully, and preserves both protected payload identities. These
four cases have been collected and linted locally; live execution is pending.

Both original bugs were reproduced on the base binaries through the new E2E
scenarios: the small request evicted a spared co-resident, and the protected
sharing request never started. Corrected binaries must pass the matrix.

Required local checks: formatting, targeted locked tests, full locked tests,
workspace clippy excluding `spur-ffi` with all targets, cargo shear,
knowledge synchronization, and pytest flake8. Live tests run serially with
throwaway container/state ownership and bounded logs/storage, followed by cleanup.

## Initial validation results

- Full locked Rust suite: 6,647 passed, 48 ignored, zero failures.
- Focused selector/allocation boundary suite: 30 passed, one manual profile ignored.
- Formatting, workspace clippy with warnings denied, cargo shear, knowledge
  synchronization, and E2E flake8 passed.
- Baseline live regression scenarios failed for the two intended bugs.
- Corrected isolated live matrix: 21 distinct CPU, memory, reclaim, and handoff
  scenarios passed. Four real-GPU cases explicitly skipped because hardware was
  unavailable; deterministic Rust tests cover typed/stable GPU placement.
- Five affected live cases passed again using the updated binaries. The final
  placeholder-text guard passed local regression validation and does not change
  the CPU/memory scenarios. The isolated deployment was removed, with original
  containers verified intact.
