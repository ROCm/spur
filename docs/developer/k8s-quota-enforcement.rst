Quota Enforcement for SPUR-Managed Kubernetes (Design)
======================================================

.. note::

   This is a design document for a planned capability, not yet implemented. It
   describes how SPUR would enforce its resource model on workloads running in a
   SPUR-managed k0s cluster. Feedback welcome.

Goal
----

Let SPUR **seamlessly manage cluster resources** (accounts, QoS, fair-share,
partitions, GRES/GPU limits) while the k0s cluster remains **a completely normal
Kubernetes deployment** to its users. A user with a kubeconfig runs ``kubectl``
against vanilla k8s objects and never touches a SPUR CLI; SPUR is the *invisible
policy plane* that makes those workloads obey SPUR's resource model.

Principles
----------

1. **SPUR is the source of truth + reconciler.** SPUR's account/partition/QoS
   model is authoritative; a controller projects it onto k8s objects and corrects
   drift (GitOps-style).
2. **k8s enforces natively.** Enforcement rides on standard primitives
   (Namespace, ResourceQuota, LimitRange, RBAC, PriorityClass, node
   labels/taints, and — for batch fairness — Kueue). No proprietary API sits in a
   user's path.
3. **SPUR observes usage back.** A watch controller feeds real k8s consumption
   into SPUR's accounting/fair-share, so SpurJobs and raw pods draw on **one**
   unified quota/fair-share view.
4. **No hard dependency in the hot path.** Pod creation must not block on a SPUR
   RPC; enforcement is declarative (admission/quota) or asynchronous (queueing),
   not a synchronous call to spurctld.

Architecture
------------

