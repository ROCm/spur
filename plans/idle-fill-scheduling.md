# Idle-Fill Scheduling

Let a job that has exhausted its QOS group *node* quota run on nodes that are
otherwise idle, and take those nodes back when a job with a genuine quota claim needs
them. Behind one cluster switch, `scheduler.idle_fill_enabled`, defaulting to off.

## Revision note

Draft 3. Drafts 1 and 2 were each checked claim-by-claim against the source, then
attacked adversarially. That found two defects that would have shipped a feature which
does not work, and one prior art question the design had not asked:

- **The loan would have been unrecallable.** A borrowed job keeps counting toward the
  QOS aggregates, so it blocks its own team's legitimate jobs *at the admission gate* —
  and a gate-blocked job is dropped from the pending list, so it never reaches the
  scheduler and can never trigger reclaim. Exactly the failure the feature exists to
  prevent. §7.
- **Reclaim as designed was a denial-of-service vector.** Victim selection is a
  partition-level boolean that never checks whether an eviction helps the reclaimer.
  Combined with draft 2's bypass of the preemption gates, one permanently
  unschedulable job would destroy one borrowed job per second, forever. §8.1.
- **Something close to this already ships.** `docs/admin-guide/accounting.rst:942-978`
  documents a burst QOS pattern that does opportunistic borrowing with reclaim using
  only existing features. §2 argues what idle-fill adds; if that argument does not
  convince, the cheapest correct answer is to document the existing pattern better and
  build nothing.

§13 lists everything the earlier drafts got wrong. §9 is a register of every defect
found, with a fix for each — it is the working list for whoever implements this.

---

## 1. Problem

A group node cap exists to stop one team monopolizing a contested cluster. The cap is
enforced unconditionally, including when nothing is contested.

On a 24-node cluster where each team's QOS carries `grptres=node=4`: a team with four
nodes busy and a fifth job queued sees that job pend with `QOSGrpNodeLimit` while the
other twenty nodes sit idle. Nobody is competing for them. The cluster is enforcing a
fairness rule against a party of one.

The waste scales with how conservatively quotas are set, which makes the safest quota
configuration also the most wasteful one.

## 2. Why not the existing burst QOS pattern?

This question has to be answered first, because the shipped documentation already
describes opportunistic borrowing with reclaim (`accounting.rst:942-978`):

> A common design is to give users two pools: a *normal* pool with guaranteed capacity,
> and a *burst* pool they can use for extra work when the cluster has free nodes. Burst
> jobs run opportunistically and are kicked out as soon as a normal job needs the slot.

It is built from `preempt_type = "qos_priority"`, a `burst` QOS at priority `-5000`
with `preemptmode=requeue`, and `preempt=burst` on the normal QOS. It works today and
costs nothing to adopt.

Three things it does not do:

1. **It requires the user to know.** Overflow work only borrows if the user passes
   `--qos=burst`. A user who submits normally and hits the node cap still pends,
   which is the case in §1. Goal: no user action.
2. **It is not derived from the quota.** Burst is a separate pool with its own
   priority, not "the work that exceeded your share". A team must split its own
   pipeline across two QOSes and decide, per job, which is which.
3. **Borrowing is unbounded by quota rather than bounded by idleness.** A burst job
   runs whenever it outranks something, not only when capacity is genuinely spare.

So idle-fill's distinct claim is narrow and worth stating plainly: **it makes the
existing burst behavior automatic and quota-derived instead of opt-in and pool-derived.**
Everything else here is machinery to achieve that safely.

The intent is to build it, because the first point is the one that matters: a scheduler
that strands capacity unless users know a flag has not solved the problem in §1. But the
comparison deserves an explicit answer from reviewers rather than an assumption from the
author, so it is Q1.

## 3. Goals and non-goals

**Goals**

- An over-quota job runs when there is genuinely spare capacity.
- A job with a quota claim can take that capacity back.
- No behavior change for clusters that do not opt in.
- No new user action.

**Non-goals**

- Guaranteeing *which* nodes a team gets. Teams that own hardware want partitions.
- Account and association quotas (Q4).
- Partial dispatch. Placement is already all-or-nothing (`backfill.rs:577-580`).
- Heterogeneous jobs, burst-buffer jobs, and licensed jobs, which are excluded from
  borrowing (§9, D4/D5/D6).
- Suspend-based reclaim. Suspend does not free nodes (§8.4).

## 4. Two tiers

- **Legitimate** — within quota. Never evicted by this mechanism.
- **Borrowed** — running only because capacity was spare. Displaceable by a legitimate
  job that needs the capacity.

The contract is: *you may use spare capacity, and you will lose it on demand.* A loan
that cannot be recalled is not a weaker version of this feature; it is a worse one
(§12).

## 5. Placement: one pass, two tiers

### 5.1 The mechanism

Idle-fill appends the over-quota candidates to the **tail** of the priority-sorted
pending list that already goes to the single `schedule()` call
(`scheduler_loop.rs:191`), below every in-quota job.

