Idle-Fill Scheduling
====================

A quota is a ceiling on what a team may *claim*, not a rule that capacity should
sit empty when nobody wants it. Without idle-fill, a job that has hit its QOS
group node quota waits even when nodes are idle, and those node-hours are simply
lost — the job is not waiting for a resource, it is waiting for a number.

Idle-fill lets such a job run anyway, on capacity no job with a quota claim wants.
A run placed this way is **borrowed**: it consumes no quota while it runs, and it
is reclaimed when a job that does hold a claim needs the nodes.

The feature is off by default. Enable it with ``[scheduler] idle_fill_enabled``.

.. important::

   Enabling idle-fill **narrows a documented preemption guarantee**. Reclaim
   ignores ``preempt_mode``, so on a partition with ``preempt_mode = "off"`` a
   borrowed job can still be evicted. Read `What the guarantee becomes`_ before
   enabling it on a cluster whose users rely on that promise.

Configuration
-------------

.. code-block:: toml

   [scheduler]
   idle_fill_enabled = true    # default false
   idle_fill_exempt_secs = 60  # default 60

``idle_fill_enabled``
   The master switch. With it off, nothing in this page applies and no job is
   ever stamped as borrowed.

``idle_fill_exempt_secs``
   How long a borrowed job runs before it may be reclaimed. This is the only
   guard between a borrowed job and reclaim, so it is deliberately short — order
   of a minute — and it is capped. The window **doubles on each successive
   eviction of the same job**, up to one hour, so a job that keeps losing its
   node backs off rather than being lent and evicted indefinitely.

There is deliberately no reuse of ``preempt_exempt_time`` here. That knob is
unbounded, and a user can raise their own by submitting to several partitions,
since the most protective matched partition wins. A cluster-wide
``preempt_exempt_time = 3600`` would protect every borrowed job from reclaim for an
hour, leaving a job with a real claim waiting for capacity that was lent away.

Which jobs may borrow
---------------------

A pending job is eligible only when its **QOS group node cap is the sole limit it
breaches**. Every other limit is still enforced: other group TRES dimensions,
per-job and per-user caps, max running jobs, max submit jobs, and the group wall
budget.

This is stricter than it may first appear, and the strictness is the point. A job
over both its node cap *and* its GPU cap must not be lent nodes, because doing so
would let it escape a cap on a genuinely contended resource. Eligibility is
therefore decided by re-running the full limit check with the node cap lifted, and
treating the job as eligible only if it then passes — not by reading the reason
code the job reports, which names only the first breach found.

Four kinds of job are never lent capacity:

- **Jobs with no effective time limit.** A borrowed job with no bound would make
  its node appear busy indefinitely, and other jobs would plan around a slot years
  out. Set ``default_time_limit_minutes``, a partition ``max_time``, or submit with
  ``-t`` if you want these jobs to be eligible.
- **Heterogeneous job components**, because a component placed on borrowed
  capacity can strand its own siblings.
- **Jobs requesting a burst buffer**, whose staging state cannot be evaluated
  speculatively.
- **Jobs requesting licenses**, which cannot be re-checked without risking a
  license an in-quota job claimed in the same scheduling pass.

Where borrowed jobs are placed
------------------------------

Borrow candidates are offered only the capacity left after every in-quota job has
taken what it can start on *and* reserved what it is waiting for. They are
considered last, below every job with a claim, so a borrowed job can never take a
node an in-quota job would have used, and it cannot displace a future slot the
scheduler is holding.

A candidate that cannot start immediately simply does not start; it holds no
future reservation. Under very deep queues, borrow candidates are the first thing
dropped, so idle-fill degrades to off rather than crowding anyone out.

Quota accounting
----------------

A borrowed run is held outside **every** quota aggregate, at both the association
and the QOS gate: node and other TRES totals, occupied-node counts, running and
submitted job counts, and array concurrency.

This is not cosmetic. If a borrowed job counted toward its own team's quota, it
would block that team's *next legitimate job* at an admission gate — and a job
blocked there is removed from the pending list before the scheduler ever sees it,
so it could never trigger reclaim. The lent capacity could then never be recovered,
which is the exact failure the feature exists to prevent.

One consequence to plan for: because borrowed nodes leave the aggregates, neither
the QOS group node cap nor the association node cap bounds how much a scope may
borrow. Only the amount of genuinely idle capacity does.

