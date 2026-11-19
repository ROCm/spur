# Resource-aware victim selection

One PR addresses protected-job shielding and excessive victim selection in both
QOS preemption and idle-fill reclaim. A small request on a shared node should
keep protected and unnecessary co-residents running with their allocations intact.

## Implementation

The controller captures a coherent, compact view of currently held resources.
It subtracts only selected eligible jobs' exact held slices from a private
projection, and checks immediate placement through the scheduler's existing
CPU, memory, GPU, partition, reservation, exclusivity, and handoff logic.

Supported uniform or exclusive requests with single-node victims calculate
sufficient per-node groups from actual allocations and partition charges, then
verify their union once with the real placer. The fallback uses bisection to find
a sufficient ordered prefix and repeatedly prunes unnecessary victims. QOS
priority and configured victim costs retain deterministic ordering. Only a
verified final set may be acted on; neither path guarantees a global minimum or
exhaustively proves impossibility.

Protected, suspended, completing, reserved-but-pending, and unknown residual
allocations remain charged. Missing slices, inconsistent aggregate ownership,
stale inventory generations, conflicting same-cycle reservations, and exhausted
budgets supply no release credit. Suspend mode cannot qualify as a capacity
victim because it retains the allocation.

## Search before and after the performance review

A full placement trial asks whether the incoming job can run across candidate
nodes under the scheduler's resource and placement rules. Every release during
search is hypothetical: no victim is evicted until a final set is accepted.

Before the performance review, the search repeatedly called the full placer:

```text
Try removing nobody -------------------------- already fits -> no eviction
        |
   cannot fit
        v
Try removing all eligible jobs ---------------- cannot fit -> no eviction
        |
       fits
        v
Add ranked victims one at a time
        |
Full placement check after EVERY addition
        |
Stop adding when placement succeeds
        |
Try restoring each selected victim
Full placement check after EVERY restoration
        |
Accept the remaining sufficient set
```

The all-eligible trial establishes feasibility; it does not authorize evicting
all eligible jobs. If 64 one-CPU victims are all necessary, this search needs
two initial checks, 64 growth checks, and 64 pruning checks: 130 full placement
calls. The former 128-call limit could stop it before selection completed.

The current common path moves repeated resource checks onto individual nodes:

```text
Filter suitable nodes
        |
Examine candidate nodes sequentially
        |
        +-- Node A: release A1 -> sufficient group {A1}
        +-- Node B: release B1, B2 -> sufficient group {B1, B2}
        +-- Node C: already sufficient -> no victims needed
        |
Prune unnecessary victims within each group
        |
Subtract already usable nodes from the incoming node count
        |
Choose enough groups using QOS/configured victim ranking
        |
ONE full placement verification of the combined set
        |
        +-- fits -> accept the set
        +-- does not fit -> use fallback
```

For a two-node incoming job in this example, C supplies one node without
eviction; the configured ranking chooses one additional sufficient group from
A or B. Protected allocations remain charged on every node. In the 64-victim
case, the common path still performs roughly 130 local resource checks, but only
one full placement verification if that candidate set passes.

### When selection uses fallback