This works because of how the pass walks the list. For each job in caller order
(`backfill.rs:434`) it either places the job now and reserves its nodes on the timeline
so later jobs see them busy (`backfill.rs:684-698`), or — when it cannot start now —
**reserves the future slot it would need** and moves on (`backfill.rs:718-726`). Later
jobs must fit around that reservation.

A tail job is therefore offered exactly the capacity left after every in-quota job has
taken what it can start on *and* reserved what it is waiting for. That is the
definition of spare capacity, computed by the code that already owns the question.

Verified: `schedule()` never re-sorts `pending` and never reads `job.priority` at all —
the priority sort lives entirely in the caller (`cluster.rs:3657-3664`). Tail position
is real precedence.

### 5.2 What the single pass buys

Draft 1 proposed a second `schedule()` call over a filtered set of idle nodes. That is
abandoned. Each row below is something the second call would have had to rebuild by
hand:

| Concern | Handled by the single pass |
|---|---|
| Capacity accounting | Timelines rebuild from `ClusterState` each call (`backfill.rs:377`) and update as jobs are placed. A second call is blind to the first call's assignments, so a *partially* allocated node would have its free remainder double-booked. |
| Backfill protection | Automatic, and **better than draft 1 proposed.** Draft 1 excluded every node holding a future reservation. The timeline instead lets a borrowed job use such a node when its time limit fits before the reserved start — capacity draft 1 would have stranded. |
| The published plan | `planned_starts()` reads the timelines and must be called immediately after `schedule()` (`backfill.rs:146-164`). A second call would collapse the plan `sinfo` shows to just the idle slice. |
| Reservations, k0s | Already applied by `find_suitable_nodes`. Note an idle node can *simultaneously* sit under an admin reservation (`node_match.rs:200-229`); filtering on node state alone, as draft 1 did, would have handed reserved nodes to arbitrary jobs. |
| Array concurrency | Enforced by the caller while building the list (`cluster.rs:3666-3677`). Two passes would have exceeded it. |
| Metrics, logging | One `schedule_time_us`, one set of unplaced-reason logs, no `last_outcome` churn. |

`Assignment` carries no tier information, but the caller appended the candidates, so it
tags results by job-ID membership. No signature change.

### 5.3 Where the append must happen

Strictly between the node-set construction and the `schedule()` call —
`scheduler_loop.rs:163` and `:191`. Not where the pending list is built.

`nodes_off_dispatch_cooldown(&pending)` derives the node set from the pending list: a
job with `--nodelist` pins its nodes back in past dispatch cooldown
(`cluster.rs:2745-2754`). Appending earlier would let a borrowed job's nodelist unpin a
node the cooldown was protecting, and an **in-quota** job could then be placed on it.
The `hit_depth_limit` metric at `:161` is similarly sensitive.

### 5.4 Consequences for the caller

An unplaced candidate is still in the list, so four things must be handled:

1. **It must not trigger preemption.** `try_preempt` receives everything that failed to
   schedule (`scheduler_loop.rs:220-238`). A borrowed job has no claim and must never
   evict anyone.
2. **It must keep reporting `QosGrpNodeLimit`.** It is over quota; that is the true
   reason. `update_pending_reasons` would otherwise relabel it `NoSuitableNodes`.
3. **It must not be forwarded to a federated peer** (`scheduler_loop.rs:243-250`).
   Exporting a job that is over its local quota is a policy nobody has chosen.
4. **It must not take a future-slot reservation.** A candidate that cannot start now
   should simply not start. Otherwise it publishes a `StartTime` and `SchedNodeList`
   through `planned_starts()` for a job with no claim — user-visible as an idle node
   "planned" for an over-quota job, alongside a `QOSGrpNodeLimit` reason. Nothing
   functional reads those maps (`server.rs:4726-4753`), so this is presentation only,
   but it is contradictory presentation and the reservation buys nothing: the only jobs
   behind a candidate are other candidates.

Truncation at `max_jobs_per_cycle` (`backfill.rs:432`, default 10000) is a prefix
operation, so candidates are dropped first and in-quota jobs are never displaced.
Idle-fill degrades to off under extreme queue depth, which is the right failure
direction.

## 6. Eligibility: the node cap must be the sole blocker

### 6.1 Why the pending reason cannot be the test

The obvious rule — "blocked with `PendingReason::QosGrpNodeLimit`" — is **wrong**.
`qos_resource_breach` returns only its *first* breach (`qos.rs:99`) and checks
`grp_tres` in cpu, node, mem, gpu order (`qos.rs:72-84`). A job over both its node cap
and its GPU cap reports the node one; lending it idle nodes would let it escape a cap on
a genuinely contended resource. The reverse error also bites: a job over its node cap
*and* its group CPU cap reports CPU, so a reason-based rule would refuse to consider it
forever.

### 6.2 The test

