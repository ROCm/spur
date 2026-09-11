# Idle-Fill Scheduling

Let a job that has exhausted its QOS group *node* quota run on nodes that are
otherwise idle, and take those nodes back by preemption when a job with a genuine
quota claim needs them.

Everything sits behind one cluster switch, `scheduler.idle_fill_enabled`, defaulting
to off, so an existing cluster behaves exactly as it does today until an operator
opts in.

This document is the design under review. Two findings from the codebase changed its
shape relative to the original proposal:

- **The configuration surface halves, from four knobs to two.** Two of the proposed
  knobs describe mechanisms that already exist or behavior that is already invariant
  (§6).
- **Reclaim is not free reuse of the existing preemption.** The proposal assumes any
  quota-holder can call the loan in. The current `try_preempt` will not permit that
  under default configuration, for reasons unrelated to the QOS allow list (§5.5).
  This is the largest piece of new work and the part most worth challenging.

---

## 1. Problem

A group node cap exists to stop one team monopolizing a contested cluster. But the
cap is enforced unconditionally, including when nothing is contested.

On a 24-node cluster where each team's QOS carries `grptres=node=4`: a team with
four nodes busy and a fifth job queued sees that job pend with `QOSGrpNodeLimit`
while the other twenty nodes sit idle. Nobody is competing for them. The cluster is
enforcing a fairness rule against a party of one.

The waste scales with how conservatively quotas are set, which makes the safest
quota configuration also the most wasteful one.

## 2. Goals and non-goals

**Goals**

- An over-quota job runs when there is genuinely spare capacity.
- A job with a quota claim can always take that capacity back.
- No behavior change for clusters that do not opt in.
- No new user action: users do not opt in, mark jobs, or learn a flag.

**Non-goals**

- Guaranteeing *which* nodes a team gets. This is capacity-based, not
  ownership-based; teams that own specific hardware want partitions.
- Account and association quotas. Only the QOS group node cap is in scope for a
  first cut (§11, Q3).
- Partial dispatch. Running part of a multi-node job is not supported today and this
  does not add it (§6).
- A checkpoint-on-eviction mode. It does not exist in Spur (§6).

## 3. What an operator and a user see

An operator sets `idle_fill_enabled = true` under `[scheduler]`. Nothing else is
required, though `preempt_mode` must be non-`Off` somewhere for reclaim to work,
which is a real constraint rather than a footnote (§5.5).

A user submits as normal. A job that would previously have pended with
`QOSGrpNodeLimit` may now start, and may later be evicted. Its fate on eviction
follows the existing `PreemptMode`, so it is the same fate any preempted job gets on
that cluster.

The one genuinely new user-visible thing is that a job can be evicted for a reason
that is not "someone with higher priority arrived" but "you were never entitled to
this node". Surfacing that clearly matters, and is Q5.

## 4. Two tiers

- **Legitimate** — within quota. Never preempted by this mechanism.
- **Opportunistic** — running only because capacity was spare. Displaceable by any
  legitimate job.

The contract is: *you may use spare capacity, and you will lose it on demand.* Both
halves matter. A loan that cannot be recalled is not a weaker version of this
feature; it is a different and worse one (§10).

## 5. Design

### 5.1 Two scheduling passes, one cycle

Both passes run inside a single scheduling cycle.

**Pass 1** is today's scheduling, completely unchanged. All within-quota jobs are
evaluated in priority order with normal limit enforcement.

**Pass 2** places over-quota jobs on whatever pass 1 left empty.

The implementation choice that keeps this small: **pass 2 is a second `schedule()`
call over a filtered node slice.** `ClusterState::nodes` is a plain `&[Node]`, so
handing the scheduler only the idle nodes reuses the entire placement engine — GPU
matching, fabric topology, `cons_tres` packing, per-node capacity — without touching
the backfill scheduler at all. Whole-job atomicity and priority ordering come along
for free, because the existing engine already provides both.

These are two passes within one cycle, not two phases of delivery. The document uses
*pass* throughout to keep them distinct.

### 5.2 Eligibility: the node cap must be the *sole* blocker

This is the subtle part, and getting it wrong is a correctness bug rather than a
missing feature.