The scheduler knowledge base defines the exact
[direct-selection gates](../knowledge/scheduler/scheduling.md#direct-selection-eligibility)
and their scope. The following examples illustrate why those gates matter.
An additive node list such as `sbatch -N2 -n2 --nodelist=node-a job.sh` requires
node-a and one more node, so independent group ranking cannot freely choose both
nodes.

The multi-node-victim restriction reflects coupled resource releases:

```text
Node A: [eligible X][eligible Y]
Node B: [eligible X][protected P]

Choosing X once releases X's slices on BOTH nodes.
```

Even if Y alone could satisfy an incoming job on A, eligible two-node victim X
sends that search to fallback. A protected two-node job does not trigger this
particular gate because it is absent from the eligible list; its allocations
still constrain placement. The incoming job's node count is a separate matter:
a one-node incoming job can use fallback because a candidate victim spans two.

Local selection also falls back if it finds too few sufficient groups or its
combined set fails full placement verification. Work/input exhaustion instead
defers the search without eviction; fallback is also bounded.

The fallback checks unchanged and all-eligible projections, bisects a ranked
prefix while retaining a set that passed full placement, then repeatedly tries
restoring individual victims. Bisection reduces growth checks, but pruning can
still be expensive. Scheduler heuristics need not be monotone, so this is not a
proof of the globally smallest or cheapest victim combination.

### Uneven requests and example commands

These examples omit `--exclusive`, so they request shares of nodes:

| Submission | Per-node demand | Effect of this resource shape |
| --- | --- | --- |
| `sbatch -N2 -n3 -c2 job.sh` | Tasks `[2, 1]`, CPUs `[4, 2]` | Unequal CPU counts force fallback |
| `sbatch -N2 -n3 --gpus-per-task=1 job.sh` | Tasks `[2, 1]`, GPUs `[2, 1]` | Unequal CPU/GPU counts force fallback |
| `sbatch -N2 -n2 --gpus=4 job.sh` | Four GPUs total; placement resolves the split | Multi-node GPU totals force fallback, even when evenly divisible |
| `sbatch -N2 -n2 --gpus-per-node=2 job.sh` | CPUs `[1, 1]`, GPUs `[2, 2]` | Resource shape permits the common path; other gates still apply |

For the first command:

```text
                    Node A       Node B
Tasks                  2            1
CPUs per task          2            2
CPU requirement        4            2
```

A GPU-total request such as `--gpus=4 -N2` can resolve to `[2, 2]` or `[3, 1]`
depending on capacity. It does not require exactly two GPUs per node.
`--exclusive` bypasses the uneven-resource gate because local selection tests
whole-node availability; it does not bypass multi-node-victim, additive-list,
or memory-policy gates. Node inventory must still satisfy the request: an
exclusive one-node job requesting two GPUs cannot use a one-GPU node.

### Partition memory-policy example

The knowledge base lists the five
[partition memory-policy gates](../knowledge/scheduler/scheduling.md#partition-memory-policy-gates),
including their presence-based check across all partitions in scheduler state.
For example, with this partition:

<!-- spellcheck-disable -->

```toml
[[partitions]]
name = "compute"
nodes = "ALL"
max_mem_per_cpu = 2048
```

<!-- spellcheck-enable -->

`sbatch -p compute -N1 -n1 -c1 --mem=8G job.sh` has this effective requirement:

```text
Submitted:       1 CPU  + 8 GiB
Partition rule:  at most 2 GiB per CPU
Effective:      4 CPUs + 8 GiB
```

Partition defaults can also supply memory absent from the submitted request.
The fallback uses the full placer's partition-specific adjustment and admission
rules instead of assuming one unchanged resource request.

### Trade-offs for mostly one- and two-node jobs

| Aspect | Before the performance review | Current search |
| --- | --- | --- |
| Supported common case | Repeated full placement trials | Local resource checks plus one successful full verification |
| Very few necessary victims | Growth can succeed after few additions | May examine many candidate nodes before choosing a group |
| Many necessary victims | Many full growth and pruning trials | Local checks in the common path; bisection reduces fallback growth trials |
| Multi-node victims | General search | Still use fallback; two-node victims do not receive the common-path benefit |
| Implementation | Fewer selection paths | Local checks and full placement must enforce consistent requirements |
| Temporary storage | Snapshot and trial workspace | Also uses local groups and indices, inside explicit metadata/work limits |
| Victim selection quality | Ranked and pruned, without a global optimum guarantee | Same limitation; local sufficient groups constrain the common-path choices |

Incoming job width alone does not predict performance. Mostly one-node eligible
victims with supported partition policies favor the common path. Eligible
two-node victims or configured memory-policy gates send even a simple one-node
request to fallback. Neither path partially evicts a job: selecting a multi-node
victim releases that job's held allocations across its nodes.

The [performance assessment](../knowledge/scheduler/scheduling.md#performance-assessment)
is narrower than a general speedup claim: the common path reduces full placement
calls, while overall latency for mostly one-/two-node workloads remains unproven
against the original search. The calculations and scoped measurements below
provide evidence for that distinction.

## Worked search-cost comparisons

These are counts derived from the loops, not elapsed-time benchmarks. Assume
valid allocation records, stable inputs, sufficient budgets unless noted, and
ordinary CPU-only placement without additional topology/policy failures. A full
trial and a local resource check are different amounts of work; their counts
cannot be added and interpreted as milliseconds or CPU instructions.

If the first K ranked victims are sufficient and every one remains necessary,
the earlier linear search needs:

```text
2 initial trials + K growth trials + K pruning trials = 2 + 2K full trials
```

In the new common path, an occupied node that needs its first k victims incurs
one initial resource check, k growth checks, one sufficient-group check, and k
pruning checks: `2 + 2k` local checks. An already usable node needs one local
check. Candidate filtering, grouping, sorting and ranking are additional work.
The combined set then needs one successful full placement verification.

| Example | Earlier search: full trials needed | Current search |
| --- | --- | --- |
| 128 full nodes, eight one-CPU victims per node; incoming job needs two CPUs on one node | `2 + 2 + 2 = 6` | `128 * (2 + 2*2) = 768` local checks, then 1 full trial |
| 16 full nodes, eight one-CPU victims per node; incoming job needs two CPUs on each of two nodes, with node-major victim order | `2 + 10 + 10 + 4 = 26` | `16 * (2 + 2*2) = 96` local checks, then 1 full trial |
| One node, 64 one-CPU victims, all 64 necessary | `2 + 64 + 64 = 130`; the former 128-call limit prevents completion | 130 local checks, then 1 full trial |
| Fallback forced from the outset; 64 ranked candidates, all 64 necessary | 130 required; the former call limit prevents completion | `2 + 6 + 64 = 72` full trials |
| Fallback forced from the outset; 64 ranked candidates, the first victim alone sufficient | `2 + 1 + 1 = 4` | `2 + 6 + 1 = 9` full trials |

The first example assumes the preferred node's jobs lead the ranking. Its first
two victims suffice and cannot be pruned. The old projection-release loop also
scans the entire eligible list for every node on each trial:

```text
Earlier: 6 trials * 128 nodes * 1,024 candidates
       = 786,432 selected-set membership checks in that loop

Current: 1 trial * 1,024 candidates
       = 1,024 selected-set membership checks in that loop
       + 2 node-index lookup operations to release the selected one-node slices
```

These counts cover only projection release. The new path additionally builds
groups and performs the 768 local checks. Both paths prepare inputs, inspect
holders, sort candidates and invoke the placer. The earlier nested scan is CPU
work, not an allocated 128-by-1,024 matrix. Work limits can also stop an earlier
search before it executes its logically required trial count.

The two-node example explains why growth and pruning can cost more than the
final victim count suggests. With equal costs and node-major ranking:

```text
Ranked order: A1 A2 A3 A4 A5 A6 A7 A8 B1 B2 B3 ...

Growth reaches A1..A8 + B1 + B2 before TWO nodes each have two free CPUs.
    Growth trials:       10

First pruning pass restores A8..A3, keeping A1, A2, B1, B2.
    Pruning trials:      10

Second pruning pass confirms those four survivors are necessary.
    Pruning trials:       4

Including the two initial checks: 2 + 10 + 10 + 4 = 26 full trials.
Only four jobs are finally selected; no job is evicted during these trials.
```

The fallback examples assume monotone CPU capacity and no failed direct-path
attempt before fallback. For 64 necessary victims, bisection tests prefixes of
32, 48, 56, 60, 62 and 63 before retaining the already-proven set of 64. If the
first victim suffices, it tests 32, 16, 8, 4, 2 and 1 instead. This demonstrates
the trade-off: bisection reduces growth work for a large necessary prefix but
can make more full calls for a tiny one. Prepared trial reuse can lower each
call's cost, so call counts alone do not establish the latency difference.

### Held-allocation indexing example

For one job with 1,024 allocated nodes and one slice for each, the earlier
membership checks repeatedly searched the allocated-node list. One complete
traversal needs `1 + 2 + ... + 1024 = 524800` string equality checks when every
slice name is present and no node has completed. The current helper builds
1,024 borrowed-name set entries and performs 1,024 expected-constant-time hash
membership checks, plus the same completion checks. Hashing still reads name
bytes; these are different operations, not a measured 256-fold speedup.

The set's stored references alone occupy `1024 * 8 = 8 KiB` on a 64-bit build,
before hash-table capacity/control overhead. Jobs holding one or two allocated
nodes use their short list directly and create no membership set. This indexing
improvement is separate from the direct-path reduction in placement calls.

## New and updated data structures

| Structure | Lifetime and purpose | Storage bound |
| --- | --- | --- |
| `preemption::Snapshot` | One reclaim/preemption pass; compact nodes, held job slices, and partition usage captured under jobs-then-nodes read locks | Input sizing precedes cloning; no scripts, environment, execution history, CPU binding, or GPU topology metadata |
| `VictimPool` | Borrowed references to holders plus eligible IDs and QOS rank | Candidate processing obeys work/metadata limits; at most 1,024 victims in the final set |
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

### Worked temporary-memory example

Both search versions allocate compact snapshots and trial workspace. Repeating
130 placement trials does not retain 130 snapshots simultaneously: the mutable
node/charge projection is reused. The new version adds or changes temporary
indices and groups; it does not add a persistent controller allocation index or
Raft record.

For illustration, assume 128 nodes, 1,024 held/eligible one-node jobs, eight jobs
per node, two necessary victims per local group, and 16-byte node names. Use a
64-bit build, with no partition charges or GPU allocations in this example.
The current compiled type layouts give these inline sizes:

| Type | Inline bytes; heap contents excluded |
| --- | --- |
| `Job` | 2,552 |
| `Node` | 1,304 |
| `ResourceAllocations` | 312 |
| `GpuResource` | 384 |
| Reference / `JobId` | 8 / 4 |
| `String` / `Vec<T>` header | 24 / 24 |

These are layout measurements from the x86-64 Rust 1.95.0 build at `6bc99fc4`,
not stable API sizes or an allocator/RSS measurement. Even a compact `Job` has
inline headers for empty containers; omitting script and environment payloads
does not shrink its type.

The snapshot plus projection contain this much inline node/job storage:

```text
Snapshot held jobs:       1,024 * 2,552 bytes = 2,552 KiB
Snapshot nodes:             128 * 1,304 bytes =   163 KiB
Projection nodes:           128 * 1,304 bytes =   163 KiB
                                              ---------
                                              2,878 KiB = 2.81 MiB
```

Both algorithms need these kinds of copies. This subtotal excludes heap-owned
node names, allocation maps and slices, partition charges, timelines and other
workspace. For example, just the occupied key/value payload of the snapshot's
1,024 single-entry per-node allocation maps adds
`1024 * (24 + 312) = 336 KiB`, before key text, spare buckets and table overhead.

Some temporary structures used during current selection have these raw payloads:

| Structure | Payload calculation | KiB |
| --- | --- | --- |
| Ordered victim references, present in both versions | `1024 * 8` | 8 |
| Per-node eligible groups: borrowed name, vector header, job references | `128 * (16 + 24) + 1024 * 8` | 13 |
| Sufficient groups: node reference, vector header, two victim IDs per node | `128 * (8 + 24) + 256 * 4` | 5 |
| Borrowed views of sufficient groups for ranking | `128 * (8 + 16)` | 3 |
| Selector's borrowed node lookup and projection's owned-name lookup | `128 * (16 + 8) + 128 * (24 + 8 + 16)` | 9 |

The listed payloads total 38 KiB, but that is neither total selection memory nor
the increase over the earlier algorithm. It excludes spare vector/hash capacity,
table metadata, allocator overhead, ranking heap/maps, ownership aggregates,
per-node scratch storage and the larger copies above. The earlier method also
had temporary grouping/ranking structures: its per-node map of all eligible job
IDs alone has a 9 KiB raw payload in this example. Container lifetimes differ,
so summing every allocation also does not establish peak live memory.

GPU inventory adds another scale factor. At eight GPUs per node, the snapshot
and projection GPU arrays alone add
`2 * 128 * 8 * 384 = 768 KiB` of inline GPU entries, plus type strings.
Timeline inventories and the original controller state are additional. These
examples explain why metadata-entry limits and the 5 MiB text limit cannot be
read as a 5 MiB bound on controller RSS.

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

Deterministic regressions check that requests requiring 60, 64 and 1024 co-resident victims use one full
placement verification. A 4096-node fixture with eight GPUs per node fits capture
and selection within default limits. A separate 1024-node protected holder fixture
captures live allocation records and verifies selection both before and after a
node completes. Pools with 1025 eligible jobs select one sufficient victim through
both direct selection and fallback; a final set exceeding 1024 still defers.

### Recorded measurements of the later indexing follow-up

The ignored `mixed_workload_profile_*` tests measure the production selector with
128 shared GPU nodes, protected tenants, and 310 eligible jobs, mostly one-/two-node
jobs with occasional eight-/sixteen-node jobs. Three alternating before/after
runs of 64 selections each gave these median-of-run-median timings in an
unoptimized build, comparing revision `24b74b1b` with bounded candidate processing
and indexed held-allocation traversal in `0e759866`. Both revisions already have
the per-node method and bisection fallback. These measurements therefore do not
measure the earlier linear search against the new per-node method:

| Incoming nodes | Before index follow-up (ms) | After index follow-up (ms) |
| --- | --- | --- |
| 1 | 22.07 | 21.99 |
| 2 | 25.71 | 25.70 |
| 8 | 26.85 | 26.35 |
| 16 | 26.57 | 26.55 |

Each selection used 11 full placement checks and chose one eligible job. The
multi-node-victim fallback remains in use. Whole-process peak RSS was 29.0–30.0
MiB before and 29.6–30.1 MiB after, including fixtures and the test harness.
For one-node incoming jobs, charged work rose from 385,432 to 409,126 units;
for two-node jobs, from 401,659 to 425,353 units. These are conservative budget
charges, not CPU instructions or elapsed time; the latter values use about
20.5% and 21.3% of `MAX_WORK`, respectively. More accurately charging temporary
processing can increase these counts without increasing measured latency.
These measurements exclude snapshot construction and real job dispatch, and do
not establish a general speedup or a worst-case memory bound. Separate complete
test processes peaked at about 56 MiB for the wide-holder regression and 145 MiB
for the 4096-node fixture; those figures include controller state and fixtures.

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