Re-run the full QOS limit check with the group node dimension lifted to `0`, which
`tres_cap_breach` skips, and treat the job as eligible only if it then passes. Exact,
not heuristic:

- Zeroing the cap cannot disturb the demand — `grp_node_charge` is a separate parameter
  (`qos.rs:204-213`).
- Every other QOS limit is re-evaluated, including the non-TRES ones: max running jobs
  per user, max submit jobs, group wall budget (`qos.rs:216-239`).
- Scoped to `grp_tres`; the per-job and per-user node caps are untouched, which an
  existing test already pins (`qos.rs:930`).

**Do not generalize "0 means ignore".** It holds for TRES only. For wall time `0` means
*block everything* (`qos.rs:45-47`), so zeroing a wall limit would invert it.

### 6.3 Why the earlier gates need no re-checking, and the two that do

`classify_pending_jobs` runs its gates in sequence, each dropping the job on failure:
begin-time, partition, dependency, reservation, priority sort, array concurrency,
accounting readability, then account and QOS, then licenses, then burst buffer
(`cluster.rs:3568-3780`). `account_block_with` runs first in the same closure and
returns before `qos_block_with` (`cluster.rs:3715-3741`).

So a job reaching the QOS gate has provably passed partition, dependency, reservation,
array, and account — including `AssocGrpNodeLimit`. The sole-blocker test does not need
to re-check them.

Licenses (`cluster.rs:3750`) and burst buffer (`cluster.rs:3774`) run *after* and are
never reached by a collected candidate. Both are handled by exclusion rather than
re-evaluation (§9, D5 and D6) because neither is a pure predicate.

The test must reuse the aggregates the gate already holds, including in-pass
`PassReservations` adjustments (`cluster.rs:7208-7215`). Re-deriving them elsewhere
evaluates a different cluster state and can disagree with the gate that produced the
verdict.

## 7. Quota accounting: borrowed nodes must not count

This is the defect that would have made the feature inert, and the most important
section in the document.

`sum_running_tres` counts every `Running` job matching its predicate, with no notion of
tiers (`cluster.rs:7449-7472`). A borrowed job is Running and is in the QOS, so from the
next cycle its nodes count toward `grp_tres[Node]`. It likewise increments
`user_running_count` for `max_jobs_per_user` (`qos.rs:217-221`) and consumes an
`array_max_concurrent` slot (`cluster.rs:3666-3691`).

The consequence is not merely inaccurate accounting. With `grptres=node=4`: four
legitimate nodes busy, a fifth job borrows an idle node, aggregate now reads 5. A
legitimate job finishes, so the team legitimately uses 3 and is entitled to a 4th. Job B
arrives wanting 1 node. At the gate `4 + 1 > 4`, so B is blocked — and
`retain_eligible` *removes* blocked jobs from the list (`cluster.rs:7099-7108`). B never
reaches `schedule()`, never lands in `unscheduled`, and **never reaches reclaim.** The
loan is permanently unrecallable, and B — a job with a genuine claim — is itself
demoted to a candidate, because it now passes the sole-blocker test.

The `max_jobs_per_user` variant is worse, because lifting the node dimension does not
rescue it: that limit is checked before the TRES breach, so B fails the sole-blocker
test too and is neither scheduled nor collected. It pends forever.

**The fix, and the principle:** borrowed capacity is outside the quota, so it must be
outside the aggregate that enforces the quota. `sum_running_tres`, `occupied_nodes`
(`cluster.rs:7589`), and the running/submitted counts in `qos_block_with` all need an
`idle_fill` exclusion when computing the QOS dimension.

That principle has a consequence worth surfacing: if borrowed nodes do not count,
borrowing is bounded only by idle capacity, not by any multiple of quota. One team can
borrow the entire idle cluster. Reclaim makes that recoverable rather than permanent, and
tail-priority ordering shares it out among borrowers, but there is no cap. Q3 asks
whether one is needed.

## 8. Reclaim

### 8.1 It cannot ride inside `try_preempt`

Draft 2 proposed modifying `try_preempt` to bypass the partition mode, the priority gap,
and the QOS allow list for a borrowed victim. That is unsafe, because victim selection
does not check that an eviction *helps*.

`preempt_overlaps_pending_nodes` (`scheduler_loop.rs:783-818`), for a job with no
explicit nodelist, returns true for any node the victim occupies that is in any of the
reclaimer's partitions. No GPU type, memory, feature, or topology check. And nothing
verifies the eviction closed the shortfall.

Today that is inert: the outer loop short-circuits on `preempt_mode == Off`
(`:697`) and the victim must clear the hardcoded 2x priority gap (`:705`). Removing both
turns it into a weapon. A job that can never be placed — `--gres=gpu:8` where the
largest node has 4, or a `--constraint` no node carries — is not rejected at submit;
`update_pending_reasons` only relabels it (`cluster.rs:4860-4878`) and it re-enters
`unscheduled` every cycle. At `interval_secs = 1` (`config.rs:626`) it would requeue one
borrowed job **per second, indefinitely**, needing no priority edge, no `preempt_mode`,
and no allow-list entry. Cost to the submitter: one job that never runs. The benign
version needs no malice at all — any in-quota job waiting on capacity no borrowed job
holds chews through unrelated borrowed jobs.