The obvious rule is "the job was blocked with `PendingReason::QosGrpNodeLimit`". That
rule is **wrong**. The resource check returns only its *first* breach, and evaluates
`grp_tres` in the order cpu, node, mem, gpu. A job over both its node cap and its
GPU cap therefore reports the node one. Lending it idle nodes would let it escape a
cap on a genuinely contended resource — exactly what quotas exist to prevent.

Instead: re-run the whole limit check with the node dimension lifted, and treat the
job as eligible only if it then passes. Lifting means setting that dimension to `0`,
which the checker ignores rather than treating as a block.

This is exact rather than heuristic, and it also handles the reverse case: a job over
both its node cap and its group *CPU* cap reports CPU, and a reason-based rule would
have wrongly excluded it from ever being considered.

Eligibility is decided where the running aggregates already exist, not in the gate
that consumes the verdict. Recomputing them in the gate would re-derive every
running-job aggregate, undoing the single-pass aggregation the gate was built
around. It is also gated on the config switch, so a cluster with idle-fill off pays
nothing for the extra evaluation.

### 5.3 The idle budget

Idle nodes are those with no running jobs, **minus** three sets:

1. Nodes pass 1 just claimed this cycle.
2. Nodes that are idle now but hold a **future reservation for a legitimate job**.
   The scheduler already reports these — `planned_starts()` returns exactly the nodes
   that are empty now but reserved for a pending job's future slot. Missing this
   would make idle-fill delay the large job that backfill was deliberately
   protecting, reintroducing the starvation problem backfill exists to solve.
3. Nodes under an admin reservation, or reserved for k0s.

On (3): a reserved-but-empty node is excluded because an idle-fill job carries no
promise to finish before the reservation begins. A future refinement could allow it
when the job's time limit provably fits the gap; the machinery for that reasoning
exists, but it is not needed for a first cut.

Node state alone is not a sufficient test — a node can be idle *and* reserved.

### 5.4 Reclaim, and why it is not free

The proposal says a legitimate job can reclaim nodes from opportunistic jobs
"regardless of allow list configuration". The existing preemption path will not do
that. Three separate gates stand in the way:

| Gate | Effect |
|---|---|
| Priority gap | A victim is skipped unless its priority is *less than half* the pending job's, so a legitimate job of comparable priority cannot reclaim its own quota. **This applies under default configuration**, which makes it the widest hole. |
| QOS allow list | When `preempt_type = qos_priority`, the pending job's QOS must list the victim's QOS. A cluster with QOS-based node quotas is precisely the cluster likely to have this set. |
| `PreemptMode` | Must be non-`Off` on the victim's partition or QOS, or nothing is evicted at all. |

Reclaim therefore needs a deliberate opportunistic-victim path: when the victim ran
as idle-fill, neither the priority gap nor the allow list should apply, because an
opportunistic job has no claim to weigh against. The proposal's instinct is right —
the allow list answers "who wins between two jobs that both have a claim", a
different question — but the code does not implement that distinction yet.

`PreemptMode = Off` is different in kind and should be **honored**: if an operator
has disabled preemption, we must not lend capacity we cannot take back (Q1).

Two mechanical details worth knowing: eviction happens **one job per cycle**, so
freeing a multi-node idle-fill job takes several cycles; and the minimum-run-time
protection the proposal asks for already exists as `preempt_exempt_time`, so
idle-fill reuses it rather than adding a parallel knob.

Victim ordering within the opportunistic tier is lowest-priority-first, which is what
the existing code already does.

### 5.5 Configuration

| Knob | Level | Default | Purpose |
|---|---|---|---|
| `scheduler.idle_fill_enabled` | cluster only | `false` | Master switch |
| `idle_fill_preemptable` | per-QOS | `false` | Marks a QOS as always-opportunistic (burst), so it joins pass 2 even while within quota |

`idle_fill_enabled` is cluster-only by design. An over-quota job is by definition
escaping a per-QOS limit, so a per-QOS opt-in would let a QOS grant itself the
exemption — the knob would undermine the thing it configures.

Minimum run time reuses `preempt_exempt_time`. Eviction fate reuses `PreemptMode`.

