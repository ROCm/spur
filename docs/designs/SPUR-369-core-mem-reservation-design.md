# SPUR-369 + SPUR-386 — Reserved cores per GPU and core/memory specialization

- **Stories:** SPUR-369 (reserved cores per GPU with NUMA affinity) and
  SPUR-386 (WEKA client: core and memory specialization). Both under Epic
  SPUR-367; one shared spec. SPUR-386 implements the original request in
  Task SPUR-328.
- **Component:** spurctld (scheduler), spurd (node agent), spur-core,
  spur-sched, spur-devices, spur-k8s, spur-cli, proto
- **Status:** Revised (2026-10-30) — external design review (Yan Sun,
  2026-10-30) found 5 must-fix and 1 should-fix gaps in the accounting
  invariant, restart recovery, cgroup v2 semantics, enforcement guarantees,
  memory activation and layout ordering; all six addressed below. SPUR-391
  and SPUR-392 (already implemented) are unaffected — the fixes land in
  SPUR-393/394/395, none of which had started.
- **Related:** SPUR-368 (per-partition resource limits) provides the
  resource-generic partition accounting this builds on.

## Background

### Spur today (upstream `main` @ `799f59b`)

- The controller schedules CPU **counts**, never core IDs:
  `ResourceSet.cpus` / `ResourceAllocations.cpus` are scalars
  (`crates/spur-core/src/resource.rs:34,48`), and the launch message carries
  no CPU IDs (`proto/slurm.proto:33-38`, `LaunchJobRequest.allocated`).
- The node agent picks the actual cores first-fit from its own per-core
  ledger (`crates/spur-sched/src/cons_tres.rs:157,369-375`). Its indices are
  a synthetic `0..n` built from the online-CPU *count*
  (`crates/spurd/src/reporter.rs:710-745`), so on a node whose online CPUs
  are not contiguous they name the wrong cores.
- Jobs already get a cgroup v2 `cpuset.cpus` when `constrain_cores` is on
  (default) (`crates/spurd/src/executor.rs:1240-1359`). `cpuset.mems` is
  never written, and nothing is written on the `spur` cgroup root itself.
- There is no core or memory specialization. `NodeConfig.reserved_memory_mb`
  only lowers the controller's schedulable memory
  (`crates/spurctld/src/cluster.rs:8565`, `resource.rs:225-249`); no cgroup
  backs it.
- Per-GPU NUMA node and `[[devices.gres]] cores=` are discovered or parsed
  (`crates/spur-devices/src/cdi/discovery.rs:488`,
  `crates/spur-core/src/config.rs:1501`) but dropped before they reach the
  scheduler (`crates/spurd/src/reporter.rs:661-681`). Node CPU/NUMA topology
  is not discovered.
- There is no `--cpus-per-gpu` and no `DefCpuPerGPU` equivalent.
- `spurd` reads its own `spur.conf` (`crates/spurd/src/main.rs:323`) but not
  the `[[nodes]]` entries; `RegisterAgentRequest`
  (`proto/slurm.proto:897`) carries only a `ResourceSet`, labels and
  identity.

### Slurm behavior (verified in Slurm source, `master` @ `521370b436`)

- **`RestrictedCoresPerGPU`** (24.05+) is enforced only in the controller.
  For each GPU it takes the lowest R unique cores of that GPU's `Cores=`
  list, skipping spec cores and cores already given to an earlier GPU
  (`src/slurmctld/node_mgr.c:3424-3514`). Missing affinity or too few unique
  cores fails node registration.
- Jobs that request no GPU cannot use restricted cores; whole-node jobs can
  (`src/plugins/select/cons_tres/cons_helpers.c:64-102`).
- Binding is **per socket**, not per device: a job may keep at most
  `GPUs × R` restricted cores, and taking k restricted cores on a socket
  forces `ceil(k/R)` GPUs from that socket
  (`src/plugins/topology/common/gres_filter.c:675-690`,
  `src/plugins/select/cons_tres/gres_select_filter.c:1793-1830`).
- **`DefCpuPerGPU`** applies only when neither `--cpus-per-task` nor
  `--cpus-per-gpu` is given; it can be set per partition. `--cpus-per-gpu`
  with `-c` is a fatal error
  (`src/plugins/select/cons_tres/job_test.c:458-505`,
  `src/common/slurm_opt.c:5829-5835`).
- **`CoreSpecCount`** picks the highest core on the highest socket first,
  then round-robin downward; **`CpuSpecList`** is expanded to whole cores;
  setting both is an error (`src/common/node_conf.c:391-487`). Spec cores are
  removed from every job (`cons_helpers.c:113-129`).
- **`MemSpecLimit`** both lowers allocatable memory and caps the daemon's
  cgroup `memory.max` (`job_test.c:712`, `src/slurmd/slurmd/slurmd.c:3360-3399`).
- With `TaskPluginParam=SlurmdOffSpec`, slurmd runs on the non-spec CPUs
  (`slurmd.c:3290-3325`).
- `isolate_cpusets` is not a Slurm setting.

### WEKA requirements

From WEKA's Slurm integration guide: reserve the WEKA client's cores with
`CpuSpecList` (processor IDs — both hyperthreads of a core, e.g. `47,95`) or
`CoreSpecCount`, and its memory with `MemSpecLimit`; use
`TaskPluginParam=SlurmdOffSpec` so scheduler daemons stay off those cores;
constrain jobs with cgroups (`ConstrainCores`, `ConstrainRAMSpace`); and set
`isolate_cpusets=false` in WEKA's own `/etc/wekaio/service.conf` so the
WEKA agent does not reset the scheduler's cpusets.

## Requirements (approved 2026-10-28)

### SPUR-369 — reserved cores per GPU

1. Node setting `restricted_cores_per_gpu = R`, counted in **cores**. On SMT
   nodes a core is all of its hyperthreads, as in Slurm.
2. Each GPU's core affinity comes from its `[[devices.gres]] cores=` entry
   if set; otherwise it is the CPU list of the GPU's discovered NUMA node.
