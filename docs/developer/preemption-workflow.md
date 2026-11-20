# Spur scheduling and preemption workflow

## Example and notation

- Each node has **8 GPUs**; each letter below represents one allocated GPU.
- **X**: incoming job; needs **2 nodes, with 2 GPUs on each**.
- **P**: protected job; holds **4 GPUs on Node 1**; cannot be preempted for X.
- **Q**: protected job; holds **4 GPUs on Node 2**; cannot be preempted for X.
- **R**: protected job; holds **all 8 GPUs on Node 3**; cannot be preempted for X.
- **A**: eligible victim; holds **2 GPUs on Node 1**.
- **B**: eligible victim; holds **2 GPUs on Node 1**.
- **C**: eligible victim; holds **2 GPUs on Node 2**.
- **D**: eligible victim; holds **2 GPUs on Node 2**.
- Configured victim preferences put **A before B** and **C before D**.
- CPU, memory, and other placement constraints fit in this example.

```text
Node 1: [ P P P P ][ A A ][ B B ]
Node 2: [ Q Q Q Q ][ C C ][ D D ]
Node 3: [ R R R R R R R R ]
```

- **L0** is the scheduler cycle. Each deeper level adds an enclosing loop.
- Steps **1–6 are sequential stages inside the same L1 pending-job loop**.
- A `FOR` includes equivalent Rust iterator traversals. The diagram shows
  the main scheduling, victim-selection, and placement loops; helper-level
  resource scans and specialized heterogeneous/fabric paths are omitted.
- The partition loop is conditional. Without partition-specific cap or
  memory-policy resolution, its body runs once and its child loops move up
  one nesting level. The example uses this simpler case.
- Steps 4–5 form one **trial**. They change temporary state only. Actual
  preemption begins at step 6.
- **[LIVE]** marks controller state kept across cycles. **[TEMP]** marks
  derived working data for a pass, selection, or trial. Scheduler timelines
  are reused but rebuilt for each ordinary pass or placement trial.
- Map annotations use `key -> value`; these are `HashMap`s unless stated
  otherwise. `Vec` is a list and `HashSet` is a set, not a map. The plot names
  the main resource and selection structures, not every helper cache.
- The scheduler reads current records in memory. Placement trials do not
  read the Raft log; Raft persists and replicates committed state changes.

## Complete workflow

<!-- spellcheck-disable -->