## 6. Corrections to the original proposal

All verified against the source.

| The proposal assumes | Reality |
|---|---|
| `idle_fill_exempt_secs` is a new knob | Already exists as `preempt_exempt_time`, with the same cluster to QOS precedence the proposal describes, plus a partition level it omits. A second knob would give operators two controls for one behavior. |
| `idle_fill_atomic` needs a cluster/partition/QOS override chain, default true | Whole-job-or-nothing is **already invariant**: placement refuses to assign when fewer nodes are placeable than requested. `true` is a no-op, and `false` would mean *building* partial dispatch, which does not exist. A three-level override chain for a constant is pure cost. |
| Eviction fates are `REQUEUE`, `CANCEL`, `CHECKPOINT` | There is no checkpoint mode in Spur; the fourth mode is `Suspend`. Worse, unrecognized mode strings parse silently to `Off`, so an operator writing `checkpoint` would get *no preemption* rather than an error. |
| "No new configuration needed" for preemption | False twice over: preemption is opt-in via `PreemptMode`, and the gates in §5.4 block the reclaim semantics described. |

Two of the four proposed knobs therefore disappear.

Separately, and outside this feature: `plans/implementation-roadmap.md` claims under
"Gang Scheduling" that the scheduler partially starts multi-node jobs when only some
nodes are available. That is stale — placement is already all-or-nothing — and is
worth correcting so the next reader does not plan around it.

## 7. Compatibility

No breaking change. Specifics:

- **`Job` gains an `idle_fill` field.** `Job` is serialized into Raft snapshots, so
  the field **must** carry `#[serde(default)]` or a new controller crashes replaying
  an old snapshot. It must genuinely persist rather than be skipped, because the
  exempt window has to survive a restart or leader failover.
- **`WalOperation::JobStart` gains a field** recording the decision durably at
  dispatch, also `#[serde(default)]`, with a frozen-payload test matching the
  existing discipline for that enum.
- **QOS records are PostgreSQL-backed, not in the Raft log**, so
  `idle_fill_preemptable` carries no replay risk. Migrations are additive only.
- **Config**: a new defaulted field keeps deployed `spur.conf` files loading.
- **Proto, CLI, REST**: additive only. No tag renumbering, no flag removed, no field
  changing meaning.

## 8. Work breakdown

Ordered so each step compiles on the previous one.

| # | Layer | What |
|---|---|---|
| 1 | `spur-core` QOS | Sole-blocker predicate and unit tests |
| 2 | `spur-core` config | `idle_fill_enabled` and its default |
| 3 | `spurctld` gate | Report eligibility out of the QOS gate; collect candidates without changing pass-1 behavior |
| 4 | `spurctld` scheduler loop | Idle budget: the three exclusions in §5.3 |
| 5 | `spurctld` scheduler loop | Pass 2: second `schedule()` over the idle slice, dispatched through the existing path |
| 6 | `spur-core` job and WAL | `idle_fill` flag, `JobStart` field, frozen fixture |
| 7 | `spurctld` scheduler loop | Opportunistic-victim path (§5.4) |
| 8 | `spurctld` | Refuse to lend when the loan cannot be recalled (Q1) |
| 9 | QOS record, DB, gRPC, `sacctmgr` | `idle_fill_preemptable` burst tier |
| 10 | proto, `squeue`, REST | Show that a job is running on loan |
| 11 | `docs/admin-guide/` | A dedicated page, cross-linked from the config and accounting references |

Steps 1 to 3 are inert on their own: nothing reads the candidate list, so the cluster
behaves identically. That makes them safe to write first but **not** meaningful to
ship first (§10).

One trap found while prototyping step 3: the collected candidate must have its
`preferred_nodes` cleared. The account gate runs first and, on success, credits nodes
the account *already occupies* — nodes that have running jobs and are therefore never
idle. Left in place, that credit is interpreted as a nodelist and, once it covers the
job's node count, restricts placement to exactly those non-idle nodes. Pass 2 would
then try to place idle-fill jobs on the one set of nodes guaranteed not to be idle.

## 9. Test plan

### Unit