.. note::

   A QOS group node cap counts **distinct occupied nodes, not jobs**. Two
   non-exclusive jobs that pack onto one node consume one node of quota, so a cap
   may never be reached at all by jobs that share nodes. Expect idle-fill to
   engage mainly for exclusive or whole-node work.

Reclaim
-------

When a job holding a quota claim cannot be placed, Spur looks for borrowed jobs
whose eviction would let it run.

The set of jobs to evict must **provably close the shortfall, or nothing is evicted**. A node
counts as recovered only when every job on it is reclaimable, so the outcome is
all-or-nothing: the waiting job gets a full allocation, or the cluster is left
untouched. A partial eviction would destroy work without helping anyone, and a job
that can never be placed anywhere — asking for more GPUs than any node has, say —
can never assemble such a set, and so evicts nothing, ever.

Reclaimed jobs are **requeued, not cancelled**. The job returns to the queue with
its spec intact and starts again when capacity allows, so the user loses the
partial run, not the work. ``--no-requeue`` does not prevent this; suspension is
not used, because a suspended job keeps its node allocation and would never
release the capacity.

Reclaim also does not charge the lost run to fairshare. Charging it would lower the
borrower's priority for work it was not allowed to finish, which would make it the
preferred next victim — the more capacity it lost, the more it would go on to lose.

Reclaim is attempted before preemption, since recovering lent capacity is always
preferable to evicting a job that holds a claim to it.

What the guarantee becomes
--------------------------

The partition setting ``preempt_mode = "off"`` documents that running jobs are
never kicked out. With idle-fill enabled that promise narrows to:

   **Jobs with a quota claim are never kicked out.**

Reclaim consults none of ``preempt_mode``, the priority gap, the QOS allow list,
or ``preempt_exempt_time``. It is not preemption: preemption arbitrates between two
jobs that both hold a claim, whereas a borrowed job holds none.

The alternative — refusing to lend capacity wherever reclaim is not permitted —
was rejected because it would make idle-fill inert on a default-configured cluster.
A borrowed job is *defined* by being reclaimable, so a setting that shielded it
would not yield a safer job; it would yield capacity that was lent out and can
never be recovered.

Two things bound the change. The master switch is off by default, so no existing
cluster behaves differently until an operator enables it. And the guarantee weakens
only for jobs that never had a claim to the capacity: for every job running inside
its quota, ``preempt_mode = "off"`` still means exactly what it says.

Relationship to burst QOS
-------------------------

Spur already supports opportunistic borrowing through the burst-QOS pattern (see
:doc:`accounting`), where a low-priority QOS is made preemptable by other QOS via
``preempt_type = qos_priority`` and allow-lists.

The two differ in who decides. The burst pattern is opt-in per job and requires the
submitter to choose a burst QOS, and an operator to maintain the allow-lists.
Idle-fill is automatic and derived from the quota a team already has: any job whose
only obstacle is its group node cap is considered, with nothing to opt into.

A per-QOS flag, ``idle_fill_preemptable``, marks a QOS whose jobs are always
reclaimable even while inside quota, so an existing burst QOS can be migrated onto
idle-fill's reclaim path. Jobs in such a QOS still count fully toward every quota,
because they are running inside their own.

Set it with ``sacctmgr``:

.. code-block:: console

   $ sacctmgr -i modify qos burst set idlefillpreemptable=yes
   $ sacctmgr show qos format=Name,IdleFillPreemptable
   Name            IdleFillPreemptable
   burst           yes
   normal          no

The flag accepts ``yes``/``no``, ``true``/``false``, and ``1``/``0``. It defaults to
``no`` on every QOS, including those created before this release, so enabling
idle-fill does not make any existing workload reclaimable until you say so.

Observing borrowed runs
-----------------------

``squeue`` reports it with the ``%W`` field:

.. code-block:: console

   $ squeue -o "%i %j %u %t %N %W"
   JOBID NAME         USER     ST NODELIST BORROWED
   18    alice-legit  ifalice  R  node-3   no
   19    bob-borrow   ifbob    R  node-4   yes

An over-quota job that has *not* been lent capacity keeps reporting its real
reason, ``QOSGrpNodeLimit``, rather than a placement reason — being over quota is
the truth about why it is waiting.

``sacct --format=Borrowed`` records whether a completed run was borrowed, and a
reclaimed run is
reported as ``PREEMPTED`` with ``PreemptMode=Requeue`` and the job that reclaimed
it. See :doc:`/user-guide/monitoring-jobs`.