### 8.2 Reclaim is its own routine

Reclaim is a separate, self-contained step after placement, not a modification of
`try_preempt`:

1. For each in-quota job that the pass did not place, compute the nodes that would
   actually let it run, using the same `find_suitable_nodes` the scheduler uses.
2. Intersect with nodes held by borrowed jobs.
3. If the borrowed jobs on those nodes would free **enough** capacity to place the job,
   evict exactly that set. If not, evict nothing — a partial eviction destroys work
   without helping anyone.
4. Otherwise leave the job pending. It keeps its future-slot reservation, so it stays
   ahead of every borrowed job next cycle.

This fixes the denial-of-service directly: an unplaceable job never has a satisfiable
victim set, so it evicts nothing, forever. It also fixes the multi-node accumulation
problem draft 2 could only argue about analytically, because step 3 is atomic — the
reclaimer either gets its whole allocation or nothing changes.

It is also better isolated: one function, inputs (unplaced legitimate jobs, borrowed
jobs, nodes), output (a victim set), independently testable without a scheduler loop.

### 8.3 Eviction must actually free the node before it is reused

`JobPreemptRequeue` deallocates inside the Raft apply (`cluster.rs:5756-5763`) and only
then fires a fire-and-forget cancel (`scheduler_loop.rs:2267-2279`); the agent does
SIGTERM, waits 5 seconds, then SIGKILL (`agent_server.rs:4559-4574`). There is no
`Completing` handshake, which the rest of the codebase deliberately maintains —
`force_finish_completing_job` cancels on unreported nodes "so their agents release the
allocation before the controller frees those nodes" (`scheduler_loop.rs:2115-2117`).

At a 1-second cycle the reclaimer can therefore be dispatched onto a node whose victim
process still holds GPU memory and its cgroup, and the reclaim fails in exactly the case
the feature exists for. If the agent is unreachable the node is freed anyway and the
borrowed workload is orphaned on a node the controller believes is empty.

`cancel_job_on_nodes` already exists for this, awaiting every RPC under
`CANCEL_RPC_TIMEOUT` "so the caller can establish a happens-before ordering against
later actions" (`scheduler_loop.rs:2281-2296`). Reclaim uses it, and holds the freed
nodes out of the next cycle until the agent confirms.

### 8.4 Requeue is the fate; Suspend and Cancel are not

**Requeue.** It genuinely frees nodes, closes the accounting run, and returns the job to
`Pending` with its spec intact (`cluster.rs:5731-5745`). It ignores `spec.requeue` by
existing design (`cluster.rs:1871-1874`), so `--no-requeue` cannot pin borrowed
capacity. `preempt_requeue_count` is deliberately excluded from the `max_batch_requeue`
hold (`cluster.rs:5531-5536`), so a repeatedly evicted job is never cancelled.

**Suspend cannot work.** `JobSuspend` signals SIGSTOP but leaves `allocated_nodes` and
`allocated_resources` untouched (`cluster.rs:5946-5968`), so capacity is never released;
and nothing resumes the job, since the only resume path is the user-facing RPC
(`server.rs:1134-1139`). A suspended borrowed job holds its nodes forever.

**Cancel is gratuitous.** The job never had a claim; taking the run back is the price,
destroying the work is not.

### 8.5 The exempt window must be bounded, and thrash must back off

Two settings that look like protection are not sized for this question.

`preempt_exempt_time` resolves QOS, then the *most protective* matched partition, then
the cluster fallback (`scheduler_loop.rs:622-636`), and is unbounded. An operator with an
ordinary cluster-wide `preempt_exempt_time = 3600` makes every borrowed job
unreclaimable for an hour the moment they enable idle-fill — a job with a real quota
claim waits an hour for capacity that was lent away. A user can also raise their own
window with `--partition=fast,protected`, since the maximum across matched partitions
wins, and that needs no privilege. Reclaim therefore uses a **separate bounded
minimum-run window** for borrowed jobs rather than inheriting a knob sized for
arbitrating between two claim-holders.

The anti-thrash hold is `max(interval_secs * 2 + 3, 5)` (`cluster.rs:1875-1876`), which
at defaults is **5 seconds** — exactly the agent's SIGTERM-to-SIGKILL grace. A borrowed
job can be lent, evicted, and re-lent every five seconds indefinitely, each round
costing a Raft entry, an epilog, an accounting upsert, an agent RPC, and a log line,
while completing nothing. `preempt_requeue_count` is incremented but backs nothing off.
Reclaim backs off on it, following the existing `launch_backoff_secs` shape
(`cluster.rs:64-69`), turning unbounded churn into a converging series.

### 8.6 The `preempt_mode = "off"` question