- **Eligibility**: sole blocker; job already fits; a second dimension also breached
  (the §5.2 trap, asserting the misleading reason code as an explicit precondition);
  a non-TRES limit also blocking; no group cap; node dimension unset.
- **Pass 2 placement**: an over-quota job fills idle nodes; budget exhaustion leaves
  the remainder pending; higher priority takes first pick; a job larger than the
  budget does not partially start; a node held for a future legitimate job is not
  lent out.
- **Gate**: candidates collected only when enabled; pass-1 reasons unchanged;
  `preferred_nodes` cleared.
- **Reclaim**: a legitimate job evicts an opportunistic one at equal priority; an
  opportunistic job cannot evict a legitimate one; the exempt window is respected;
  lending is refused when preemption is off.

All in-process, no external services, no timeouts.

### Live cluster

Unit tests do not substitute for this. On a real multi-node cluster, built from the
branch being proposed:

1. With `idle_fill_enabled = false`, an over-quota job still pends with
   `QOSGrpNodeLimit` and nothing else changes. This regression guard matters more
   than the happy path.
2. Enabled, with `grptres=node=1`: one job fills the quota, a second starts on an
   idle node.
3. An in-quota job needing that node evicts the idle-fill job per `PreemptMode` and
   starts.
4. Repeating (3) immediately after (2) does *not* evict, until `preempt_exempt_time`
   has elapsed.
5. Restarting the controller with an idle-fill job running preserves the flag.
6. A node held for a future legitimate start is not lent out.

## 10. Delivery: the loan and the recall ship together

A tempting split is pass-2 scheduling first, reclaim second. That is wrong.

Without reclaim, idle-fill is a **regression**, not a smaller improvement. It turns
"an idle node and a pending job" into "a node held by a job with no claim to it that
cannot be evicted". The fallback — that idle-fill jobs simply run to completion where
preemption is unconfigured — holds only for **bounded** jobs. The cluster-wide default
time limit is `0`, meaning unbounded, so a job submitted without `-t` can hold
borrowed nodes indefinitely, and the team whose quota it borrowed has no recourse.

The unit of delivery is therefore the complete mechanism: pass 2 **and** reclaim,
behind the default-off switch. Default-off is what makes landing both at once safe —
it is a zero-behavior-change deployment for every existing cluster.

The burst tier (step 9) and the observability and docs surfaces (steps 10 and 11) are
genuinely separable and can follow, because none of them change the core contract.

## 11. Open questions

**Q1 — Lending what we cannot take back.** Should idle-fill refuse a job with no
effective time limit, and refuse entirely when the relevant `PreemptMode` is `Off`?
Both, in my view: each is a cheap check, and together they make the promise honest.
Without them the feature has a failure mode strictly worse than today's waste.

**Q2 — How far should reclaim override the existing gates?** Relax both the priority
gap and the QOS allow list for idle-fill victims (implementing the proposal as
written); relax only the allow list, so reclaim still needs a priority edge; or change
nothing and accept that reclaim works only where configuration happens to permit. I
favor relaxing both, since an opportunistic job has no claim to weigh. But that means
a *low*-priority legitimate job can evict a *high*-priority idle-fill job — correct
under the loan model, surprising under a pure-priority model — so it deserves an
explicit decision rather than an assumption.

**Q3 — Scope.** QOS group node cap only, or the association and account caps too? The
account gate runs before the QOS gate and returns early, so account-blocked jobs never
reach the idle-fill decision as designed. Which cap do the teams asking for this
actually hit?

**Q4 — Fate of an evicted idle-fill job.** Fate follows the existing `PreemptMode`, so
a cluster set to `Cancel` destroys work that only ran opportunistically. `Requeue`
seems kinder for this tier specifically. Is a per-tier override worth the complexity,
or is documented guidance enough?

**Q5 — Observability.** Is a `squeue` indicator enough, or do operators need counters
for nodes currently on loan and idle-fill evictions? The latter implies a new metrics
export.

**Q6 — Burst timing.** Should `idle_fill_preemptable` land with the core mechanism or
after? Deferring keeps the first change focused on one question: can an over-quota job
safely borrow idle capacity?