3. Each GPU's reserved cores are the lowest R unique cores in its affinity
   list, skipping spec cores and cores already given to an earlier GPU
   (Slurm's algorithm). If affinity is missing or there are not enough unique
   cores, the node is drained with a clear reason.
4. Who may use reserved cores:
   - jobs with no GPUs never get them;
   - whole-node (`--exclusive`) jobs may use every non-spec core;
   - a GPU job may use up to `R × (its GPUs on that socket)` reserved cores
     on each socket (Slurm's per-socket rule). Spur meets this by giving a
     job only its own GPUs' reserved cores, a stricter subset of the same
     limit.
5. `--cpus-per-gpu` on sbatch/salloc/srun, plus a `def_cpu_per_gpu` default
   (global and per partition) with Slurm's precedence. Combining
   `--cpus-per-gpu` with `-c` is rejected at submit.

### SPUR-386 — spec cores and memory (WEKA client)

6. Node settings `core_spec_count` or `cpu_spec_list` (not both).
   `core_spec_count` uses Slurm's order (highest core on the highest socket
   first, then round-robin downward). `cpu_spec_list` takes OS CPU IDs (as
   WEKA documents them, unlike Slurm's abstract IDs) and is expanded to whole
   cores.
7. Memory: the existing `reserved_memory_mb` becomes the single knob, with
   `mem_spec_limit` accepted as the Slurm name. It already reduces
   schedulable memory; on nodes that also set spec cores it is now enforced on
   the node too. Nodes that only set `reserved_memory_mb` are unchanged.
8. Spec cores and spec memory come off the controller's schedulable totals.
   On the node:
   - the `spur` cgroup root's `cpuset.cpus` excludes the spec CPUs, so no
     job can land on them;
   - the root's `memory.max` is capped at total memory minus the spec memory
     (when spec cores are set);
   - `spurd` and `spurstepd` run off the spec cores (Slurm's
     `SlurmdOffSpec`), on by default because WEKA needs it.
9. Spur writes cpusets only inside its own cgroup subtree. Ops docs tell WEKA
   users to set `isolate_cpusets=false`.

### Shared

10. Prerequisite fix: `spurd` uses the real online CPU IDs instead of a
    `0..n` count.
11. Everything is optional. A node with none of these settings behaves
    exactly as today.

### Out of scope

`--core-spec`, `--thread-spec`, `AllowSpecResourcesUsage`,
`--gres-flags=enforce-binding`, and shared GPUs (shards/MPS).

## Architecture

**Chosen: A — the controller tracks pool counts, the node agent picks real
core IDs.**

- At registration the agent reports its real topology (online CPUs with their
  core, socket and NUMA node; each GPU's NUMA node and configured `cores=`)
  in new optional `RegisterAgentRequest` fields.
- The controller combines that with the node's `[[nodes]]` settings in one
  pure function (in `spur-core`) that decides which CPU IDs are spec cores
  and which are reserved for each GPU, and sends the result to the agent. This
  is the single source of truth, so controller and agent cannot disagree about
  the pools.
- The controller schedules with counts: spec CPUs are removed from the node's
  total, and it checks only the general pool. It already chooses GPU device
  IDs, so it knows how many reserved CPUs each job's GPUs bring.
- The agent turns the job's CPU count and GPUs into real CPU IDs from the
  matching pools using its existing per-core ledger.

Why counts are enough: each GPU's reserved CPUs are used only by jobs holding
that GPU, and a job takes at most its own GPUs' reserved CPUs. So a job always
finds its GPUs' reserved CPUs free, and the controller never needs individual
core IDs or per-socket counts (see "Controller accounting" below).

### Architecture details

#### Data flow

```
spurd startup
  ├─ discover topology: online CPU IDs → (core, socket, NUMA node);
  │  per GPU: NUMA node, configured `cores=`
  └─ RegisterAgent(resources, topology) ──► spurctld (leader)
                                              ├─ propose NodeRegister / NodeUpdate (raw report, as today)
                                              └─ apply step, on every replica:
                                                 apply_node_config_policy
                                                   └─ CoreLayout::compute(topology, gpus, [[nodes]])
                                                      ├─ Ok  → node.core_layout, total_resources.cpus −= spec
                                                      └─ Err → node unschedulable "core-layout: <why>"
  ◄── RegisterAgentResponse(core_layout)
  ├─ write spur cgroup root: cpuset.cpus = online − spec
  │                          (+ memory.max = total − spec memory when spec cores are set)
  ├─ pin every spurd thread and running spurstepd off the spec CPUs
  └─ replay adopted jobs into the ledger (cgroup-recovered first, pool-aware
     picker only if the job's cgroup is gone)

heartbeat(held layout fingerprint) ──► reply: reregister if the layout changed
                                        or needed topology is missing

scheduling (counts only)            launch
  general_free ≥ general_need  ──►  LaunchJobRequest(allocated, core_layout)
                                      └─ spurd adopts the layout if it changed, then the
                                         ledger picks real CPU IDs: the job's own GPUs'
                                         reserved CPUs first, then the general pool
```

#### Topology and the core layout

- **Topology travels with the raw report.** `RegisterAgentRequest` gains a
  compact `CpuTopology` (one entry per physical core: its socket, NUMA node and
  CPU IDs), and `GpuResource` gains `numa_node` and `affinity_cpus` (a
  configured `cores=` converted to CPU IDs by the agent). The controller keeps
  the topology only in the node's raw report (`detected_resources`), which
  `NodeRegister`/`NodeUpdate` already persist and compare (`cluster.rs:9840`);
  it is not copied into `total_resources` (in either branch of
  `apply_node_config_policy`, including the no-match `detected.clone()` at
  `cluster.rs:8583`) and not returned in node listings.
  So it is replicated, snapshotted and change-detected with no new WAL
  operation, and stored once per node.
- **One pure function decides the pools.** `CoreLayout::compute` in
  `spur-core` takes the topology, the GPUs and the matched `[[nodes]]` entry
  and returns the spec CPUs, each GPU's reserved CPUs, and the general CPUs, or
  an error naming the problem.
- **It runs in the apply step**, inside `apply_node_config_policy`
  (`cluster.rs:8556`), which already runs on register, update and snapshot
  reconcile. The layout and any layout error are derived (`#[serde(skip)]` on
  `Node`), recomputed on every replica from the replicated raw report and the
  replica's config.
- **Restart-only settings.** Because `scontrol reconfigure` re-derives node
  policy on the leader only (`cluster.rs:5680-5691`), a live change would leave
  replicas with different layouts. `reconfigure` therefore rejects changes to
  `core_spec_count`, `cpu_spec_list`, `restricted_cores_per_gpu`,
  `reserved_memory_mb` and `daemons_off_spec` with "restart all controllers to
  apply". The check runs first, before `reconfigure` proposes any partition
  change (`cluster.rs:5603-5665`), so a rejected reconfigure changes nothing.
  The settings take effect when the controllers restart with the new
  `spur.conf`, which must be identical on every controller as for the rest of
  the config.
- **Kubernetes-backed nodes** register through the operator without topology;
  the layout is skipped for them (logged once if a matching `[[nodes]]` entry
  sets the new fields).
- **Core numbering** for `cores=` follows Slurm: a core is all hyperthreads
  that share a physical core; core indices are abstract and socket-major
  (`0..cores`), ordered by each core's lowest CPU ID. `cpu_spec_list` takes
  **OS CPU IDs** (as WEKA's guide writes them), widened to whole cores; this
  differs from Slurm's `CpuSpecList`, which uses Slurm's abstract CPU IDs, and
  is called out in the Slurm-compatibility docs.
- **Bad layouts make the node unschedulable.** The apply step records a
  derived layout error; the node is excluded from placement and shown as
  `DRAIN` with reason `core-layout: <why>`, and it clears on the next
  successful computation. An operator fixes the config and restarts the
  controllers, or the agent re-registers with a fixed topology; no manual
  resume needed (details in Error Handling).

#### Controller accounting — counts are enough

- `total_resources.cpus` excludes spec CPUs, and `reserved_memory_mb` keeps
  lowering schedulable memory as today. Spec resources are therefore invisible
  to every existing check (fit, never-runnable, SPUR-368 caps).
- A job's reserved share on a node is
  `reserved_share = min(cpus, Σ reserved CPUs of its GPUs on that node)`; it
  needs `general_need = cpus − reserved_share` from the general pool. CPU-only
  jobs need all their CPUs from it; exclusive jobs take everything (they hold
  every GPU, so every reserved CPU).
- **Why the reserved share always fits:** a GPU's reserved CPUs are handed
  only to jobs holding that GPU, and only one job holds a GPU at a time. So
  when a job gets a GPU, that GPU's reserved CPUs are free, and the only new
  check is `general_free ≥ general_need`. The controller tracks one extra count
  per allocation (`reserved_cpus`), not per-socket or per-core state. Slurm's
  per-socket limit (`R ×` the job's GPUs on a socket) holds automatically,
  since a job only ever takes its own GPUs' reserved CPUs.
- **The reserved share is atomic (design review, finding 1).** This invariant
  only holds if a job's reserved-share CPUs always come from its own GPUs'
  actual reserved cores — never from the general pool. A layout change can
  break that: a CPU-only job that predates the layout (or ran under an older
  one) may physically occupy what the new layout calls a GPU's reserved cores,
  while the controller counts that job as general (below). If the agent then
  let a GPU job "spill" its reserved-share portion into the general pool
  because the real reserved cores were squatted, the controller would never
  charge that spillover against general — so when the legacy job later exits,
  the controller believes general capacity is free that is in fact still held
  by the GPU job. A new CPU-only job then passes scheduling and fails at
  launch. The fix: a job's reserved-share CPUs are drawn **only** from its own
  GPUs' reserved pool, atomically. If physically insufficient, the launch
  fails (`AllocError::CpusUnavailable`, same requeue path as any other
  shortfall) rather than spilling into general — it does not partially
  allocate. Only the excess beyond `reserved_share` (`general_need`, already
  charged to general) may draw from the general pool; that part is unaffected.
  A GPU job can therefore queue, transiently, until a legacy job vacates its
  reserved cores after a layout change — bounded by that job's lifetime, not a
  standing hazard, and limited to the restart-only, operator-driven moment a
  layout changes.
- **Layout changes under running jobs.** Each allocation records the
  fingerprint of the layout it was made under (a stable FNV-1a hash of the
  layout's canonical encoding, not `DefaultHasher`). When a node's layout
  changes (restart with a new R or spec, a CPU going offline), allocations made
  under an older fingerprint count as entirely general (`reserved_cpus`
  treated as 0). That remains the safe direction for the job's *own* charge —
  CPUs counted as general that physically sit on reserved cores only make the
  physical general pool larger than the controller's count — provided the
  atomicity rule above stops anything else from spilling into that same
  physical space on the strength of a false "it's free" belief.
- **Never runnable:** a CPU-only job larger than a node's general capacity,
  or a GPU job whose `general_need` exceeds it, can never start on that node;
  the existing never-runnable check and pending reasons use these numbers.
- **One CPU-sizing helper.** Today many places compute a job's CPUs as
  `num_tasks × cpus_per_task` (QoS/association limits and GrpTRES usage,
  `--mem-per-cpu`, requested TRES, accounting CPU-seconds and fairshare,
  `sacct`/`sstat` CPU columns, `SPUR_CPUS_ON_NODE`, cgroup fallback, prolog
  context, Kubernetes pod sizing). With `--cpus-per-gpu` that is wrong, so a
  single `spur-core` helper computes per-node CPUs as
  `max(tasks × cpus_per_task, cpus_per_gpu × GPUs)` (and the job total) and
  every consumer moves to it; accounting records the allocated CPU total.

#### Node agent

- **Prerequisite fix:** the per-core ledger is indexed by real CPU IDs from
  `/sys/devices/system/cpu/online` instead of `0..count`
  (`reporter.rs:709-746`, `cons_tres.rs:177`). Offline IDs are never handed
  out.
- **Picking cores.** Given the job's CPU count and GPUs, `spurd` takes the
  job's own GPUs' free reserved CPUs — up to `reserved_share`, **atomically**
  (design review, finding 1): if fewer than `reserved_share` are actually free
  in that GPU's reserved pool, the allocation fails rather than drawing the
  shortfall from general. Only CPUs beyond `reserved_share` (the job's
  `general_need`) come from the general pool, preferring its GPUs' sockets and
  whole cores. Spec CPUs are never handed out. The split formula is shared
  code in `spur-core`, so controller and agent agree.
- **Why the agent never comes up short on the general pool.** CPUs the
  controller counted as general but that physically sit on reserved cores
  (jobs started before the layout existed or under an older layout) leave the
  same number of general CPUs physically free — because, per the atomicity
  rule above, nothing is ever allowed to draw a job's reserved-share portion
  from general on the strength of those CPUs merely "looking free."
- **Restart recovers real placement, not a guess (design review, finding
  2).** `spurd` registers, and so adopts the layout, before it replays adopted
  jobs (`main.rs:568` before `main.rs:666`). Jobs with recorded CPU IDs are
  replayed as today (`agent_server.rs:3803-3870`). For a descriptor with no
  recorded CPU IDs (for example the non-head nodes of a multi-node job, which
  only have step descriptors), re-picking synthetic IDs is unsafe: the process
  is already running, confined by its surviving cgroup, to whatever CPUs it
  actually held before the restart — a fresh pick cannot move it, and marking
  different CPUs "busy" in the rebuilt ledger leaves the job's *real* CPUs
  unprotected for a new allocation to collide with. Every stepd descriptor
  already carries `job_id` and `run_attempt`, which deterministically
  reconstruct that job's cgroup path (`executor.rs`'s `cgroup_path_for`).
  `spurd` reads that cgroup's live `cpuset.cpus.effective` and recovers the
  job's actual CPU IDs from it — there is no remaining case where any picker
  runs for a still-running process. Only when the cgroup itself is gone (the
  process did not survive) is there nothing to recover: that descriptor goes
  through normal job-exit cleanup instead, never a guessed pick. If the cgroup exists but its effective cpuset cannot
  be read, the node holds that job's CPU count out of the general pool as
  unknown-but-occupied (not available for new allocations) until an operator
  resolves it, rather than guessing.
- **Layout on every launch, gated by generation (design review, finding 6).**
  Each `LaunchJobRequest` for a node with a layout carries it (a few hundred
  integers) alongside the allocation's existing `generation` (the node
  inventory generation the scheduling decision was made under — already sent
  today for a different purpose, detecting repartition between schedule and
  launch). A delayed, in-flight launch computed under an older topology could
  otherwise arrive after a newer re-registration and regress `spurd` back to a
  stale layout. The fix needs no new wire field: the agent extends its
  existing generation-staleness check (today used only to reject a dispatch
  whose resource generation no longer matches live inventory) to also gate
  `core_layout` adoption — a launch whose `generation` is older than the
  agent's current one is rejected as stale (the same retryable path as any
  other generation mismatch) before any layout change is considered, so a
  stale message can never regress the held layout. A registration reply needs
  no such check: the agent already knows its own current generation — it is
  the same `ResourceSet.generation` value the agent bumps on each topology
  rebuild and sends in every `RegisterAgentRequest` — and a reply is always
  computed against what it just sent.
- **Idle nodes and late topology.** Each heartbeat carries the fingerprint of
  the layout `spurd` holds. If it differs from the node's current layout, or
  the node's settings need topology the controller has not got (for example
  `spurd` was upgraded before the controllers, which dropped the unknown
  field), the heartbeat reply asks `spurd` to re-register. So idle nodes pick
  up a new layout (pinning, cgroup root) without waiting for a launch. A
  re-register is a fresh registration round-trip, so it is inherently current
  — no staleness check is needed there either.

#### Node-level enforcement (SPUR-386)

- When it gets a layout with spec CPUs, `spurd` writes the `spur` cgroup
  root's `cpuset.cpus` (online CPUs minus spec). Nodes that only set
  `reserved_memory_mb`, as today, get no root writes, so existing
  configurations are unchanged. Writes stay inside Spur's own subtree.
- **Tightening the root cpuset never widens a running job's effective set
  (design review, finding 3, confirmed against the cgroup v2 kernel docs).**
  cgroup v2 does not shrink a child's effective cpuset to empty when its own
  `cpuset.cpus` no longer intersects the parent's: it is "treated just like an
  empty `cpuset.cpus`" and the child's effective set **widens** to inherit the
  parent's full effective set. So a one-CPU job whose sole CPU newly becomes
  spec or GPU-reserved would, if the root cpuset were simply tightened, end up
  with *every* non-spec CPU — including other GPUs' reserved cores — exactly
  the opposite of "keeps running on fewer CPUs." The fix: before writing a
  smaller root `cpuset.cpus`, `spurd` reconciles against every live job cgroup
  under `spur/`. A CPU is removed from the root only if no live job's own
  `cpuset.cpus` would then have empty intersection with the new root set; a
  CPU still needed to keep some running job's cpuset non-empty is deferred —
  left in the root's (and that job's) allowed set — until that job exits, then
  swept into spec/reserved on the next reconciliation (next heartbeat or
  layout-adoption event). This is the same "defer until it is safe" shape as
  the controller-side accounting fix above, applied to the kernel's actual
  cpuset semantics instead of the controller's counts.
- **Tightening memory never risks the OOM killer (design review, finding 5,
  revised after a second review pass).** Checking `memory.current` once and
  then writing a lower `memory.max` directly is not race-safe: a job can
  allocate between the check and the write, and per the cgroup v2 docs,
  writing a `memory.max` the cgroup cannot be reclaimed under invokes the OOM
  killer inside it — reporting the outcome honestly afterward does not
  prevent that kill. The fix changes the write itself, not just its
  reporting, using the two limits' documented difference in kind: going over
  `memory.high` "never invokes the OOM killer" (throttle and reclaim
  pressure only), while `memory.max` does when reclaim fails. So when spec
  cores are set, `spurd`:
  1. immediately lowers `memory.high` to the target (node memory minus
     `reserved_memory_mb`) — safe at any usage level, since it only throttles;
  2. on each later heartbeat, checks `memory.current`; once it is at or below
     the target (`memory.high`'s pressure is what gets it there), lowers
     `memory.max` to the same target — now safe, because usage is already
     confirmed under it and `memory.high` has been holding it there meanwhile;
  3. only then reports the reservation `pending → active`.
  Until step 2 succeeds, the reservation is `pending` (a surfaced,
  operator-visible node condition, not a silent retry loop), and the WEKA
  memory guarantee is not advertised as live — `memory.high` alone throttles
  but does not guarantee exclusivity. If usage will not settle on its own,
  draining or otherwise reducing existing jobs' memory so step 2 can proceed
  is an explicit operator action (documented in the runbook), never something
  `spurd` forces through by writing `memory.max` while over target.
- When the spec settings are removed, `spurd` resets the root (`cpuset.cpus`
  empty, i.e. inherit; `memory.max` = `max`) and widens the daemon masks back
  to all online CPUs.
- With `daemons_off_spec` (default on), `spurd` pins every one of its threads
  (`/proc/self/task/*`) and every thread of every running `spurstepd`
  (`/proc/<pid>/task/*`) to the non-spec CPUs each time it receives a layout;
  threads and processes created later inherit the mask. Sites that pin daemons
  with systemd `AllowedCPUs=` turn it off.
- **Enforcement is a prerequisite for the guarantee, not best-effort (design
  review, finding 4).** A node that configures any of `core_spec_count`,
  `cpu_spec_list` or `restricted_cores_per_gpu` is making an isolation promise
  ("CPU-only jobs never use reserved cores", "WEKA's memory is never taken").
  That promise does not actually hold if root enforcement can fail and the
  node keeps accepting jobs on controller-only accounting while still looking
  "configured" — the earlier `[cgroup] required` default (false) meant exactly
  that silent failure mode. The fix: for a node with any of these settings,
  successful root cgroup enforcement is mandatory, independent of
  `[cgroup] required` (which keeps its existing meaning for ordinary cgroup
  degradation unrelated to these features). If the root writes fail (cpuset
  not enabled at the top level, cgroup v1, or the reconciliation above cannot
  make progress), the node is unschedulable with a `core-layout:` reason,
  exactly like a bad layout — not "degraded," not still accepting jobs.
  Job-affinity enforcement is tightened the same way, but only where it is
  actually load-bearing: spec-core isolation (`core_spec_count`/
  `cpu_spec_list`) is enforced unconditionally at the **root** cpuset — every
  job inherits the root's effective set regardless of `constrain_cores`, so
  spec CPUs are excluded from any job whether or not it gets its own
  per-job cpuset. GPU-reservation isolation (`restricted_cores_per_gpu`) is
  different: reserved CPUs are ordinary, non-spec CPUs at the root, and what
  keeps a CPU-only job off them is its own **per-job** cpuset — which
  `constrain_cores=false` skips entirely, falling back to an affinity mask
  that (Yan's review correctly points out) included GPU-reserved CPUs and
  which a process can widen on its own regardless. So `constrain_cores=false`
  is rejected at config load only on a node that sets
  `restricted_cores_per_gpu` — not merely spec cores, which do not need a
  per-job cpuset to be protected. A process affinity mask is not an
  enforceable boundary; cgroup `cpuset.cpus` is. Sites that need per-GPU
  reservation get it only with `constrain_cores=true`.

#### `--cpus-per-gpu` and `def_cpu_per_gpu`

- `JobSpec` gains `cpus_per_gpu`. When set, a node's CPU count is
  `max(tasks × cpus_per_task, cpus_per_gpu ×` the job's GPUs on that node`)`.
- `def_cpu_per_gpu` is resolved at submit, next to `apply_default_time_limit`
  (`cluster.rs:1197`): the job's own value, else the partition's, else
  `[scheduler]`. It applies only to GPU jobs that set neither `-c` nor
  `--cpus-per-gpu`. A job listing several partitions uses the partition value
  only if all of them agree, else the `[scheduler]` value; a later partition
  change does not re-resolve submitted jobs. (Slurm resolves per partition at
  schedule time; this is a documented simplification.)
- That needs "was `-c` given" to reach the controller; today the CLI defaults
  `-c` to 1 (`sbatch.rs:45-52`). New clients send 0 on the wire for "not
  given". The controller converts it to `cpus_per_task = 1` plus a transient,
  non-persisted "not given" flag, so `cpus_per_task` stays ≥ 1 through size
  checks and the job_submit hook; if the hook sets `cpus_per_task`, the flag
  is cleared. `def_cpu_per_gpu` is resolved before the QoS and association
  checks, which size the job with the CPU helper. Old clients keep sending 1,
  which simply means `def_cpu_per_gpu` does not apply to them.

Rejected alternatives:

- **B — controller owns core IDs:** Slurm-exact and enables future
  topology-aware binding, but needs per-core bitmaps for every node in the
  controller, restart reconciliation, and larger launch messages.
- **C — node agent only:** the controller would keep sending CPU-only jobs to
  nodes that cannot run them, and they would fail at launch.
- **Replicating the per-node settings in the Raft log** (so `reconfigure`
  could apply them live): fully deterministic, but adds a persisted shape for a
  setting that changes rarely; restart-only was chosen instead.

## Examples

Field names below are the proposed ones; they are final only once the
Interfaces section is approved.

### Example node

All examples use one node type:

- 2 sockets × 64 cores, 2 hyperthreads per core → 256 CPUs.
- Linux CPU numbering: socket 0 is cores 0–63 (CPUs 0–63, hyperthread
  siblings 128–191); socket 1 is cores 64–127 (CPUs 64–127, siblings
  192–255).
- 8 GPUs: GPUs 0–3 on NUMA node 0 (socket 0), GPUs 4–7 on NUMA node 1
  (socket 1).
- 2 TB of memory.

Spur schedules CPUs (hyperthreads). Core-based settings (`core_spec_count`,
`restricted_cores_per_gpu`) are counted in **cores**, so each core is 2 CPUs
on this node.

### SPUR-386 — WEKA client

#### Configuration

```toml
# /etc/spur/spur.conf
[[nodes]]
names = "riot-[001-512]"
core_spec_count = 1            # one physical core for the WEKA frontend
# cpu_spec_list = "127,255"    # same thing, by CPU ID (not both)
reserved_memory_mb = 16384     # a.k.a. mem_spec_limit: WEKA client memory
```

```ini
# /etc/wekaio/service.conf (WEKA's setting, not Spur's)
isolate_cpusets=false
```

```bash
# Mount WEKA with its frontend pinned to the reserved core
mount -t wekafs -o core=127 <backend>/<fs> /mnt/weka
```

`core_spec_count = 1` picks the highest core on the highest socket, which is
core 127: CPUs 127 and 255. `cpu_spec_list = "127"` would expand to the same
whole core.

#### What happens

1. `spurd` starts and registers, reporting the node's real online CPU IDs,
   sockets and NUMA nodes.
2. The controller computes the spec pool `{127, 255}` and returns it in the
   registration response.
3. The node's schedulable totals become 254 CPUs (256 − 2) and 2 TB − 16 GB
   of memory.
4. `spurd` writes the `spur` cgroup root:

   ```
   /sys/fs/cgroup/spur/cpuset.cpus = 0-126,128-254
   /sys/fs/cgroup/spur/memory.max  = <2 TB − 16 GB>
   ```

   and keeps `spurd` and `spurstepd` off CPUs 127 and 255.
5. `scontrol show node riot-001` shows the reservation:

   ```
   CPUTot=256 CPUEfctv=254 CoreSpecCount=1 CPUSpecList=127,255 MemSpecLimit=16384
   ```

#### Jobs

```bash
# Whole-node job: gets every CPU except WEKA's
sbatch -N1 --exclusive -p cpu job.sh
# job cpuset: 0-126,128-254 — never 127 or 255
```

The WEKA frontend keeps running on core 127 no matter how busy the node is,
and no job can use the 16 GB reserved for it.

### SPUR-369 — reserved cores per GPU

#### Configuration

```toml
[[nodes]]
names = "riot-[001-512]"
core_spec_count = 1              # SPUR-386, as above
reserved_memory_mb = 16384
restricted_cores_per_gpu = 8     # 8 cores (16 CPUs) reserved for each GPU

[[partitions]]
name = "gpu"
nodes = "riot-[001-512]"
def_cpu_per_gpu = 16             # GPU jobs get 16 CPUs per GPU unless they ask

[[partitions]]
name = "cpu"
nodes = "riot-[001-512]"

[scheduler]
def_cpu_per_gpu = 8              # cluster-wide fallback for other partitions
```

GPU affinity comes from each GPU's NUMA node, so nothing else is needed on
this node. To override it (for example on a node that reports no NUMA
information), set `cores` on the GPU entries, in cores as in Slurm's
`gres.conf Cores=`:

```toml
[[devices.gres]]
name = "gpu"
type = "mi325x"
file = "/dev/dri/renderD[128-131]"   # GPUs 0-3
cores = "0-63"

[[devices.gres]]
name = "gpu"
type = "mi325x"
file = "/dev/dri/renderD[132-135]"   # GPUs 4-7
cores = "64-127"
```

#### Resulting pools

Each GPU gets the lowest 8 unused cores of its affinity list, skipping the
spec core:

| Pool | Cores | CPUs |
|---|---|---|
| Spec (WEKA) | 127 | 127, 255 |
| GPU 0 / 1 / 2 / 3 (socket 0) | 0–7 / 8–15 / 16–23 / 24–31 | each core plus its sibling (e.g. GPU 0: 0–7, 128–135) |
| GPU 4 / 5 / 6 / 7 (socket 1) | 64–71 / 72–79 / 80–87 / 88–95 | e.g. GPU 4: 64–71, 192–199 |
| General | 32–63, 96–126 | 32–63, 160–191, 96–126, 224–254 (126 CPUs) |

The controller tracks only counts: 126 general CPUs on the node, and for each
allocation how many of its CPUs came from its GPUs' reserved CPUs.

#### Workflows

**A GPU job with the default CPUs.**

```bash
sbatch -p gpu --gres=gpu:mi325x:2 train.sh
```

1. No `-c` and no `--cpus-per-gpu`, so `def_cpu_per_gpu = 16` applies:
   2 GPUs × 16 = 32 CPUs.
2. The controller picks GPUs 0 and 1. Together they bring 2 × 16 = 32
   reserved CPUs, so all 32 CPUs come from the reserved pool and the job needs
   nothing from the general pool. The launch message carries 32 CPUs, GPUs 0
   and 1, and the node's layout.
3. `spurd` gives it GPU 0's and GPU 1's cores: cpuset
   `0-15,128-143`, `ROCR_VISIBLE_DEVICES=0,1`.

**A GPU job that wants more CPUs than are reserved.**

```bash
sbatch -p gpu --gres=gpu:1 --cpus-per-gpu=24 prep.sh
```

The job gets 24 CPUs: its GPU's 16 reserved CPUs plus 8 from the general
pool, preferring the same socket.

**A CPU-only job.**

```bash
sbatch -p cpu -c 64 analytics.sh
```

It may use only the general pool (126 CPUs on this node): for example cpuset
`32-63,160-191`. It can never land on a GPU's reserved cores, so GPU jobs
always find their companion cores free.

**A CPU-only job that can never fit.**

```bash
sbatch -p cpu -N1 -c 200 big.sh
```

Without reserved cores it would fit (254 CPUs), but CPU-only jobs can only
use the 126 general CPUs. It stays `PENDING` with a reason that says so, for
example:

```
CPU-only job needs 200 CPUs; at most 126 per node are available outside GPU-reserved and spec cores
```

**A whole-node job.**

```bash
sbatch -p cpu -N1 --exclusive full.sh
```

Whole-node jobs may use reserved cores, so it gets all 254 non-spec CPUs.
(If the `cpu` partition also has a SPUR-368 cap below 254, SPUR-368's
never-runnable rule applies instead.)

**Rejected combination.**

```bash
sbatch -p gpu --gres=gpu:1 -c 8 --cpus-per-gpu=16 x.sh
# error: --cpus-per-gpu cannot be combined with --cpus-per-task (-c)
```

**A misconfigured node.**

```toml
restricted_cores_per_gpu = 20    # 4 GPUs × 20 = 80 cores, but socket 0 has 64
```

The node registers but is drained:

```
Reason=core-layout: restricted_cores_per_gpu=20 needs 80 unique cores near GPUs 0-3; 64 available
```

#### Showing it

```
$ scontrol show node riot-001
   CPUTot=256 CPUEfctv=254 CoreSpecCount=1 CPUSpecList=127,255 MemSpecLimit=16384
   RestrictedCoresPerGPU=8(CPUs:0-31,64-95,128-159,192-223)
   Gres=gpu:mi325x:8
```

On the node, each job's actual cores are in
`/sys/fs/cgroup/spur/job_<id>_<attempt>/cpuset.cpus.effective`.

## Build Targets

One design, two Stories. Each code path below names the Story that owns it.
Shared groundwork (topology, real CPU IDs, the layout function and its
delivery to the agent) belongs to SPUR-386, which ships first (P1, WEKA);
SPUR-369 builds on it. Sub-tasks are created per build target **per Story**,
each covering only that Story's code paths.

| Build Target | Type | Build File | SPUR-386 (shared + spec) | SPUR-369 (reserved per GPU) |
|---|---|---|---|---|
| spur-proto | library | `crates/spur-proto/Cargo.toml` (`proto/slurm.proto`) | topology, layout, node display fields | `cpus_per_gpu`, `def_cpu_per_gpu`, `reserved_cpus`, GPU affinity |
| spur-core | library | `crates/spur-core/Cargo.toml` | topology model, layout (spec part), node config | layout (reserved part), split formula, CPU-sizing helper, job/partition fields |
| spur-devices | library | `crates/spur-devices/Cargo.toml` | — | `cores=` merged onto discovered GPUs |
| spur-sched | library | `crates/spur-sched/Cargo.toml` | ledger on real CPU IDs, spec exclusion | general-pool fit, reserved/general core picking, `cpus_per_gpu` sizing |
| spurctld | binary | `crates/spurctld/Cargo.toml` | layout in apply step, restart-only settings, layout delivery, node display | placement, never-runnable and pending reasons, `def_cpu_per_gpu`, CPU-sizing consumers |
| spurd | library + binary | `crates/spurd/Cargo.toml` | topology discovery, layout adoption, cgroup root, daemon pinning | GPU affinity report, CPU-sizing consumers |
| spur-k8s | library + binary | `crates/spur-k8s/Cargo.toml` | — | pod CPU sizing via the helper |
| spur-cli | binary | `crates/spur-cli/Cargo.toml` | `scontrol show node` spec fields | `--cpus-per-gpu`, `-c` unset, reserved/partition display |

Not changed: `spur-stepd` (job setup lives in `spurd`'s executor; its
processes are re-pinned by `spurd`). `spur-ffi` is out of scope: adding
`cpus_per_gpu` to its C struct changes the ABI, so FFI clients keep
`cpus_per_task` semantics (a zero-filled `cpus_per_task` now means "not
given", documented).

Suggested order: spur-proto → spur-core → spur-devices/spur-sched → spurctld
and spurd → spur-k8s/spur-cli, for SPUR-386 first, then SPUR-369.

## Code Path Design

### Code Path: proto-topology-and-layout

**Build target:** spur-proto · **Story:** SPUR-386

**Responsibility:** Wire types for the agent's topology report, the layout
sent to the agent (registration reply and launch), and node display fields.

**Interfaces:** `CpuTopology`, `CoreLayout` messages; new optional fields on
`RegisterAgentRequest`, `RegisterAgentResponse`, `LaunchJobRequest`,
`NodeInfo` (exact tags in Interfaces).

**Dependencies:** none.

**Implementation Notes:** Append-only, new tags only. Every field is optional
or repeated, so old agents and controllers ignore them. Topology is compact:
one entry per physical core.

**Error Handling:** n/a (schema only).

### Code Path: proto-gpu-cpu-requests

**Build target:** spur-proto · **Story:** SPUR-369

**Responsibility:** `JobSpec.cpus_per_gpu`, `PartitionInfo.def_cpu_per_gpu`
(plus create/update partition requests), `ResourceAllocations.reserved_cpus`,
`GpuResource.numa_node`/`affinity_cpus`, `NodeInfo` reserved-CPU display.

**Dependencies:** proto-topology-and-layout.

**Implementation Notes:** `PartitionInfo` tags must follow whichever of
SPUR-368 (planning 45-51) and this Story lands first.

**Error Handling:** n/a.

### Code Path: cpu-topology-model

**Build target:** spur-core · **Story:** SPUR-386

**Responsibility:** `CpuTopology` (physical cores → socket, NUMA node, CPU
IDs), abstract core numbering (socket-major, by lowest CPU ID), CPU-list
parse/format helpers, proto conversions. `ResourceSet::clamped` does not copy
the topology into `total_resources`.

**Dependencies:** spur-proto.

**Implementation Notes:** Pure data; no `/sys` access here (that is in
`spurd`), so tests use fixtures.

**Error Handling:** malformed lists return an error, never panic.

### Code Path: core-layout-spec

**Build target:** spur-core · **Story:** SPUR-386

**Responsibility:** `CoreLayout::compute` skeleton plus the spec pool:
`core_spec_count` (Slurm order: highest core on highest socket, then
round-robin downward) or `cpu_spec_list` (OS CPU IDs) widened to whole cores;
general pool = the rest. `ResourceSet::clamped` subtracts the spec CPU count
from `cpus`.

**Dependencies:** cpu-topology-model.

**Implementation Notes:** One pure function, deterministic for the same
input, called by the controller only.

**Error Handling:** typed `LayoutError` (spec IDs not online, spec takes
every core, missing topology while spec is configured), each with a one-line
reason for the node's `core-layout:` reason.

### Code Path: node-config-spec

**Build target:** spur-core · **Story:** SPUR-386

**Responsibility:** `[[nodes]]` fields `core_spec_count`, `cpu_spec_list`,
`mem_spec_limit` as an alias of `reserved_memory_mb`, and `daemons_off_spec`;
`Config::validate()` checks; a helper listing the restart-only fields for
`reconfigure`.

**Dependencies:** none.

**Implementation Notes:** All new fields `serde(default)`; absent means
today's behavior.

**Error Handling:** reject both spec settings set, and unparsable
`cpu_spec_list`, at config load.

### Code Path: core-layout-reserved

**Build target:** spur-core · **Story:** SPUR-369

**Responsibility:** Extend `CoreLayout::compute` with
`restricted_cores_per_gpu`: per GPU, the lowest R unique cores of its
affinity (`cores=` if set, else its NUMA node's cores), skipping spec cores
and cores taken by earlier GPUs (GPUs in stable-id order); record each GPU's
reserved CPUs. Also the shared split formula
`reserved_share(cpus, reserved CPUs of the job's GPUs)` and the `[[nodes]]`
field `restricted_cores_per_gpu`.

**Dependencies:** core-layout-spec.

**Implementation Notes:** The split formula is the single definition used by
both scheduler fit and agent picking. Add the `Config::validate()`
cross-section check: `constrain_cores=false` is rejected if any `[[nodes]]`
entry sets `restricted_cores_per_gpu` (design review, finding 4). This check
belongs here, not in SPUR-392's already-merged `node-config-spec` validation,
because it is specific to GPU-reservation isolation: `restricted_cores_per_gpu`
is enforced per-job (a job's own cpuset must exclude other GPUs' reserved
cores, which `constrain_cores=false` skips), whereas `core_spec_count`/
`cpu_spec_list` are enforced unconditionally at the root cpuset regardless of
`constrain_cores` — so a spec-only node (SPUR-386, no `restricted_cores_per_gpu`)
does not need this rejection and must not get it. Do not duplicate this check
into `node-config-spec`.

**Error Handling:** `LayoutError` for a GPU with no affinity (no `cores=`, no
NUMA node, or a NUMA node without CPUs as on MI300A) and too few unique cores;
config rejects `constrain_cores=false` on a node that sets
`restricted_cores_per_gpu`.

### Code Path: cpu-sizing-helper

**Build target:** spur-core · **Story:** SPUR-369

**Responsibility:** `JobSpec.cpus_per_gpu`, the transient "`-c` not given"
flag, `PartitionConfig.def_cpu_per_gpu`, `SchedulerConfig.def_cpu_per_gpu`,
`ResourceAllocations.reserved_cpus` and `layout_fingerprint` (add/subtract),
and one helper for a job's CPUs per node and in total
(`max(tasks × cpus_per_task, cpus_per_gpu × GPUs)`). Move the `spur-core`
consumers to it: QoS and association limits (`qos.rs:52`,
`account_limits.rs:38`) and `--mem-per-cpu` memory (`job.rs:817`). Add
`def_cpu_per_gpu: Option<Option<u32>>` (`#[serde(default)]`) to
`WalOperation::PartitionUpdate` (`wal.rs:350`).

**Dependencies:** spur-proto.

**Implementation Notes:** All persisted additions `serde(default)` (they reach
`JobSubmit`/`JobStart` WAL entries); the "not given" flag is
`#[serde(skip)]`.

**Error Handling:** validation rejects `cpus_per_gpu = 0` and
`def_cpu_per_gpu = 0`.

### Code Path: gres-cores-merge

**Build target:** spur-devices · **Story:** SPUR-369

**Responsibility:** Make `cores=` overrides work on auto-detected nodes. Today
dedup needs an identical device-path set (`registry/device_registry.rs:222`),
and a discovered GPU carries both `/dev/dri/cardN` and `renderDN`
(`cdi/discovery.rs:341-342`), so a `file=/dev/dri/renderD…` entry never
matches and would add a duplicate GPU. New rule: a configured expanded entry
whose device paths overlap a discovered GPU's render node is merged into it
(its `cores` copied over) and then dropped; entries that overlap nothing keep
today's behavior. Precedence for a GPU's cores: `cores=` from `spur.conf`,
then the CDI `spur.amd.com/cores` annotation (`cdi/annotations.rs:14`), then
its NUMA node.

**Dependencies:** none.

**Error Handling:** an entry that overlaps several discovered GPUs is merged
into each of them; one that overlaps none stands alone as a configured GPU, as
today.

### Code Path: ledger-real-cpu-ids

**Build target:** spur-sched · **Story:** SPUR-386

**Responsibility:** `cons_tres::NodeAllocation` indexed by real CPU IDs
(online set, not `0..n`), spec CPUs permanently unavailable, restore working
on ID sets, `update_capacity` working on ID sets. **Restore recovers real
placement, not a guess (design review, finding 2):** for a descriptor with no
recorded CPU IDs, read the surviving job's live cgroup `cpuset.cpus.effective`
(path is `cgroup_path_for(job_id, run_attempt)`, deterministic from fields
every descriptor already carries) and mark exactly those CPU IDs busy. There
is no remaining case where guessing a placement is correct, so
`fallback_cpu_ids` (first-fit) is removed from this path entirely, not merely
deprioritized: if the cgroup itself no longer exists, the process did not
survive and there is nothing to place — that descriptor is cleaned up like any
other job exit, no CPU pick attempted. If the cgroup exists but its effective
cpuset cannot be parsed, hold the job's CPU count out of the free set as
unknown-but-occupied rather than guessing.

**Dependencies:** cpu-topology-model, core-layout-spec.

**Implementation Notes:** With no topology (old agent build) the online set is
`0..n`, which is today's behavior.

**Error Handling:** a recorded CPU ID no longer online or now spec is dropped
on restore with a warning (job keeps running; cgroup clamps it as today). A
job recovered from its live cgroup keeps running on exactly its prior CPUs —
no warning needed, since nothing was dropped.

### Code Path: general-pool-fit

**Build target:** spur-sched · **Story:** SPUR-369

**Responsibility:** Node fit and timeline checks use the general pool: CPU-only
jobs need `cpus ≤ general_free`; GPU jobs need `general_need ≤ general_free`;
exclusive jobs unchanged. `build_node_allocation` fills `reserved_cpus` from
the chosen GPUs. `base_node_request`, `cpu_per_node_counts` and
`plan_per_node_alloc` size CPUs with the helper. The controller's
`general_need` check assumes a GPU job's `reserved_share` always fits from
its own GPUs' reserved pool (design review, finding 1) — see
pool-aware-core-picking for the agent-side half of that invariant (the
reserved-share pick must be atomic, never spilling into general).

**Dependencies:** core-layout-reserved, cpu-sizing-helper.

**Implementation Notes:** Nodes without a layout keep today's single-pool
check. Shares files with SPUR-368 (`backfill.rs`, `timeline.rs`); rebase on
whichever lands first.

**Error Handling:** n/a (pure fit logic).

### Code Path: pool-aware-core-picking

**Build target:** spur-sched · **Story:** SPUR-369

**Responsibility:** Ledger allocation takes the job's own GPUs' free reserved
CPUs — **atomically, up to `reserved_share`** (design review, finding 1): if
fewer than `reserved_share` are actually free in the job's own GPUs' reserved
pool (for example a legacy allocation from before a layout change still holds
some of them), the pick fails outright rather than drawing the shortfall from
general. Only CPUs beyond `reserved_share` come from the general pool,
preferring the GPUs' sockets and whole cores. This is what keeps
general-pool-fit's invariant true: nothing is ever charged "0 general" by the
controller while physically consuming general CPUs.

**Dependencies:** ledger-real-cpu-ids, core-layout-reserved.

**Error Handling:** if the general pool is short, or the job's own GPUs'
reserved pool is short even by one CPU (controller and agent disagree, or a
legacy allocation squats it after a layout change), `AllocError::CpusUnavailable`
refuses the whole launch — never a partial allocation — so the job requeues
instead of running on the wrong cores or silently consuming uncounted general
capacity.

### Code Path: layout-in-apply-step

**Build target:** spurctld · **Story:** SPUR-386

**Responsibility:** `apply_node_config_policy` calls `CoreLayout::compute`
(skipped for Kubernetes-backed nodes) and stores the derived
`Node.core_layout` or `Node.core_layout_error` (excluded from placement,
shown as `DRAIN` with the `core-layout:` reason) and its fingerprint.
`RegisterAgent` returns the layout, and the launch dispatch path includes the
node's current `generation` alongside it in `LaunchJobRequest` so the agent
can reject a stale, delayed launch before adopting its layout (design review,
finding 6, reusing the existing generation field — no new proto field). The
heartbeat handler sets `reregister` when the agent's held fingerprint differs
or needed topology is missing. `reconfigure` rejects changes to the
restart-only fields before proposing anything. A node whose root enforcement
fails (spec-enforcement) is also marked with `Node.core_layout_error` and
excluded from placement, independent of `[cgroup] required` (design review,
finding 4) — enforcement failure and layout-computation failure share the
same unschedulable mechanism.

**Dependencies:** spur-core layout code paths.

**Implementation Notes:** Runs on every replica and on snapshot reconcile;
derived fields are `#[serde(skip)]`; no new WAL operation.

**Error Handling:** see Error Handling (unschedulable semantics, admin drains
win).

### Code Path: node-display-spec

**Build target:** spurctld · **Story:** SPUR-386

**Responsibility:** `node_to_proto` fills spec CPU list, spec core count and
spec memory as display fields; REST node output gets the same fields.

**Dependencies:** layout-in-apply-step.

### Code Path: reserved-placement-and-reasons

**Build target:** spurctld · **Story:** SPUR-369

**Responsibility:** Commit/release `reserved_cpus` with allocations, tagged
with the layout fingerprint, and count allocations from an older layout as
entirely general;
`resource_impossible_reason` and `update_pending_reasons` judge CPU-only and
GPU jobs against general capacity with a descriptive reason; node display of
reserved CPUs. The controller never needs to track which physical CPUs are
squatted (design review, finding 1) — that invariant is enforced entirely by
the agent-side atomic reserved-share pick in pool-aware-core-picking; this
code path's job is only to keep the counts (`general_free`,
`reserved_cpus`) consistent with what the agent is contractually allowed to
hand out.

**Dependencies:** general-pool-fit, layout-in-apply-step.

### Code Path: cpu-per-gpu-submit

**Build target:** spurctld · **Story:** SPUR-369

**Responsibility:** `proto_to_job_spec`/REST map `cpus_per_gpu` and "`-c` not
given"; `def_cpu_per_gpu` resolved next to `apply_default_time_limit`;
reject `--cpus-per-gpu` with explicit `-c` and without GPUs; partition
create/update/show fields and the `PartitionUpdate` WAL field (also passed by
`reconfigure`, `cluster.rs:5643`); resolve `def_cpu_per_gpu` before QoS and
association checks; move controller CPU-sizing consumers to the helper
(requested TRES and min CPUs `server.rs:5266,5294`, GrpTRES usage `job_tres`
`cluster.rs:9694`, prolog context `scheduler_loop.rs:624`, accounting
CPU-seconds `accounting/db.rs:558` from the allocated CPU total).

**Dependencies:** cpu-sizing-helper.

### Code Path: topology-discovery

**Build target:** spurd · **Story:** SPUR-386

**Responsibility:** Read real online CPU IDs and, per CPU,
`topology/core_id`, `physical_package_id`, and NUMA membership from
`/sys/devices/system/node`; report `CpuTopology`; ledger built on real IDs.

**Dependencies:** cpu-topology-model, ledger-real-cpu-ids.

**Implementation Notes:** `/sys` root injectable so tests use fixture trees.

**Error Handling:** unreadable topology → report none (layout then fails only
if the node's settings need it).

### Code Path: spec-enforcement

**Build target:** spurd · **Story:** SPUR-386

**Responsibility:** Adopt the layout from the registration reply or a launch
request when it differs and its `generation` is not older than the agent's
current one (design review, finding 6 — reject and keep the held layout
otherwise); send the held fingerprint in heartbeats and re-register when
asked.

Before writing a smaller `spur/cpuset.cpus`, reconcile against every live job
cgroup under `spur/`: a CPU is only removed from the root if no live job's own
`cpuset.cpus` would then have empty intersection with the new root set,
otherwise it is deferred until that job exits (design review, finding 3 —
cgroup v2 widens, rather than empties, a child whose cpuset no longer
intersects its parent's). When spec cores are set, lower `spur/memory.high`
to the target immediately (throttle only, never OOM-kills), and write
`spur/memory.max` to the same target only once a later `memory.current` read
confirms usage has settled at or below it — reporting the memory reservation
as `pending` until then, never active (design review, finding 5: `memory.high`
first removes the race, rather than merely reporting around it); reset both
`memory.high` and `memory.max` (plus the cpuset) when spec settings are gone.
Pin every thread of `spurd` and of each running
`spurstepd` off spec CPUs when `daemons_off_spec`, widening back when the
settings are removed; set each job process's affinity before exec to its
cpuset. `constrain_cores=false` combined with `restricted_cores_per_gpu` is
rejected at config load (design review, finding 4) — there is no "non-spec
CPUs" affinity fallback to fall back to. Spec-only nodes (no
`restricted_cores_per_gpu`) are unaffected: spec isolation is root-enforced
regardless of `constrain_cores`.

**Dependencies:** topology-discovery, layout-in-apply-step.

**Error Handling:** for a node with any spec/reserved setting, root write or
reconciliation failure makes the node unschedulable (`core-layout:` reason),
independent of `[cgroup] required` (design review, finding 4) — see Error
Handling. `[cgroup] required` keeps its prior meaning for other cgroup
degradation.

### Code Path: gpu-affinity-report

**Build target:** spurd · **Story:** SPUR-369

**Responsibility:** Copy each GPU's `numa_node` and `cores=` (converted from
core indices to CPU IDs) into `GpuResource`; pass the job's GPUs and the
layout into the ledger's picker at launch. For replaying adopted jobs without
recorded CPU IDs (`replay_adopted_allocations`), placement comes from the
cgroup-recovery step in ledger-real-cpu-ids (design review, finding 2), not
from this picker — a gone cgroup means a dead job to clean up, never a case
this picker handles; move agent CPU-sizing consumers to the helper
(`SPUR_CPUS_ON_NODE` `agent_server.rs:5367`, cgroup fallback
`agent_server.rs:1396`); set `SPUR_CPUS_PER_GPU`/`SLURM_CPUS_PER_GPU`.

**Dependencies:** gres-cores-merge, pool-aware-core-picking, spec-enforcement.

**Error Handling:** a `cores=` index beyond the node's cores is dropped with a
warning.

### Code Path: k8s-cpu-sizing

**Build target:** spur-k8s · **Story:** SPUR-369

**Responsibility:** Pod CPU requests and `SPUR_CPUS_ON_NODE`
(`spur-k8s/src/agent.rs:362`) use the CPU-sizing helper, so `--cpus-per-gpu`
jobs get correctly sized pods.

**Dependencies:** cpu-sizing-helper.

### Code Path: scontrol-node-spec

**Build target:** spur-cli · **Story:** SPUR-386

**Responsibility:** `scontrol show node` prints `CPUEfctv`, `CoreSpecCount`,
`CPUSpecList`, `MemSpecLimit` when set.

### Code Path: cpus-per-gpu-flags

**Build target:** spur-cli · **Story:** SPUR-369

**Responsibility:** `--cpus-per-gpu` on sbatch/salloc/srun (+ `#SBATCH`,
`SBATCH_CPUS_PER_GPU`/`SALLOC_CPUS_PER_GPU`/`SLURM_CPUS_PER_GPU` env), `-c`
sent as 0 when not given, client-side conflict errors; `scontrol show node`
`RestrictedCoresPerGPU=`; `scontrol show partition` `DefCpuPerGPU=`; `sacct`
and `sstat` CPU columns (`sacct.rs:276`, `sstat.rs:208`) use the helper.

### Docs

Each Sub-task updates the docs for its user-visible surface:
`docs/admin-guide/configuration.rst` (`[[nodes]]`, `[[partitions]]`,
`[scheduler]`, `[devices]` `cores` units, `[cgroup]`, restart-only fields in
"Applying configuration changes"), `docs/user-guide/submitting-jobs.rst`
(flags), `docs/user-guide/monitoring-jobs.rst` (`scontrol show node`),
`docs/deployment/native-host.rst` (cpuset delegation, daemon pinning, WEKA
`isolate_cpusets=false`, and the operator runbook for draining jobs before a
`memory.max` tightening that cannot otherwise complete — design review,
finding 5), `docs/deployment/upgrading.rst` (upgrade all controllers before
setting the new fields), `docs/deployment/kubernetes.rst`
(layout not applied to Kubernetes-backed nodes),
`docs/deployment/partitioning.rst` (`def_cpu_per_gpu`),
`docs/migration-from-slurm.rst` and `docs/user-guide/slurm-compatibility.rst`
(parity notes: OS CPU IDs in `cpu_spec_list`, `def_cpu_per_gpu` resolved at
submit, per-GPU reserved pools).

## Cross-Target Interfaces

### Configuration (`spur.conf`)

| Section | Field | Type / default | Story | Meaning |
|---|---|---|---|---|
| `[[nodes]]` | `core_spec_count` | `u32`, unset | 386 | Cores set aside for node services, Slurm order. |
| `[[nodes]]` | `cpu_spec_list` | CPU list string, unset | 386 | Same, by OS CPU ID (`"127,255"`, `"0-1"`); widened to whole cores. Not with `core_spec_count`. |
| `[[nodes]]` | `reserved_memory_mb` (alias `mem_spec_limit`) | `u64`, 0 | 386 | Existing field; additionally enforced on the node when spec cores are set. Alias via `#[serde(alias)]`. |
| `[[nodes]]` | `daemons_off_spec` | `bool`, `true` | 386 | Pin `spurd`/`spurstepd` off spec CPUs (Slurm `SlurmdOffSpec`). Only matters when spec cores are set. |
| `[[nodes]]` | `restricted_cores_per_gpu` | `u32`, unset | 369 | Cores reserved for each GPU (Slurm `RestrictedCoresPerGPU`); 0 = off. |
| `[[partitions]]` | `def_cpu_per_gpu` | `u32`, unset | 369 | Default CPUs per GPU for this partition. |
| `[scheduler]` | `def_cpu_per_gpu` | `u32`, unset | 369 | Cluster-wide fallback. |
| `[devices]` `[[devices.gres]]` | `cores` | existing | 369 | Units defined as Slurm core indices (was parsed but unused); merged onto the matching auto-detected GPU. |

The `[[nodes]]` fields above are restart-only: `scontrol reconfigure` rejects
changes to them. The settings are read by the controller; `spurd` gets what it
needs in the layout, so per-node settings live in one place.

### Core types (`spur-core`)

```rust
pub struct CoreInfo { pub socket: u32, pub numa_node: Option<u32>, pub cpus: Vec<u32> }
pub struct CpuTopology { pub cores: Vec<CoreInfo> }   // abstract core index = position

pub struct GpuReservedCpus { pub stable_id: u64, pub cpus: Vec<u32> }
pub struct CoreLayout {
    pub spec_cpus: Vec<u32>,
    pub mem_spec_mb: u64,          // 0 unless spec cores are set
    pub gpu_reserved: Vec<GpuReservedCpus>,
    pub general_cpus: Vec<u32>,
    pub daemons_off_spec: bool,
}

pub struct NodeCoreSettings { /* the [[nodes]] fields above */ }

impl CoreLayout {
    /// Ok(None) when the node has none of the settings: behaves exactly as today.
    pub fn compute(topology: Option<&CpuTopology>, gpus: &[GpuResource],
                   settings: &NodeCoreSettings) -> Result<Option<CoreLayout>, LayoutError>;
    /// Sum of the reserved CPUs of the given GPUs.
    pub fn reserved_for(&self, gpu_stable_ids: &[u64]) -> u32;
    /// Stable across builds: FNV-1a over a canonical encoding.
    pub fn fingerprint(&self) -> u64;
}

/// CPUs a job takes from its GPUs' reserved CPUs.
pub fn reserved_share(cpus: u32, reserved_of_its_gpus: u32) -> u32;

impl JobSpec {
    /// The job's CPUs on a node holding `gpus_on_node` of its GPUs.
    pub fn cpus_on_node(&self, tasks_on_node: u32, gpus_on_node: u32) -> u32;
    /// The job's total CPUs (for limits, TRES and memory sizing).
    pub fn total_cpus(&self) -> u64;
}
```

New persisted fields, all `#[serde(default)]`: `ResourceSet.topology:
Option<CpuTopology>` (kept only in `detected_resources`),
`GpuResource.numa_node: Option<u32>`, `GpuResource.affinity_cpus: Vec<u32>`,
`ResourceAllocations.reserved_cpus: u32`,
`ResourceAllocations.layout_fingerprint: u64` (controller-side only, not on
the wire), `JobSpec.cpus_per_gpu: Option<u32>`, `Partition.def_cpu_per_gpu:
Option<u32>`, `WalOperation::PartitionUpdate.def_cpu_per_gpu:
Option<Option<u32>>`. Derived,
`#[serde(skip)]`: `Node.core_layout`, `Node.core_layout_error`, the
transient "`-c` not given" flag on `JobSpec`.

### Proto (`proto/slurm.proto`, append-only)

```proto
message CoreInfo    { uint32 socket = 1; optional uint32 numa_node = 2; repeated uint32 cpus = 3; }
message CpuTopology { repeated CoreInfo cores = 1; }
message GpuReservedCpus { uint64 stable_id = 1; repeated uint32 cpus = 2; }
message CoreLayout {
  repeated uint32 spec_cpus = 1;   uint64 mem_spec_mb = 2;
  repeated GpuReservedCpus gpu_reserved = 3;   repeated uint32 general_cpus = 4;
  bool daemons_off_spec = 5;
}

RegisterAgentRequest  { CpuTopology topology = 9; }                         // 386
RegisterAgentResponse { CoreLayout core_layout = 4; }                       // 386, absent = none
LaunchJobRequest      { CoreLayout core_layout = 14; }                      // 386, absent = none
HeartbeatRequest      { uint64 core_layout_fingerprint = 8; }               // 386, 0 = none held
HeartbeatResponse     { bool reregister = 1; }                              // 386
NodeInfo              { string spec_cpus = 59; uint32 core_spec_count = 60;
                        uint64 mem_spec_mb = 61; }                          // 386, display
NodeInfo              { uint32 restricted_cores_per_gpu = 62;
                        string reserved_cpus = 63; }                        // 369, display
GpuResource           { optional uint32 numa_node = 7; repeated uint32 affinity_cpus = 8; } // 369
ResourceAllocations   { uint32 reserved_cpus = 5; }                         // 369
JobSpec               { optional uint32 cpus_per_gpu = 86; }                // 369
PartitionInfo, CreatePartitionRequest, UpdatePartitionRequest:
                      { optional uint32 def_cpu_per_gpu = <next free>; }    // 369
UpdatePartitionRequest{ bool clear_def_cpu_per_gpu = <next free>; }
```

`JobSpec.cpus_per_task` (tag 13) keeps its type; on the wire 0 now means "not
given". The controller converts it to 1 plus the transient flag immediately,
so in-memory and stored jobs always have `cpus_per_task ≥ 1`. Partition tags
are assigned after SPUR-368's (45-51) if it lands first. The topology is not
part of `ResourceSet` on the wire, so node listings do not carry it.

### Submit-time resolution (`spurctld`)

For every submit path (gRPC, REST, srun-created jobs), in `submit_job` next to
`apply_default_time_limit`:

1. `cpus_per_gpu` set and no GPU request → reject: `--cpus-per-gpu requires a
   GPU request` (Slurm rejects the same, `src/interfaces/gres.c:8271`).
2. `cpus_per_gpu` set and `-c` given → reject: `--cpus-per-gpu cannot be
   combined with --cpus-per-task (-c)`.
3. `-c` not given, GPU request, no `cpus_per_gpu` → `cpus_per_gpu =` partition
   `def_cpu_per_gpu` (all listed partitions agreeing), else `[scheduler]
   def_cpu_per_gpu`, if any.

Per node, the CPU request is `JobSpec::cpus_on_node`:
`max(tasks_on_node × cpus_per_task, cpus_per_gpu × gpus_on_node)`.

### CLI

| Command | Change |
|---|---|
| `sbatch`, `salloc`, `srun` | `--cpus-per-gpu=<n>`; `#SBATCH --cpus-per-gpu`; input env `SPUR_CPUS_PER_GPU` + `SBATCH_CPUS_PER_GPU` / `SALLOC_CPUS_PER_GPU` / `SLURM_CPUS_PER_GPU` (srun). `-c` no longer defaults to 1 on the wire. Same two errors as above, raised client-side first. |
| `srun` inside an allocation | `--cpus-per-gpu` rejected for steps ("not supported for job steps"); step GPU requests do not exist yet. |
| Job environment | `SPUR_CPUS_PER_GPU` / `SLURM_CPUS_PER_GPU` set when the job has `cpus_per_gpu`; `SPUR_CPUS_ON_NODE` uses the helper. |
| `scontrol show node` | `CPUTot` (all online), `CPUEfctv` (minus spec), `CoreSpecCount`, `CPUSpecList`, `MemSpecLimit`, `RestrictedCoresPerGPU=<R>(CPUs:<list>)` — each only when set. |
| `scontrol show partition` | `DefCpuPerGPU=<n>` when set. |
| `scontrol update partition` / `spur partition` create | `def_cpu_per_gpu` settable and clearable. |
| `scontrol reconfigure` | Fails with "restart all controllers to apply" if a restart-only `[[nodes]]` field changed. |

### REST

`SubmitJobFields.cpus_per_gpu: Option<u32>`; an absent `cpus_per_task` means
"not given" instead of 1. Node responses include the display fields;
partition responses include `def_cpu_per_gpu`.

### `spurd` ↔ `spurctld`

- **Register:** `RegisterAgentRequest.topology` and per-GPU
  `numa_node`/`affinity_cpus` out; `RegisterAgentResponse.core_layout` back.
  `spurd` keeps the last layout in memory; it is not persisted locally (it
  re-registers on every start).
- **Launch:** `LaunchJobRequest.core_layout` carries the node's current layout,
  alongside `allocated.generation` (already sent today for allocation
  staleness). If the layout differs from the held one **and** `generation` is
  not older than what `spurd` currently holds, `spurd` adopts it (re-applying
  the cgroup root and pinning) before picking cores; an older generation is
  rejected via the existing stale-dispatch path without touching the held
  layout (design review, finding 6 — no new field, the launch's existing
  generation now also orders layout adoption). Absent `core_layout` means "no
  layout": an old controller, or a node without the settings.
- **Heartbeat:** `spurd` sends the fingerprint of its held layout; the reply's
  `reregister` asks it to register again (layout changed, or the controller
  lacks topology the node's settings need). Old agents ignore the flag; old
  controllers never set it.
- **Node files written by `spurd`:** `/sys/fs/cgroup/spur/cpuset.cpus`
  (reconciled against live job cgroups before any CPU is removed — design
  review, finding 3), `/sys/fs/cgroup/spur/memory.high` (only with spec cores,
  lowered immediately, throttle-only), `/sys/fs/cgroup/spur/memory.max` (only
  with spec cores, written and reported active only once usage has settled
  under target and a later read confirms it — finding 5);
  per-job `cpuset.cpus` as today.

## Error Handling

### Failure modes

| Where | Failure | Behavior |
|---|---|---|
| Config load | `core_spec_count` and `cpu_spec_list` both set; unparsable `cpu_spec_list`; `def_cpu_per_gpu = 0` | `Config::validate()` rejects the config, as for other invalid settings. `restricted_cores_per_gpu = 0` means off. |
| Reconfigure | A restart-only `[[nodes]]` field changed | `reconfigure` fails with "restart all controllers to apply"; nothing changes. |
| Layout (controller) | Settings need topology but the controller has none for the node | The heartbeat reply asks `spurd` to re-register (fixes a `spurd` that was upgraded before the controllers). Until topology arrives the node is unschedulable, reason `core-layout: no CPU topology from spurd; restart or upgrade spurd`. |
| Layout | Spec IDs not online; spec takes every core; a GPU with no affinity (no `cores=`, no NUMA node, or a NUMA node without CPUs as on MI300A); too few unique cores | Node unschedulable with a one-line reason naming the GPU and numbers, e.g. `core-layout: restricted_cores_per_gpu=20 needs 80 unique cores near GPUs 0-3; 64 available`. |
| Layout | Kubernetes-backed node matches a `[[nodes]]` entry with the new fields | Layout skipped, logged once; the node behaves as today. |
| Layout | Changes while jobs run (restart with a new R or spec, CPU offlined) | Running jobs keep their cores. Their allocations carry the old fingerprint and count as entirely general; the general free count saturates at 0, so no new job lands until enough is released. A GPU job needing reserved-share CPUs still held by such a legacy job fails to launch and requeues (finding 1) rather than spilling into general. Idle agents pick up the new layout through the heartbeat `reregister`. |
| Agent restart | Job without recorded CPU IDs (non-head node of a multi-node job) | `spurd` recovers its actual CPU IDs by reading the live cgroup's `cpuset.cpus.effective` at the job's deterministic cgroup path, never by re-picking (finding 2). Only if the cgroup itself is gone does this fall through to normal job-exit cleanup. |
| Agent restart | Cgroup exists but its effective cpuset cannot be read | The job's CPU count is held out of the general pool as unknown-but-occupied until an operator resolves it; never guessed. |
| Node enforcement | Spec memory is set, `memory.current` above target | `spur/memory.high` is lowered to target immediately (throttle only, never OOM); `spur/memory.max` is written, and the reservation reported active, only once a later `memory.current` read confirms usage has settled at or below target (finding 5). Draining existing jobs to help it settle is an operator action, not automatic. |
| Agent topology | `/sys` topology unreadable | Report no topology; fails only if the node has settings that need it. |
| Agent `cores=` | Index beyond the node's cores | Entry ignored with a warning; the GPU falls back to its NUMA node, or the layout fails if it has none. |
| Launch | Agent's reserved-share CPUs for the job's own GPUs are short (controller and agent disagree, or squatted by a legacy allocation after a layout change) | `AllocError::CpusUnavailable` → `ResourceExhausted` → controller's existing `ResourcesUnavailable` requeue (`scheduler_loop.rs:2211`). The job never runs on wrong cores, and never spills the shortfall into general (finding 1). |
| Launch | `LaunchJobRequest.allocated.generation` is older than the agent's current generation | Rejected as stale via the existing generation-mismatch path, before any `core_layout` is adopted (finding 6) — the agent's held layout is never regressed by a delayed message. |
| Agent restart | A recorded CPU ID is no longer online or is now spec | Dropped from the replayed set with a warning; the job keeps running (cgroup clamps it, as today). |
| Node enforcement | Writing `spur/cpuset.cpus` or `spur/memory.max` fails (cpuset not enabled at the top level, cgroup v1), or root-cpuset reconciliation against live jobs cannot make progress | Logged on each layout change. A node with any spec/reserved setting is unschedulable with a `core-layout:` reason — enforcement is a prerequisite for the node accepting jobs under these settings, independent of `[cgroup] required` (finding 4). `[cgroup] required` keeps its prior meaning for cgroup degradation unrelated to these features. |
| Config load | `restricted_cores_per_gpu` is configured together with `constrain_cores = false` | Rejected at config load (finding 4) — affinity alone is not an enforceable boundary for per-job GPU-reservation isolation. Spec-only settings (`core_spec_count`/`cpu_spec_list`) are unaffected — root enforcement does not depend on `constrain_cores`. |
| Node enforcement | Tightening the root cpuset would leave a live job's own cpuset with no overlap with the new set | That CPU is deferred (kept in the root's and the job's allowed set) until the job exits, instead of letting the job's effective cpuset widen to everything non-spec per cgroup v2 semantics (finding 3). |
| Node enforcement | Pinning a `spurd` thread or `spurstepd` fails | Warning; not fatal. Spec CPUs are still kept from jobs by the root cpuset. |
| Submit | `--cpus-per-gpu` without GPUs, with `-c`, `= 0`, or in a job step | Rejected with the messages in Interfaces, client-side first and again on the controller. |

### "Unschedulable" for a bad layout

A failed layout sets the derived `Node.core_layout_error` in the apply step
(no WAL operation). While it is set the node is excluded from placement and
shown as `DRAIN` with `Reason=core-layout: …`. An admin drain/resume does not
touch it, and its reason never overwrites an admin reason. It clears as soon as
a later computation succeeds (agent re-registers with a fixed topology, or the
controllers restart with a fixed config).

### Upgrade safety

Not a breaking change (no `!` in the PR title), provided the rules below hold.

- **Persisted state:** every new persisted field on `ResourceSet`,
  `GpuResource`, `ResourceAllocations`, `JobSpec` and `Partition` is
  `#[serde(default)]`/`Option`; derived `Node` fields are `#[serde(skip)]`;
  nothing is renamed or removed. A new controller replays old WAL entries and
  snapshots unchanged. No persisted type uses `deny_unknown_fields`, so an old
  controller can still read new entries on rollback (it ignores the new
  fields).
- **Proto:** new tags only. `cpus_per_task = 0` from a new client to an old
  controller is clamped to 1 there (`server.rs:4812`), the same result. Old
  clients always send ≥ 1, which the new controller treats as an explicit `-c`
  (so `def_cpu_per_gpu` does not apply to them).
- **Mixed agent/controller versions:**
  - new controller + old `spurd`: no topology; nodes without the new settings
    behave as today; nodes with them are unschedulable with the "restart or
    upgrade spurd" reason.
  - old controller + new `spurd`: topology dropped as an unknown field, no
    layout sent, no root writes or pinning. The only change is the ledger
    using real CPU IDs (the prerequisite fix). Once the controllers are
    upgraded, the heartbeat `reregister` makes these agents send their
    topology again, so the upgrade order does not matter.
- **WAL operations:** `WalOperation::PartitionUpdate` gains
  `def_cpu_per_gpu: Option<Option<u32>>` with `#[serde(default)]`; old
  entries replay as "no change".
- **HA controllers:** the layout is derived on each replica, so an old replica
  would schedule without pools after failover. Upgrade all controllers before
  setting the new `[[nodes]]` fields (documented in `upgrading.rst`).
- **Config:** additive; `mem_spec_limit` is an alias, `reserved_memory_mb`
  keeps working and, without spec cores, behaves exactly as before.
  `[devices.gres] cores` gains defined units, but it was parsed and never used
  before, so no existing behavior changes.
- **Visible but opt-in:** on nodes with spec cores, `NodeInfo.total_resources.cpus`
  (and `sinfo` CPUs) drops by the spec CPUs. `scontrol` keeps `CPUTot` as all
  online CPUs and adds `CPUEfctv`, as Slurm does.
- **One genuine breaking change (design review, finding 4; flagged, not
  silently absorbed):** SPUR-399's `constrain_cores=false` +
  `restricted_cores_per_gpu` rejection (core-layout-reserved) makes a
  previously-valid `spur.conf` combination fail to load. It cannot land before
  SPUR-399, because `restricted_cores_per_gpu` does not exist until then — so
  any site that deployed SPUR-386 alone with `constrain_cores=false` (valid
  under SPUR-392) and later adds `restricted_cores_per_gpu` under SPUR-369
  must set `constrain_cores=true` at the same time, or the controller refuses
  to start. Spec-only configs (no `restricted_cores_per_gpu`) are never
  affected — this is the one exception to "not a breaking change" above; the
  Sub-task that lands it uses `!` in its PR title
  (`feat(spur-core)!: reject constrain_cores=false with restricted_cores_per_gpu`)
  and `upgrading.rst` calls it out explicitly. The alternative — accepting the
  combination and only warning — was rejected: it would let a node keep
  running with an isolation guarantee that silently does not hold.

### Design review findings (Yan Sun, 2026-10-30)

An external review of the 2026-10-29 approved design (commit `ca58e3d`) found
five must-fix and one should-fix gap in the transition and recovery rules.
Verified independently (the cgroup v2 claim against the kernel docs; the
accounting and replay claims against the current, not-yet-implemented
SPUR-393/394/395 code) before resolving. Each is addressed inline above
(Architecture details, failure-modes table) and in the Testing Strategy
below; summary:

| # | Finding | Fix |
|---|---|---|
| 1 | Accounting invariant breaks once a legacy allocation squats a GPU's newly-reserved cores and a different job later releases a general-pool charge for CPUs it never held | A job's reserved-share CPUs come only from its own GPUs' reserved pool, atomically; short → fail and requeue, never spill into general |
| 2 | Restart replay re-picks synthetic CPU IDs for a job with no recorded placement, which cannot move the already-running process | Recover the real CPU IDs from the surviving job's live cgroup (`cpuset.cpus.effective`) before ever falling back to a picker |
| 3 | cgroup v2 widens a child's effective cpuset to the parent's full set when the child's own cpuset no longer intersects it — confirmed against the kernel docs | Reconcile live jobs before tightening the root cpuset; never remove a CPU that would empty a running job's intersection |
| 4 | Root-write failure let a node keep accepting jobs on controller-only accounting while still appearing to enforce; `constrain_cores=false` affinity included GPU-reserved CPUs | Enforcement success is mandatory for a node with any spec/reserved setting (independent of `[cgroup] required`); reject `constrain_cores=false` at config load, but only when `restricted_cores_per_gpu` is set — spec-only isolation is root-enforced regardless of `constrain_cores`, so it needs no new rejection (and gets none, to avoid an unflagged break for existing SPUR-386-only configs) |
| 5 | Checking `memory.current` before writing a lower `memory.max` is not race-safe and can reclaim/OOM unrelated jobs | Lower `memory.high` first (throttle-only, never OOM, per the cgroup v2 docs) and wait for usage to settle before writing `memory.max`, instead of merely reporting around the race; expose a `pending` state until then; draining is an explicit operator action |
| 6 | A delayed in-flight launch computed under an older topology could regress the agent to a stale layout after a newer re-registration | Gate `core_layout` adoption on the launch's existing `allocated.generation` (reuses the generation-mismatch check already used for a related purpose) |

## Testing Strategy

Unit and integration tests run under `cargo test --locked` with no external
services or host dependencies: topology comes from fixture structs or fixture
`/sys` trees under a temp dir (the `/sys` root becomes injectable in
`reporter.rs`), never from the test machine. Hardware behavior is covered by
`tests/native_host/e2e/test_core_reservation.py`.

| Category | What to Test |
|---|---|
| Functional — layout (`spur-core`) | `core_spec_count` order on 1- and 2-socket, SMT and non-SMT fixtures (example node → core 127 = CPUs 127, 255); `cpu_spec_list` (OS IDs) widening to whole cores; reserved cores per GPU from NUMA and from `cores=` (lowest R unique, skipping spec and earlier GPUs; matches the "Resulting pools" table); general = the rest; `Ok(None)` with no settings; deterministic output for equal input. |
| Functional — split, fit and sizing (`spur-core`, `spur-sched`) | `reserved_share` table; CPU-only jobs fit only in general capacity; GPU jobs need only `general_need`; exclusive jobs take everything; nodes without a layout behave exactly as before (existing fit tests unchanged); `cpus_on_node`/`total_cpus` with and without `cpus_per_gpu`, incl. uneven `--gpus` splits; QoS limits and `--mem-per-cpu` use the helper. |
| Functional — invariant | Exhaustive enumeration over small node shapes (1-2 sockets, 1-4 GPUs, R = 1-3, SMT on/off, every short start/finish sequence drawn from a fixed list of job shapes, including jobs placed before the layout existed, under an older layout (R or spec changed), and jobs replayed after an agent restart without recorded CPU IDs): whenever the controller's count check admits a job, the agent's picker finds real CPUs for it, never hands out spec CPUs, and never gives a job another GPU's reserved CPUs. **Must include the design review's exact counterexample** (review finding 1): a layout change makes 4 CPUs newly GPU-reserved while a pre-existing CPU-only job occupies them (charged general); a GPU job needing those 4 as reserved-share requeues instead of spilling into general; after the legacy job exits and the GPU job retries successfully, the controller's general-free count matches physical general availability at every step. |
| Functional — ledger (`spur-sched`) | Real CPU IDs on non-contiguous online sets (`0-63,128-191`); offline IDs never handed out; restore/fallback/`update_capacity` on ID sets; own-GPU reserved then general picking with same-socket and whole-core preference; a job's reserved-share pick is atomic — partial availability in the reserved pool fails the whole pick rather than drawing the rest from general (finding 1). |
| Functional — restart recovery (`spurd`) | A surviving job with no recorded CPU IDs has its real `cpuset.cpus.effective` read from its live, deterministically-named cgroup and those exact CPU IDs marked busy in the rebuilt ledger (finding 2); a new allocation is refused the job's actual CPUs even though the stepd descriptor never recorded them; the cgroup-gone case still falls through to normal cleanup; an unreadable-but-present cgroup holds the job's CPU count out of the general pool as unknown rather than guessing. |
| Functional — devices (`spur-devices`) | `file=/dev/dri/renderD…` entry with `cores=` overlapping auto-detected GPUs (which also carry `cardN`) is merged and dropped, no duplicate GPUs; `cores=` beats the CDI annotation; unmatched entries unchanged. |
| Functional — controller (`spurctld`) | Register with topology → layout stored, `total_resources.cpus` minus spec, layout in the response and in launch requests; snapshot reconcile recomputes; Kubernetes-backed nodes skipped; `reconfigure` rejects restart-only changes before proposing anything; heartbeat `reregister` on fingerprint mismatch and on missing topology; allocations under an older fingerprint counted as general; GrpTRES usage and accounting CPU-seconds from the helper / allocated total; `PartitionUpdate` replay with and without `def_cpu_per_gpu`; `core_layout_error` set/cleared, excluded from placement, shown as DRAIN with reason, never overwrites an admin reason, and now also set when root enforcement fails on a node with spec/reserved settings (finding 4); `reserved_cpus` committed and released; never-runnable and pending reason text for CPU-only and GPU jobs too large for the general pool; `def_cpu_per_gpu` precedence (job > partition > scheduler, only when `-c` absent; multi-partition agreement rule); a `LaunchJobRequest` whose `allocated.generation` is older than the node's current generation is rejected before any layout change, confirmed via the existing generation-mismatch path (finding 6). |
| Functional — agent (`spurd`) | Fixture `/sys` → `CpuTopology`; GPU `cores=` converted to CPU IDs; layout adoption from registration and from a launch request, gated so a stale-generation launch never regresses the held layout (finding 6); root `cpuset.cpus` (and `memory.max` only with spec cores) written and reset under a temp cgroup root; tightening the root cpuset skips any CPU still needed to keep a live job's cpuset non-empty, and sweeps it in once that job exits (finding 3, with a fixture asserting the job's effective cpuset never widens past its own prior set); `memory.high` is lowered to target immediately regardless of current usage (never gated, never OOM-risking), `memory.max` write is deferred and the reservation reported `pending` until a later `memory.current` read confirms usage has settled at or below target, then `memory.max` is written and the reservation reported active (finding 5); affinity masks computed off spec for all `spurd` and `spurstepd` threads and widened back on removal; `constrain_cores=false` is rejected at config load only when combined with `restricted_cores_per_gpu` (finding 4), and NOT when only spec settings are present; heartbeat fingerprint and `reregister` handling; replay of jobs without recorded CPU IDs recovers from the live cgroup, not the picker (finding 2); env (`SPUR_CPUS_ON_NODE`, `SPUR_CPUS_PER_GPU`). |
| Functional — k8s and CLI | Pod CPU request uses the helper; `--cpus-per-gpu` parsing, `#SBATCH` and env precedence; `-c` absent sent as 0; `scontrol show node/partition` lines with and without the new fields. |
| Negative | Config validation errors, incl. `restricted_cores_per_gpu` combined with `constrain_cores=false` (rejected) and spec-only settings combined with `constrain_cores=false` (accepted — finding 4); every `LayoutError` with its message; `--cpus-per-gpu` with `-c`, without GPUs, `= 0`, in a step (client and server); agent reserved-share shortfall → `ResourceExhausted`, never partial-allocated (finding 1); stale-generation launch → rejected, layout unchanged (finding 6); root write or reconciliation failure → node unschedulable regardless of `[cgroup] required` on a node with spec/reserved settings (finding 4); `cores=` beyond the node. |
| Upgrade / compatibility | Deserialize pre-change `WalOperation` and snapshot JSON fixtures (no new fields) into new types; proto round-trips with fields absent; old-client `cpus_per_task = 1` treated as explicit; new controller + agent without topology (with and without settings); launch without `core_layout` leaves the agent's layout off; a node with only `reserved_memory_mb` gets no root writes. |
| E2E (native host) | On a 2-socket GPU node: GPU job cpuset equals its GPUs' reserved cores; CPU-only jobs never overlap reserved or spec CPUs while GPU jobs are queued; whole-node job gets every non-spec CPU; misconfigured node shows the `core-layout:` reason; a job whose sole CPU becomes spec/reserved during a live layout change keeps its prior CPU, never widens to the rest of the node (finding 3); killing and restarting `spurd` under running jobs recovers their real cpusets from the cgroup, not synthetic ones (finding 2). On any multi-core VM (SPUR-386): spec CPUs absent from every job's `cpuset.cpus.effective`, `spur/memory.high` = total − spec immediately, `spur/memory.max` = total − spec only once usage has settled under it and been confirmed by a later read (finding 5), all `spurd` threads and `spurstepd` affinity exclude spec CPUs, `scontrol show node` fields. |
| Scale | Topology and layout size for a 384-CPU, 8-GPU node (one entry per core, a few KB) and a 1,000-node `scontrol show node` listing stays well under the gRPC message limit (topology is not in listings); layout computation for a 512-node registration burst stays negligible. |
| Performance | No extra per-job work in the controller beyond one subtraction; placement benchmarks (if present) unchanged for nodes without a layout. |

## Open Questions

- PartitionInfo tag numbers depend on merge order with SPUR-368.
- Step-level `--cpus-per-gpu` waits for GPU requests on job steps.
- `spur-ffi` gains `cpus_per_gpu` only with a future ABI revision.
- CPX-partitioned GPUs: R counts per logical GPU, as Slurm counts per GRES
  device; confirm this is what RIOT operators expect.
- Design review finding 6's fix reuses `ResourceAllocations.generation` for
  layout ordering, which fully covers the common case (topology changes with
  no controller restart). A coordinated multi-controller restart for a
  `[[nodes]]` config change is not given its own ordering guarantee beyond the
  existing heartbeat-`reregister` convergence; flagged as an accepted
  limitation of that restart-only path, not a gap in the fix.
- The memory-reservation `pending` state (finding 5) needs a visible home.
  Default for SPUR-395 unless an engineer decides otherwise: a log line plus a
  metric/counter (no new proto field, no node drain — CPU-side enforcement can
  be fully active while memory enforcement is still pending, so it must not
  share the hard unschedulable mechanism). Promote to a `scontrol`/`NodeInfo`
  display field later only if operators ask for it.