The shipped documentation states the guarantee absolutely
(`configuration.rst:779-785`):

> `"off"` (default) — running jobs in this partition are never kicked out. […] The
> partition field is the on/off switch: preemption is only attempted at all when this is
> set to something other than `"off"`.

Reclaiming a borrowed job on such a partition contradicts that. Two honest options, and
this is Q2:

- **Reclaim regardless, and narrow the documented promise** to "jobs with a quota claim
  are never kicked out". Idle-fill stays self-contained: one switch, and it works. This
  is a **breaking change** in the sense `AGENTS.md` means — the PR needs the `!` marker
  and the docs edit ships with it.
- **Refuse to lend when reclaim is not permitted.** Respects the promise exactly, costs
  nothing to reason about, and means idle-fill silently does nothing on a
  default-configured cluster until the operator also enables preemption.

I lean to the first, because the second reintroduces the adoption cliff that §2 says is
the whole point of not using the burst pattern. But it is the operator's guarantee being
narrowed, so it is their call, not mine.

## 9. Defect register

Everything found by verification, with a fix. This is the implementation checklist.

| # | Severity | Defect | Fix |
|---|---|---|---|
| D1 | Blocker | Borrowed jobs count in QOS aggregates, blocking their own team's legitimate jobs at the gate where reclaim cannot see them (`cluster.rs:7449`, `:7099`) | Exclude `idle_fill` jobs from the QOS dimension of `sum_running_tres`, `occupied_nodes`, and the running/submitted counts. §7 |
| D2 | Blocker | Victim selection never checks an eviction helps, so an unplaceable job evicts one borrowed job per cycle forever (`scheduler_loop.rs:808-818`) | Reclaim is its own routine with an atomic satisfiable-victim-set test. §8.2 |
| D3 | Blocker | Eviction frees the node before the agent has killed anything; no `Completing` handshake (`cluster.rs:5756`, `scheduler_loop.rs:2267`) | Use the awaiting `cancel_job_on_nodes`; hold freed nodes out until confirmed. §8.3 |
| D4 | Blocker | Candidate collection loses the account gate's packing credit: clearing `preferred_nodes` un-enforces the reduced `grp_node_charge` the job was admitted on (`cluster.rs:7596-7601`) | Before collecting, re-check `account_block_with` with `grp_node_charge = num_nodes` (no credit). Only then is clearing sound |
| D5 | Blocker | A het component at the tail forces `HetGroupIncomplete` onto its in-quota siblings at the head (`backfill.rs:418-428`) | Never collect a job with `het_job_id` or `het_group` set |
| D6 | Serious | The burst-buffer gate is not a predicate — its success path drops the job and mutates staging state (`cluster.rs:3789-3798`) | Never collect a job with a burst-buffer requirement |
| D7 | Serious | Re-running the license predicate ignores in-pass contention, so a candidate can steal a license an in-quota job claimed this pass (`cluster.rs:3748-3766`) | Thread the post-loop `remaining` map out, or refuse to collect jobs requesting licenses |
| D8 | Serious | A collected candidate never calls `reserved.reserve(...)`, so its account and QOS cpu/mem/gpu charge vanishes from in-pass aggregates and later jobs are admitted against understated usage (`cluster.rs:3731-3743`) | Reserve on the collect path: account charge and QOS cpu/mem/gpu/count in full, node dimension deliberately 0 (per D1) |
| D9 | Serious | Eviction charges the partial run to fairshare, lowering the borrower's priority and making it the preferred next victim — a self-reinforcing loop (`db.rs:440-458`, `scheduler_loop.rs:677`) | Do not charge fairshare usage for a run ended by reclaim |
| D10 | Serious | `preempt_exempt_time` is unbounded and user-raisable via multi-partition submit; an ordinary cluster-wide hour makes borrowed capacity unreclaimable for an hour (`scheduler_loop.rs:631-635`) | A separate bounded minimum-run window for borrowed jobs. §8.5 |
| D11 | Serious | Anti-thrash hold is 5 seconds at defaults and nothing backs off (`cluster.rs:1875`) | Back off on `preempt_requeue_count`. §8.5 |
| D12 | Serious | Accounting cannot report which runs were borrowed, and the start-upsert leaves stale preemption provenance, so an evicted-then-completed job reads `COMPLETED, PreemptMode=Requeue` forever (`db.rs:280-297`) | Add `idle_fill` to the jobs table; add the three preempt columns to the upsert's `DO UPDATE SET` |
| D13 | Serious | The `idle_fill` flag is stamped once and never re-evaluated, so a job that becomes legitimate when a quota is raised stays evictable — contradicting §4 | Re-evaluate at reclaim time rather than trusting the stamp |
| D14 | Serious | Unbounded borrowed jobs make a node look busy for a year via `busy_until`, and a legitimate job then reserves future slots a year out on unrelated idle nodes (`scheduler_loop.rs:573`, `backfill.rs:233`) | Refuse to lend to a job with no effective time limit. Mandatory, not optional |
| D15 | Minor | Unplaced candidates take future-slot reservations and publish a `StartTime` for a job with no claim | Skip the `earliest > now` branch for candidates. §5.4 item 4 |
| D16 | Minor | Append point matters: appending before the node-set build lets a candidate's nodelist unpin a cooling node for an in-quota job (`cluster.rs:2745`) | Append between `scheduler_loop.rs:163` and `:191`. §5.3 |
| D17 | Minor | Double-subtract on a partially completed multi-node victim: the requeue apply passes `already_deallocated = &[]` after clearing `node_completions` (`cluster.rs:5761` vs `:5571`) | Collect completions first, as `evict_job_locked` does |