```text
L0 REPEAT EACH SCHEDULER CYCLE
|
|  Read current controller records [LIVE]:
|    ClusterManager.jobs: JobId -> Job
|      job.per_node_alloc: node name -> ResourceAllocations
|        Exact CPU, memory and device allocations owned by THIS job.
|      job.allocated_nodes: Vec<node name>
|    ClusterManager.nodes: node name -> Node
|      node.total_resources: physical capacity (record, not map)
|      node.alloc_resources: total held resources (record, not map)
|
|  Example reads:
|    jobs[A].per_node_alloc[Node 1] -> A's 2 GPUs
|    jobs[P].per_node_alloc[Node 1] -> P's 4 GPUs
|    nodes[Node 1].alloc_resources -> 8 GPUs held in total
|
|  Build pass inputs [TEMP]:
|    pending: Vec<Job>; nodes: Vec<Node> (copies from live records)
|    busy_until: node name -> expected end of current occupancy
|    handoffs: node name -> Vec<pending preemptor JobId>
|      Passed as ClusterState.preemption_handoffs.
|    partition_node_usage: partition -> node -> Vec<RunningCharge>
|      Each charge records JobId, resource counts and end time.
|    Partitions and reservations are lists, not maps.
|
+-- Prepare ordinary scheduling timelines
|   |
|   +-- L1 FOR each node
|           Read nodes[i].alloc_resources and busy_until[node].
|           Seed BackfillScheduler.timelines: Vec<NodeTimeline>.
|           Each timeline holds allocation intervals and cap charges.
|
+====================================================================+
| ORDINARY PLACEMENT                                                 |
|                                                                    |
| Data: ClusterState nodes + maps above; scheduler.timelines.        |
| HandoffGate.held_until: node -> JobId -> start time [TEMP]         |
|                                                                    |
| L1 FOR each pending job, within the scheduling limit               |
| |                                                                  |
| +-- L2 FOR applicable partition [conditional loop]                 |
| |   |                                                              |
| |   +-- L3 FOR each cluster node                                   |
| |   |       Read node records, partitions, reservations, handoffs. |
| |   |       Output suitable: Vec<node index> [TEMP].               |
| |   |                                                              |
| |   +-- L3 FOR each suitable node  <-- PLACEMENT NODE SCAN         |
| |   |   |                                                          |
| |   |   +-- Find earliest usable start time.                       |
| |   |       L4 REPEAT when conflicts push that time later          |
| |   |           Recheck resources and reservations via timelines.  |
| |   |       Output node_starts: Vec<(node index, start)> [TEMP].   |
| |   |                                                              |
| |   +-- Rank nodes; choose the required number.                    |
| |   |       free_gpu: node index -> free GPU count [TEMP]          |
| |   |       allocated_cpus_at_start: index -> used CPUs [TEMP]     |
| |   |                                                              |
| |   +-- L3 REPEAT until a common usable start                      |
| |   |   |                                                          |
| |   |   +-- L4 FOR each selected node                              |
| |   |           Recheck at the proposed common start.              |
| |   |                                                              |
| |   +-- L3 FOR each selected node                                  |
| |           Output per_node_alloc: node -> allocation [TEMP].      |
| |           This map becomes part of a successful Assignment.      |
| |                                                                  |
| |   END partition loop; choose applicable result.                  |
| |                                                                  |
| +-- Can the job start NOW?                                         |
|     YES -> if admitted, reserve capacity and return assignment.    |
|     Future placement -> pending; reserve future slot if allowed.   |
|     Admission denied -> pending; no slot reserved.                 |
|                                                                    |
| END ordinary pending-job loop                                      |
| Example: X cannot start now; all nodes have 0 free GPUs.           |
+====================================================================+
|
|  Prepare bounded preemption snapshot.
|  Read live jobs + nodes together via with_allocation_records().
|  Include assignments from this cycle; retain protected holders.
|  Snapshot working data [TEMP]:
|    snapshot.nodes: Vec<Node> with this cycle's assignments added
|    snapshot.holders: Vec<Job>, including protected resource holders
|      Each copied holder retains its per_node_alloc map.
|    snapshot.usage: partition -> node -> Vec<RunningCharge>
|    node_index: node name -> snapshot node index (capture helper)
|
|  Policy lookup maps [TEMP, reused across pending-job iterations]:
|    running_qos: running JobId -> resolved QOS policy
|    pending_qos_map: pending JobId -> resolved QOS policy
|    victim_cost: JobId -> QOS priority
|    facts: JobId -> elapsed runtime and node count (when configured)
|
+-- L1 FOR each remaining job considered for preemption
    |
    |  Current job = X. Skip if policy/limits forbid selection or if
    |  X is still awaiting releases from its previous preemption.
    |
    +-- 1. IDENTIFY ELIGIBLE VICTIMS
    |   |
    |   +-- L2 FOR each relevant running job
    |           Check QOS, protection, exemption time and mode.
    |
    |   Eligible: A/B/C/D. Protected: P/Q/R, still allocated.
    |   Example order: A, B, C, D (equal QOS; this tie order assumed).
    |   Selection inputs [TEMP; assume Cancel mode for A/B/C/D]:
    |     eligible = [(A, Cancel), (B, Cancel), (C, Cancel), (D, Cancel)]
    |     pool_ids / VictimPool.evictable = {A, B, C, D}
    |     VictimPool.holders = [P, A, B, Q, C, D, R] (order irrelevant)
    |     ordered = [A, B, C, D]; protected P/Q/R are excluded.
    |
    |   Allocation validation [TEMP; GPU counts shown]:
    |     node_index = {Node 1 -> &Node1, Node 2 -> &Node2,
    |                   Node 3 -> &Node3}
    |     known[Node 1] = P:4 + A:2 + B:2 = 8 GPUs
    |     known[Node 2] = Q:4 + C:2 + D:2 = 8 GPUs
    |     known[Node 3] = R:8             = 8 GPUs
    |     node.alloc_resources = 8 GPUs on each node; sums match.
    |     Initial projection: Node 1 = [P:4, A:2, B:2],
    |                         Node 2 = [Q:4, C:2, D:2], Node 3 = [R:8].
    |     Step 4 resets to these totals before trying each victim set.
    |
    +-- Direct path supported? ALL checks must pass:
    |     1. X needs equal CPUs/GPUs per node, OR X is exclusive.
    |        Multi-node GPU totals use fallback unless X is exclusive.
    |
    |     2. Every eligible victim occupies exactly ONE node.
    |        A multi-node victim affects several nodes; this path
    |        selects each node's victims independently.
    |
    |     3. No "must include these nodes + choose extra nodes" request.
    |        Counting enough nodes does not ensure required nodes are included.
    |
    |     4. No def_mem_* / max_mem_* settings in any partition.
    |        These rules need partition-specific memory sizing and limit checks.
    |
    |     YES -> steps 2 and 3, then full placement validation.
    |     NO  -> FALLBACK below.
    |
    +-- 2. BUILD PER-NODE VICTIM GROUPS                 [PR 1279]
    |   |
    |   +-- L2 FOR each eligible victim
    |   |       Build on_node: node name -> Vec<job reference> [TEMP].
    |   |       Example: Node 1 -> [A,B]; Node 2 -> [C,D].
    |   |
    |   +-- L2 FOR each candidate node, sequentially
    |       |
    |       |   Working data for THIS node [TEMP]:
    |       |     occupied: copy of node.alloc_resources (record)
    |       |     used: partition -> current resource charge
    |       |     releases: JobId -> Vec<(partition, charge reference)>
    |       |     Read each victim's per_node_alloc[node] for removal.
    |       |
    |       +-- Enough resources for X's per-node request WITHOUT preemption?
    |           |
    |           +-- YES --> Count as available; continue to next L2 node.
    |           |
    |           +-- NO (else) --> Try releasing eligible victims locally.
    |               |
    |               +-- L3 FOR eligible victims on THIS node
    |               |       Subtract allocation in local temporary state.
    |               |       Check local fit; BREAK when sufficient.
    |               |
    |               +-- Enough resources after this loop?
    |                   |
    |                   +-- NO --> Skip this node; continue to next L2 node.
    |                   |
    |                   +-- YES --> L3 FOR chosen victims in reverse order
    |                                   Try restoring each.
    |                                   Still fits? Remove it from victim set.
    |                                   Otherwise, keep it in victim set.
    |
    |   Output groups: Vec<(node reference, Vec<victim JobId>)> [TEMP].
    |   Node 1 -> {A}; Node 2 -> {C}; Node 3 -> no sufficient group.
    |   END node loop BEFORE proceeding to step 3.
    |   Insufficient groups -> FALLBACK.
    |
    +-- 3. CHOOSE ENOUGH GROUPS                  [PR 1146 ranking]
    |   |
    |   |   VictimSearch [TEMP; example]:
    |   |     Groups: 0 = Node 1/{A}; 1 = Node 2/{C}
    |   |     nodes_of_job = {A:[0], C:[1]}       (job -> groups)
    |   |     memo = {[A]:1, [C]:1}              (nodes made sufficient)
    |   |     heap pop order: {A}, then {C}
    |   |     victims: {} -> {A} -> {A,C}
    |   |     victim_cost: A=100, C=100          (QOS priority)
    |   |     facts: A=(60s,1 node), C=(120s,1 node)
    |   |
    |   +-- L2 FOR each candidate group
    |   |       Rank and insert into selection heap.
    |   |       (QOS first; then configured least_work/fewest_jobs/hash.)
    |   |
    |   +-- L2 WHILE fewer than X's required nodes would fit
    |       |   Choose next group; add its victims.
    |       |   Count the node their removal would make fit for X.
    |       |   Example: {A} -> 1 of 2 nodes; add {C} -> 2 of 2; STOP.
    |       |
    |       +-- L3 FOR affected groups needing rank refresh
    |               Update ranks when necessary.
    |
    |   Choose {A}, then {C}. Proposed set = {A, C}.
    |
    +-- 4. BUILD TEMPORARY AFTER-EVICTION STATE
    |   |
    |   |   PlacementProjection working data [TEMP]:
    |   |     nodes: Vec<Node>, with editable copied alloc_resources
    |   |     indices: node name -> index in that nodes list
    |   |     usage: partition -> node -> Vec<RunningCharge>
    |   |
    |   +-- Reset copied allocations and usage to snapshot values.
    |   |
    |   +-- L2 FOR each eligible job selected as a victim
    |   |   |
    |   |   +-- L3 FOR each recorded node allocation
    |   |           Read job.per_node_alloc[node].
    |   |           Use projection.indices[node] to find the copy.
    |   |           Subtract from that copy's alloc_resources.
    |   |
    |   +-- Remove selected victims' charges from projection.usage.
    |
    |   Node 1: [ P P P P ][ . . ][ B B ]
    |   Node 2: [ Q Q Q Q ][ . . ][ D D ]
    |   Node 3: [ R R R R R R R R ]
    |   Live jobs have NOT been evicted.
    |
    +=================================================================+
    | 5. FULL PLACEMENT SIMULATION                                    |
    |                                                                 |
    | Data: projection.nodes + projection.usage from step 4.          |
    | A trial BackfillScheduler has its own timelines vector.         |
    |                                                                 |
    | L2 FOR each node                                                |
    |     Rebuild trial timelines from copied allocations/charges.    |
    |                                                                 |
    | L2 FOR applicable partition [conditional loop]                  |
    | |                                                               |
    | +-- L3 FOR each cluster node                                    |
    | |       Read copied nodes and original placement constraints.   |
    | |       Output suitable: Vec<node index> [TEMP].                |
    | |                                                               |
    | +-- L3 FOR each suitable node  <-- PLACEMENT NODE SCAN          |
    | |       Check start NOW and applicable constraints,             |
    | |       including reservations during X's requested runtime:    |
    | |         Node 1 -> yes; Node 2 -> yes; Node 3 -> no.           |
    | |                                                               |
    | +-- Rank candidates; choose the required 2 nodes.               |
    | |       Same temporary ranking maps as ordinary placement.      |
    | |                                                               |
    | +-- L3 REPEAT common-start validation                           |
    | |   |                                                           |
    | |   +-- L4 FOR each selected node                               |
    | |           Recheck at the common start.                        |
    | |                                                               |
    | +-- L3 FOR each selected node                                   |
    |         Output per_node_alloc: node -> allocation [TEMP].       |
    |         This trial assignment proves fit; it does not launch X. |
    |                                                                 |
    | END partition loop                                              |
    | Complete placement NOW?                                         |
    |   Fallback call -> return Fits/Fails to its search loop.        |
    |   Direct call -> follow YES/NO below:                           |
    |   YES -> skip fallback; nonempty set goes to step 6.            |
    |          Empty set -> next pending job, with no eviction.       |
    |   NO  -> FALLBACK below.                                        |
    +=================================================================+
    |
    +-- FALLBACK [steps 2-3 cannot select victims, or step 5 fails]
    |   |
    |   |   TRIAL = steps 4 + 5; no actual eviction.
    |   |   Trial loops inherit caller nesting: bisect +1; pruning +2 levels.
    |   |
    |   +-- Prepare fallback order:
    |   |     Direct path ineligible, or steps 2-3 found too few suitable node groups:
    |   |       Group ALL eligible jobs per node: Node 1={A,B}, Node 2={C,D}.
    |   |       Rank groups via VictimSearch [PR 1146].
    |   |       Put preferred groups' jobs first within each QOS priority.
    |   |       Ordering hint only; trials below decide actual victims.
    |   |     Steps 2-3 selected victims, but step 5 still could not place X:
    |   |       Keep the existing victim order.
    |   |
    |   +-- TRIAL with NO victims
    |   |       Fits -> next pending job. Otherwise continue.
    |   |
    |   +-- TRIAL with ALL eligible victims
    |   |       Fails -> next pending job. Otherwise save this set.
    |   |
    |   +-- Set low = 0; high = number of ranked eligible victims.
    |   |
    |   +-- L2 WHILE low + 1 < high
    |   |       Bisect the victim-count range to find a smaller prefix that still places X.
    |   |       K = low + floor((high - low) / 2)
    |   |       If ordered=[A,B,C,D]: low=0, high=4 -> K=2 -> {A,B}.
    |   |       TRIAL with first K ranked victims:
    |   |         Fits -> save set; high=K (try fewer).
    |   |         Fails -> low=K (try more).
    |   |       Repeat until high = low + 1: no count remains between them.
    |   |       Keep the last verified working set for pruning.
    |   |
    |   +-- L2 BEGIN pruning pass on the saved working set
    |       |
    |       +-- L3 FOR selected victims in reverse ranked order
    |       |       TRIAL without this victim.
    |       |       Fits -> remove it from eviction set.
    |       |       Fails -> keep it in eviction set.
    |       |
    |       +-- Removed any victims? REPEAT L2; otherwise finish.
    |
    |   Remaining verified victims -> step 6.
    |   Any work/input/search limit reached -> defer X; no eviction.
    |
    +-- 6. ACTUAL PREEMPTION [verified nonempty set only]
    |   |
    |   +-- L2 FOR each eligible job selected as a victim
    |           Initiate configured preemption mode and grace.
    |
    |   Read selected IDs + eligible modes + running_qos policies.
    |   Actual preemption uses the controller's state-change path.
    |   Live job/node records change through that path and release
    |   reports; the hypothetical projection is never copied to them.
    |   Example: preempt A and C; P/Q/R/B/D continue running.
    |
    +-- Any preemption started? BREAK L1.
        Record X's limit claim while its eviction is in flight.
        A later cycle rebuilds state.
        After actual release, ordinary placement can launch X.
```

<!-- spellcheck-enable -->

## Reading the result

For this example, step 2 examines the nodes, step 3 chooses `{A, C}`, and
steps 4–5 run **one full placement trial for that complete victim set**.
Fallback is not entered. Step 6 initiates two job preemptions.

The placement trial contains its own node scans. It is **outside** step 2's
node loop. One trial does not mean one arithmetic operation, and the count
excludes ordinary scheduling before preemption and after actual release.

Victim selection follows configured preferences and removes redundant
victims; it does not guarantee a globally minimum victim set. Preemption
acts on whole jobs, so releasing a selected job can free more resources
than the incoming job needs.