.. code-block:: text

                   SPUR control plane (source of truth)
      accounts · associations · QoS · partitions · GRES · fair-share
                             │
                 ┌───────────┴────────────┐
                 ▼                         ▲
      (1) Policy Reconciler        (3) Usage Watcher
      projects SPUR model →        watches k8s pods/metrics →
      native k8s objects           feeds accounting + fair-share
                 │                         ▲
                 ▼                         │
      ┌────────────────────── k0s (vanilla k8s) ──────────────────────┐
      │ Namespace/ResourceQuota/LimitRange/RBAC/PriorityClass/labels   │
      │ [Kueue ClusterQueue/LocalQueue/ResourceFlavor]  kube-scheduler │
      └────────────────────────────────────────────────────────────────┘
                 ▲
                 │ scoped kubeconfig (SA token, namespace-bound RBAC)
             user `kubectl`  (thinks it's a regular cluster)

Concept mapping (SPUR → native k8s)
-----------------------------------

.. list-table::
   :header-rows: 1
   :widths: 22 38 40

   * - SPUR
     - Native k8s enforcement
     - Notes
   * - Account / association
     - **Namespace** + **RBAC**
     - tenancy + who-can-submit-where
   * - Account/QoS resource cap
     - **ResourceQuota**
     - CPU, memory, ``amd.com/gpu``, GRES as extended resources, pod/PVC counts
   * - Default/min/max requests
     - **LimitRange**
     - stops request-gaming (unset requests get defaulted)
   * - QoS priority
     - **PriorityClass**
     - native priority + kube-scheduler preemption
   * - Partition (node group)
     - **node labels + taints**; per-ns default ``nodeSelector`` (PodNodeSelector admission)
     - pods land only on the account's partition
   * - GRES type (``gpu:mi300x:N``)
     - **extended resource** in quota + ``ResourceFlavor``
     - typed device accounting
   * - Time limit
     - ``activeDeadlineSeconds`` (injected) / Kueue ``maximumExecutionTime``
     -
   * - Fair-share / borrowing / preemption
     - **pluggable backend**: Kueue ``ClusterQueue`` + cohort (default) *or* SPUR-native gating
     - the one thing ResourceQuota can't do; both impls behind one interface
   * - User identity
     - **ServiceAccount token** (or OIDC) scoped by RBAC
     - ``spur k8s kubeconfig --user``
   * - Accounting/usage
     - SPUR usage watcher ← k8s metrics/pod lifecycle
     - closes the fair-share loop

Layered plan
------------

**Layer 0 — Cluster ownership.** Done: SPUR provisions/owns k0s.

**Layer 1 — Tenancy + hard quotas (native, no SPUR in the hot path).**
A reconciler creates, per account, a Namespace + ResourceQuota + LimitRange +
RBAC, and ``spur k8s kubeconfig --user <u>`` mints a namespace-scoped
ServiceAccount kubeconfig. Native admission enforces hard caps + isolation
immediately. Covers "SPUR quotas enforced" for the static dimensions.

**Layer 2 — Partitions + priority.** Reconcile node labels/taints from SPUR
partitions; set a default ``nodeSelector`` per namespace (PodNodeSelector
admission plugin, built into the API server) so an account's pods stick to its
partition; map QoS priority → PriorityClass.

**Layer 3 — Usage feedback.** A watch controller mirrors running pods
(per-namespace resource use over time) back into SPUR accounting/fair-share so the
two worlds share one view.

**Layer 4 — Dynamic fairness / queueing.** The part ResourceQuota can't express.
Built as a **pluggable** ``FairnessBackend`` **interface with two
implementations**, selected by config (``[cluster] fairness = "kueue" | "native"``):

- **kueue (default, recommended):** SPUR reconciles Kueue ``ClusterQueue`` /
  cohort / ``ResourceFlavor`` / ``LocalQueue`` objects from
  accounts/partitions/QoS; Kueue suspends/admits Workloads with borrowing +
  preemption + fair sharing. The k8s-native batch-quota system; least custom code.
- **native:** no extra cluster component — SPUR adjusts ResourceQuota hard caps on
  a fair-share loop and gates pods with ``schedulingGates``, releasing them when
  the account is within its fair share.

Both back the same internal interface (``admit(account, workload) -> Decision``,
``reconcile(accounts, partitions)``), so the rest of the system is
engine-agnostic. **Tests for both** backends are part of the milestone (unit
tests over the mapping/decision logic + an end-to-end run per backend on a real
cluster).

**Layer 5 — Polish.** Time-limit injection, licenses/reservations (map to a CRD
or a semaphore), multi-tenant network policy, dashboards, ``spur k8s quota``
visibility commands.

Where it lives (code)
---------------------

- The **spur-k8s operator** grows a *policy reconciler* controller (it already
  watches ``SpurJob`` + creates pods, so it has kube-client plumbing + namespace
  logic to build on).
- **spurctld** exposes account/partition/QoS *reconcile intent* (it already holds
  associations/QoS/partitions + a Raft state machine + accounting DB).
- **spur-cli** gains ``spur k8s kubeconfig --user <u>`` and ``spur k8s quota``
  (show the projected quotas).
- A **usage watcher** — a controller (in the operator) that reconciles k8s usage
  → spurctld accounting.

Milestones (implementable increments)
-------------------------------------

1. **M1** — Namespace + ResourceQuota + LimitRange + RBAC reconciler from SPUR
   accounts; ``spur k8s kubeconfig --user``. *(Hard quotas + isolation land here.)*
2. **M2** — Partition node-labels + per-namespace nodeSelector + PriorityClass
   from QoS.
3. **M3** — Usage watcher → accounting/fair-share.
4. **M4** — Fairness backend: the ``FairnessBackend`` interface + **both** the
   Kueue-driven impl (default) and the SPUR-native-gating impl, selected by
   ``[cluster] fairness``, with tests for each.
5. **M5** — Time-limits, licenses/reservations, ``spur k8s quota``, NetworkPolicy.

Each milestone is independently shippable and testable on a real cluster.

Alternatives considered
-----------------------

- **Synchronous admission webhook doing all enforcement.** Rejected as the
  *primary* mechanism: puts a SPUR RPC in the pod-creation hot path (availability
  + latency risk, fail-open/closed dilemmas) and duplicates what ResourceQuota
  already does natively. Kept as a *narrow* tool (or a native
  ``ValidatingAdmissionPolicy``/CEL) only for declarative rules quota can't
  express.
- **SPUR replaces the kube-scheduler** (scheduler plugin/extender for all pods).
  Rejected as primary: heavy, and it makes the cluster *not* behave like regular
  k8s (surprising placement, preemption semantics). Reasonable as a far-future
  option; Kueue gets most of the benefit natively first.
- **Proprietary SPUR API / force all workloads through the Slurm CLI.** Rejected:
  directly violates "true to k8s" — users must be able to ``kubectl`` normally.
- **One shared namespace with labels instead of namespace-per-account.**
  Rejected: ResourceQuota, RBAC, and default-nodeSelector are all *namespace*
  scoped, so a single namespace can't isolate tenants or cap them independently.
- **A bespoke fair-share/queue engine in SPUR for k8s.** Rejected in favor of
  driving **Kueue** (the CNCF-native batch-quota system) — less custom code, and
  it *is* the standard k8s way — unless we specifically want no extra components
  (then the native-gating fallback).

M4 resolution — SPUR scheduling model → Kueue / native
------------------------------------------------------

Grounded in the actual spurctld code (not the Slurm ideal). The key realization:
**SPUR enforces far less than its structs imply**, so the mapping is small and,
for GPUs, an *improvement* over SPUR's current behavior.

Ground truth — what SPUR actually enforces
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

.. list-table::
   :header-rows: 1
   :widths: 45 55

   * - SPUR mechanism
     - Status in code
   * - **QoS limits** (per-user + QoS-wide **CPU/Node/Mem** TRES,
       ``max_jobs``/``max_submit`` per user, ``max_wall``)
     - **Enforced** — the *only* hard gate; evaluated at *schedule* time (job
       parks Pending-with-reason, not rejected at submit)
   * - **Fair-share** (decayed CPU-usage vs target-share, ×10 cap)
     - **Enforced** as *soft priority* only; **CPU-seconds only**, **flat** (no
       account tree), per-user shares dead
   * - **Priority** = ``max(1, base × min(fairshare,10) × age(1–2×@7d) × partition_tier)``
     - Enforced; **multiplicative**, not Slurm weighted-sum; base 0 → collapses to
       1 (a bug)
   * - **Preemption**
     - Partition ``preempt_mode`` only; victim.pri < challenger.pri/2 + node
       overlap. **QoS preempt_mode is dead**
   * - **Partition** ACL (allow/deny **accounts**), node-count/time/state,
       ``priority_tier``
     - Enforced (ACL at submit; limits park-as-pending). ``allow_groups`` /
       ``allow_qos`` / ``default_time`` / ``exclusive_user`` are **inert**
   * - **GPU** quota / GPU fair-share
     - **Never enforced** — ``TresType::Gpu`` ignored everywhere; ``gpu_seconds``
       never written
   * - Account & association limits (``max_running_jobs``, ``grp_tres``, …)
     - **Schema-only, unenforced**
   * - ``Qos.priority``, ``Qos.preempt_mode``, ``Qos.usage_factor``
     - Parsed/served but **never applied**

So there are really only **three** enforced dimensions to honor: QoS resource caps
(CPU/Node/Mem), QoS job/submit counts + wall, and fair-share ordering. Everything
else is either inert or schema-only — which we should *not* port bug-for-bug.

Mapping
~~~~~~~

.. list-table::
   :header-rows: 1
   :widths: 24 26 26 24

   * - SPUR (enforced)
     - Kueue backend
     - Native backend
     - Fidelity
   * - Account = tenancy
     - Namespace + **LocalQueue → ClusterQueue**
     - Namespace + **ResourceQuota**
     - exact
   * - Account allocation / share
     - ``ClusterQueue.nominalQuota``; org = **cohort** w/ ``fairSharing``
       (borrow/reclaim)
     - ResourceQuota caps, SPUR loop resizes
     - Kueue adds real, *hierarchical* fair borrowing SPUR lacks
   * - QoS grp/max **CPU/Mem** caps
     - resources on the ClusterQueue (or a per-QoS ClusterQueue in the cohort)
     - ``ResourceQuota`` (cpu/memory)
     - exact
   * - **GPU** cap
     - ``nominalQuota[amd.com/gpu]``
     - ``ResourceQuota[amd.com/gpu]``
     - **improves on SPUR** (unenforced today)
   * - ``max_wall``
     - ``Workload.maximumExecutionTimeSeconds``
     - inject ``activeDeadlineSeconds``
     - exact
   * - ``max_jobs``/``max_submit`` per user
     - — (Kueue quota is resource-based, not count-based)
     - per-user pod-count ``ResourceQuota`` or a gate
     - **partial** — needs a per-user gate either way
   * - Fair-share ordering
     - Kueue **fairSharing** (usage-weighted)
     - SPUR loop resizes quota + orders ``schedulingGates``
     - Kueue's is better + GPU-aware once we feed GPU usage
   * - Priority (multiplicative)
     - **WorkloadPriorityClass** (flat int) + fairSharing
     - **PriorityClass** buckets
     - **does not port 1:1**; use Kueue fairSharing + a priority class from
       QoS/tier; drop the age×tier product
   * - Preemption
     - ``ClusterQueue.preemption`` (``reclaimWithinCohort`` +
       ``withinClusterQueue``)
     - kube-scheduler preemption via PriorityClass
     - priority-based, not SPUR's ``<half`` threshold
   * - Partition (node group)
     - **ResourceFlavor** (node-label match)
     - node labels + per-ns default ``nodeSelector``
     - exact
   * - Partition account ACL
     - Namespace + RBAC (who can submit to the account's LocalQueue)
     - Namespace + RBAC
     - exact
   * - Submit-error vs park-pending
     - Workload stays **inadmissible/suspended** = "pending with reason"
     - must use **schedulingGates** to *queue*, not bare ResourceQuota
     - see below

Object model (Kueue backend — the recommended default)
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

- **Namespace** per account · **LocalQueue** per namespace → **ClusterQueue** per
  account.
- **ClusterQueue.nominalQuota** per resource (``cpu``, ``memory``,
  ``amd.com/gpu``, GRES types) from the account's allocation.
- **ResourceFlavor** per partition (node-label match) → ``resourceGroups`` on the
  ClusterQueue.
- **Cohort** per organization/parent-account with ``fairSharing`` → real,
  hierarchical fair borrowing (fixes SPUR's flat/CPU-only fair-share; this is
  where fair-share *actually* becomes better than SPUR).
- **WorkloadPriorityClass** derived from QoS priority + partition tier (exposes
  the QoS priority SPUR never wired).
- Per-user QoS caps (``max_jobs``/``max_tres_per_user``) → a small admission gate
  (Kueue has no per-user quota) — the one dimension neither backend gets natively.

The one real fidelity cliff: *queue* vs *reject*
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

SPUR **parks over-limit jobs as Pending** (it never rejects at submit for a
limit).

- **Kueue reproduces this exactly** — a Workload over quota is
  *suspended/inadmissible*, i.e. queued.
- **Bare ResourceQuota does NOT** — it *rejects the pod at create*. So the native
  backend must gate with **schedulingGates** (create the pod gated, admit when
  within the fair-share-adjusted allowance) to preserve "queue, don't reject";
  ResourceQuota alone is only acceptable for truly-hard caps.

This is the strongest argument for Kueue as the default: it is the only option
that keeps SPUR's "everything queues, nothing bounces" behavior with native
objects.

SPUR-side cleanups this surfaced (worth doing regardless of k8s)
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

- **Enforce GPU** in ``sum_running_tres`` / QoS limits, and **write
  ``gpu_seconds``** so fair-share is GPU-aware (today a GPU cluster has *no* GPU
  quota or GPU fairness).
- The **base-priority-0 multiplicative collapse** (default-priority jobs get
  effective priority 1, nullifying fair-share/age/tier).
- Decide the fate of the dead fields (``Qos.priority``/``preempt_mode`` /
  ``usage_factor``, account/association limits): either wire them or drop them —
  don't map dead behavior.

Net
~~~

The mapping is *smaller and cleaner* than feared: only QoS resource caps +
job/submit counts + fair-share ordering are live. Kueue covers the resource caps,
wall, fair-share (better, hierarchically, GPU-aware) and preemption natively and
preserves queue-don't-reject; the native backend covers the same via ResourceQuota
+ PriorityClass + schedulingGates with the one caveat above. Per-user count caps
need a small gate in both. And the port is the moment to give the GPU cluster the
GPU quota + GPU fairness it doesn't have today.

Decisions (locked)
------------------

1. **Namespace granularity:** **one namespace per account** (partition applied via
   the namespace's default ``nodeSelector``). Simplest tenancy/quota boundary.
2. **Fairness engine:** support **both** behind a ``FairnessBackend`` interface —
   **Kueue-driven is the default/recommended path**, SPUR-native gating is the
   no-extra-component alternative — with **tests for both**.
3. **Identity:** **ServiceAccount tokens** — ``spur k8s kubeconfig --user <u>``
   mints a SA + namespace-scoped RBAC + bound (rotatable) token. (OIDC is a future
   option for human SSO, not M1.)

Open questions / risks
----------------------

- **Kueue ↔ SPUR-QoS mapping fidelity** needs a spike (cohorts/borrowing vs
  fair-share half-life; how QoS priority/preemption maps to Kueue).
- **Reconcile authority / drift:** SPUR owns the ResourceQuota/RBAC/Kueue objects;
  an admin hand-editing them gets reverted — need a clear "SPUR-managed" contract
  (labels + admission guard) + an escape hatch.
- **ResourceQuota can't evict already-running pods** when a quota shrinks; only
  Kueue/preemption or a controller can reclaim. Define shrink semantics (native
  backend especially).
- **GPU accounting granularity** (whole-GPU vs MIG/partitions) for fair-share.
- **ServiceAccount token lifecycle** — rotation, revocation on account/user
  removal.