D14 resolves what draft 2 left as an open question. It is a correctness requirement, not
a judgment call.

## 10. Compatibility

- **`Job` gains `idle_fill`.** `Job` is serialized whole into `ClusterSnapshot`
  (`cluster.rs:6897`) and snapshot restore hard-fails on a deserialize error rather than
  skipping (`raft.rs:192-193`), so the field **must** be `#[serde(default)]`.
  `ClusterSnapshot.jobs` is the only persisted path reaching `Job` — `JobSubmit` carries
  `Box<JobSpec>`, not `Job` (`wal.rs:24-26`).
- **`WalOperation::JobStart` gains the same field**, also `#[serde(default)]`, plus the
  one-line copy in the apply handler (`cluster.rs:6010-6036`) without which the flag
  never lands. Not redundant with the `Job` field: no WAL operation sets arbitrary job
  fields, so the value must travel through `JobStart` for followers to converge, while
  the `Job` field is what survives snapshot and failover. `srun_step_dispatch`
  (`wal.rs:47-60`) is the exact precedent.
- **Rollback**: nothing uses `deny_unknown_fields`, so downgrading to a prior binary
  silently drops the flag and every running borrowed job becomes unmarked and
  unreclaimable. Operators need to drain borrowed jobs before a downgrade.
- **Accounting**: two additive columns (D12). There is no migrations directory — schema
  changes are `ALTER TABLE ... ADD COLUMN IF NOT EXISTS` lines in `db::migrate()`
  (`db.rs:149-167`).
- **Config**: `idle_fill_enabled` needs both a `#[serde(default)]` field and an entry in
  the hand-written `Default` impl (`config.rs:700`) or it will not compile. A near-empty
  config parse test already exists (`config.rs:2554`).
- **Breaking**: if Q2 resolves toward reclaiming regardless of `preempt_mode`, the
  documented meaning of `preempt_mode = "off"` narrows, and the PR carries `!`.

## 11. Work breakdown

| # | Layer | What |
|---|---|---|
| 1 | `spur-core/src/qos.rs` | Sole-blocker predicate and tests (§6.2) |
| 2 | `spur-core/src/config.rs` | `idle_fill_enabled`, field and `Default` entry |
| 3 | `spur-core/src/job.rs`, `wal.rs` | `idle_fill` on `Job` and `JobStart`, apply-handler copy, frozen-payload test following `wal.rs:750-769` |
| 4 | `spurctld/src/cluster.rs` | Exclude borrowed jobs from QOS aggregates (D1) — do this before anything reads the flag |
| 5 | `spurctld/src/cluster.rs` | Collect candidates: exclusions D5/D6/D7, credit re-check D4, `preferred_nodes` clear, `reserved.reserve` D8 |
| 6 | `spurctld/src/scheduler_loop.rs` | Append at the right point (D16); tag assignments; the four exclusions in §5.4; refuse unbounded jobs (D14) |
| 7 | `spur-sched/src/backfill.rs` | Suppress future-slot reservations for candidates (D15) |
| 8 | `spurctld/src/scheduler_loop.rs` | Reclaim routine (§8.2), awaiting cancel (D3), bounded window (D10), backoff (D11), flag re-evaluation (D13) |
| 9 | Fairshare | Exclude reclaimed runs from usage (D9) |
| 10 | `db.rs`, `proto`, `server.rs`, `rest/convert.rs`, `spur-cli` | Surfacing and accounting (D12, §11.1) |
| 11 | `docs/admin-guide/` | New page; edits to `configuration.rst` (§8.6) and `accounting.rst` including how this relates to the burst pattern (§2); `monitoring-jobs.rst` |
| 12 | `spur-metrics` | Nodes-on-loan gauge, reclaim counter |

Steps 1-3 are inert. The mechanism becomes live at step 6, and **step 4 must land before
step 6** or the loan is unrecallable.

### 11.1 Surfacing

`JobInfo`'s highest tag is 79 (`proto/slurm.proto:297`) with no reserved range, so 80 is
safe. Three separate touch-points: `job_to_proto` in `server.rs`; `job_to_json` in
`rest/convert.rs`, which is **hand-written from `Job`, not generated from the proto**;
and a `squeue` `%`-letter registered in *both* `squeue_header`
(`format_engine.rs:381`) and `resolve_job_field` (`squeue.rs:196`), since
`every_header_letter_resolves` (`squeue.rs:934`) fails if only one is done.
`SQUEUE_DEFAULT_FORMAT` stays untouched. Commit `513d319` is the end-to-end template.

