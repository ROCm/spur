# Resource-aware victim selection

One PR addresses protected-job shielding and excessive victim selection in both
QOS preemption and idle-fill reclaim. A small request on a shared node should
keep protected and unnecessary co-residents running with their allocations intact.

## Implementation

The controller captures a coherent, compact view of currently held resources.
It subtracts only selected eligible jobs' exact held slices from a private
projection, and checks immediate placement through the scheduler's existing
CPU, memory, GPU, partition, reservation, exclusivity, and handoff logic.

The deterministic search first checks whether the request already fits, then
whether removing all eligible jobs would place it. It grows a prefix ordered by
QOS priority, a preferred group from the configured node-ranking policy,
configured individual-job costs, node name, and job ID until placement succeeds.
It then repeatedly removes victims whose eviction is unnecessary. Only a fully checked final set may be acted on.
The search uses the existing greedy placement algorithm; it does not promise
an exhaustive proof of impossibility or a globally minimum victim count.

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
| `SearchBudget` | One scheduling cycle, shared by reclaim and QOS preemption | 256 placement calls and 2,000,000 charged work units; 128 calls per request |
| `InputSize` | Temporary conservative storage accounting | 32 MiB weighted allowance, 65,536 entries, 256 KiB text |
| Projected nodes and partition usage | One mutable workspace reset between trials | No saved subsets or trial-history cache |
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
   The 32 MiB allowance is conservative weighted accounting, not an
   allocator-enforced process/RSS ceiling. Scripts and environments are excluded.
3. **Disk:** hypothetical trials do not write WAL entries, database records,
   files, or per-trial logs. Actual evictions retain the existing persistence
   path and typically decrease when unnecessary victims are spared.
4. **Worst-case behavior:** budget exhaustion returns no victim set, including
   during pruning. The pending job retries on later cycles; there is no
   exponential subset enumeration or persistent accumulation between cycles.

The original selector timing and heap measurements below predate integration
with configured victim ordering. They compare the same prepared allocation
fixtures against the original base revision. They exclude fixture construction and
controller eligibility/snapshot overhead, and use an unoptimized test build.
They measure selector cost, not cluster throughput or end-to-end latency.
Each timing case runs 256 repetitions in five sequential fresh-process runs per
revision. The table reports the median of the five case medians, in microseconds:

| Prepared fixture | Base | Change | Additional time | Selected victims, base → change |
| --- | ---: | ---: | ---: | --- |
| One CPU from four shared jobs | 13.3 | 245.3 | 232.0 | 4 → 1 |
| One node from eight busy nodes | 19.3 | 458.5 | 439.2 | 1 → 1 |
| Eight-node request | 23.6 | 2,016.0 | 1,992.5 | 8 → 8 |
| Protected sharing | 3.8 | 239.1 | 235.2 | 0 → 1 |
| Unplaceable request | 6.9 | 136.2 | 129.3 | 0 → 0 |

The extra checks cost substantially more CPU than the old selector: roughly
18–86 times in the comparable successful fixtures, or 0.23–1.99 ms added in
this unoptimized build. Protected sharing changes functionality, so its ratio
would compare a successful search to an old rejection. Fewer real evictions
reduce work in the existing kill, persistence, and accounting paths; this
microbenchmark does not measure those savings.

Massif sampled maximum live heap after the test harness's startup peak at
69,125 bytes on the base and 104,697 bytes on the change (about 35 KiB added).
These small fixtures include the prepared workload and harness allocations;
this is not a per-search allocation total or a maximum-size-domain measurement.
The whole process's sampled heap peak was dominated by test registration.
No RSS or production-throughput ceiling is claimed.

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
- Byte/work/call limits, shared cycle budget, pruning exhaustion, huge task/GPU
  requests, and rejection of changed oversized placement inputs.
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