## 12. Delivery

Steps 1-11 ship together.

Placement without reclaim is a **regression**, not a smaller improvement: it converts
"an idle node and a pending job" into "a node held by a job with no claim that cannot be
evicted". The reassuring fallback that borrowed jobs finish on their own does not hold —
`default_time_limit_minutes` is `0`, no limit is applied (`config.rs:690`,
`cluster.rs:8087`), and the enforcement loop skips jobs without one
(`scheduler_loop.rs:1927`).

Three things draft 2 wanted to defer also belong in the unit:

- **Observability** (step 10). A destructive mechanism you cannot observe is the same
  category of mistake as a loan you cannot recall, and the live plan in §14 cannot be
  executed without it — step 2 asserts a job "reports as borrowed".
- **The fairshare exemption** (step 9). D9's feedback loop, where an evicted borrower is
  charged for the lost run and becomes the preferred next victim, is live the moment
  reclaim is.
- **Docs** (step 11). Required by repo policy for user-facing change, and §8.6 narrows a
  documented guarantee, which cannot ship silently.

Only the metrics export (step 12) genuinely follows after. Default-off is what makes
landing this much at once safe: a zero-behavior-change deployment for every existing
cluster.

## 13. Corrections

### To `idle_fill.md`

| The proposal assumes | Reality |
|---|---|
| `idle_fill_exempt_secs` is a new knob | Exists as `preempt_exempt_time`, with a wider precedence chain — though §8.5 argues reclaim should not use it |
| `idle_fill_atomic`, default true, with a three-level override chain | Whole-job-or-nothing is already invariant (`backfill.rs:577`). `true` is a no-op; `false` would mean building partial dispatch |
| Fates are `REQUEUE`, `CANCEL`, `CHECKPOINT` | No checkpoint mode exists; the fourth is `Suspend` (`partition.rs:86`), which cannot free nodes. An unrecognized mode string parses **silently to `Off`** with no warning (`config.rs:2118`) |
| "No new configuration needed" for preemption | False. Under defaults preemption does not run at all, and the reclaim semantics described are blocked three ways |
| This is new capability | The burst QOS pattern already does opportunistic borrowing with reclaim (`accounting.rst:942`). §2 |

### To drafts 1 and 2 of this document

| Said | Correction |
|---|---|
| Pass 2 is a second `schedule()` call over a filtered node slice | Abandoned. One pass, tail append (§5) |
| An explicit idle budget must exclude nodes holding future reservations | Unnecessary and worse than the timeline's own reasoning (§5.2) |
| `preempt_exempt_time` precedence is cluster, partition, QOS | Backwards: QOS, partition, cluster (`scheduler_loop.rs:618`) |
| The gate consumes a precomputed single-pass TRES aggregate | False. `sum_running_tres` runs inside the gate, per candidate (`cluster.rs:7208`) |
| The priority gap is the widest obstacle to reclaim | The partition `preempt_mode` default of `Off` is, and it short-circuits first |
| One preemption per cycle | One per *pending job* per cycle; the `break` is on the inner loop (`scheduler_loop.rs:778`) |
| Reclaim bypasses the gates inside `try_preempt` | Unsafe — a denial-of-service vector (§8.1). Reclaim is its own routine |
| Reclaim honors `preempt_exempt_time` | It must use a bounded window instead (§8.5) |
| Whether to refuse unbounded jobs is an open question | It is a correctness requirement (D14) |
| Observability can ship later | It is in the delivery unit (§12) |

### Outside this feature

Three defects found while verifying the preemption path. None are caused by idle-fill and
all three affect operators today, so they are worth fixing independently rather than being
folded into this change.

**A suspend-preempted job is never resumed.** `configuration.rst:774-776` states that a
job preempted in `suspend` mode "keeps its node allocation and continues automatically
once the higher-priority job finishes". Nothing resumes it. The only `resume = true` call
is inside the user-facing `resume_job` RPC (`server.rs:1138`); the scheduler loop only
ever calls `send_suspend_to_agents(.., false)` (`scheduler_loop.rs:767`), and
`JobSuspend` leaves `allocated_nodes` intact (`cluster.rs:5946-5968`). So the job stays
`Suspended` until a human runs `scontrol resume`, holding its nodes throughout, and the
higher-priority job that triggered the preemption gains nothing. Either the
implementation is incomplete or the documentation overpromises; Slurm auto-resumes via
gang scheduling, which suggests the former.

**A typo in `preempt_mode` silently disables preemption.** The parse is a lowercase match
ending in `_ => PreemptMode::Off` (`config.rs:2118-2123`) with no warning and no error. An
operator writing `cancle`, or `checkpoint` out of Slurm habit, gets a cluster where
preemption never fires and nothing reports why. `parse_partition_time`, a few lines below,
deliberately fails loudly on a bad value, so this reads as an oversight rather than a
decision.

**A stale roadmap claim.** `plans/implementation-roadmap.md` says under "Gang Scheduling"
that the scheduler partially starts multi-node jobs when only some nodes are available.
Placement is all-or-nothing (`backfill.rs:577`).

## 14. Test plan

### Unit

- **Eligibility**: node cap is the sole blocker; job already fits; a second TRES
  dimension also breached, asserting the misleading reason code as an explicit
  precondition; a non-TRES QOS limit also blocking; no group cap; node dimension unset.
- **Aggregates (D1)**: a borrowed job does not consume its QOS's node quota; a
  legitimate sibling is admitted while a borrowed job runs; the `max_jobs_per_user`
  case.
- **Collection**: only when enabled; het, burst-buffer, licensed, and unbounded jobs are
  never collected; the account credit is re-checked; `preferred_nodes` cleared;
  `reserved.reserve` called.
- **Placement ordering**: an in-quota job outranks a candidate for the same node; a
  candidate takes a node nobody wants; it does not take a node reserved for an in-quota
  future start; it *does* when its time limit fits before that start; it takes no future
  reservation itself.
- **Loop wiring**: an unplaced candidate does not reach `try_preempt`, keeps
  `QosGrpNodeLimit`, is not federated.
- **Reclaim**: evicts a satisfiable victim set atomically; evicts **nothing** for an
  unplaceable job (D2 regression); does not evict a legitimate job; respects the bounded
  window; backs off on repeat; re-evaluates the flag (D13); victim returns to `Pending`
  with spec intact.
- **Persistence**: frozen `JobStart` payload still deserializes; flag round-trips through
  snapshot.

### Live cluster

1. `idle_fill_enabled = false`: an over-quota job still pends with `QOSGrpNodeLimit`.
   The regression guard matters more than the happy path.
2. Enabled, `grptres=node=1`: a second job starts on an idle node and reports as
   borrowed.
3. **The D1 test.** While the borrowed job runs, a legitimate job of the same QOS starts
   when quota frees. This is the defect that made drafts 1 and 2 non-functional.
4. An in-quota job needing the borrowed node causes a requeue and starts — verifying the
   process is actually gone before the reclaimer launches (D3), ideally with a GPU
   workload where a survivor is visible.
5. **The D2 test.** Submit an unplaceable job (`--gres` exceeding any node) with borrowed
   jobs running; confirm **nothing** is evicted, over several minutes.
6. Four borrowed single-node jobs plus an in-quota four-node job: it starts in one step,
   with no partial eviction.
7. Repeat (4) immediately: no eviction until the bounded window elapses.
8. Restart the controller with a borrowed job running; the flag survives.

Real commands and output go in the PR body.

## 15. Open questions

**Q1 — Does the automatic behavior justify the work, given the burst QOS pattern already
ships?** §2 argues idle-fill's value is being automatic and quota-derived rather than
opt-in and pool-derived, and that is the intended direction. The cost is §9: three
blockers and fourteen further defects, touching admission, placement, preemption,
accounting, and fairshare. Reviewers should confirm the trade rather than inherit it —
the honest fallback, if the answer is no, is to improve the burst-pattern documentation
instead.

**Q2 — Narrow the `preempt_mode = "off"` guarantee, or refuse to lend without it?**
§8.6. Option one keeps idle-fill self-contained and needs a `!` PR and a docs edit.
Option two respects the documented promise exactly and means idle-fill does nothing on a
default cluster. I lean to the first; it narrows an operator's guarantee, so it is their
call.

**Q3 — Should borrowing be capped?** Once borrowed nodes stop counting toward quota (D1,
and they must), borrowing is bounded only by idle capacity. One team can borrow every
idle node. Reclaim makes that recoverable and tail ordering shares it among borrowers, but
nothing caps it. Options: no cap; a multiple of quota; a cluster-wide fraction. My
instinct is no cap for a first cut, because a cap on spare capacity partly recreates the
problem, but I have low confidence.

**Q4 — Which cap do the teams asking for this actually hit?** The QOS group node cap, or
the association/account cap? The account gate runs first and returns early
(`cluster.rs:3715`), so account-blocked jobs never reach eligibility. Extending needs a
second predicate and a second collection point. This also feeds Q1.

**Q5 — Burst tier.** Does `idle_fill_preemptable` (marking a QOS always-borrowed) still
have a purpose once §2's comparison is on the table, or is it a third overlapping
mechanism alongside the burst pattern and idle-fill itself? My inclination is to drop it.

**Q6 — Fairshare treatment.** D9 proposes not charging usage for a reclaimed run.
Consistent with the loan model, but it means borrowed compute is free, which could
encourage deliberate over-quota submission. Charge it and accept the victim-selection
feedback loop, or exempt it and accept free compute?
