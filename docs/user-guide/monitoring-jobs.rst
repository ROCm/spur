Monitoring & Controlling Jobs
=============================

Once jobs are submitted, you inspect the queue, the nodes, and the accounting
history with a handful of commands, and you steer running jobs with cancel,
hold, and update operations. This page covers viewing the queue (``spur queue``),
nodes and partitions (``spur nodes``), accounting history (``spur history``),
live job stats (``spur stat``), detailed records (``spur show``), and the
controls that change a job's state.

.. note::

   In ``spur queue`` (``squeue``) and ``spur nodes`` (``sinfo``), ``-h`` means
   ``--noheader``, not help. ``spur history`` (``sacct``) uses ``-n`` for the
   same purpose.

View the Queue — ``squeue``
---------------------------

``spur queue`` (Slurm ``squeue``) lists jobs currently in the system. With no
arguments it shows every active job.

.. code-block:: bash

   spur queue

.. code-block:: text

   JOBID PARTITION     NAME     USER ST       TIME  NODES NODELIST(REASON)
       1       gpu train-ll    alice  R       2:14      2 node[01-02]
       2   default    hello      bob PD       0:00      1 (Priority)

The default columns are ``JOBID PARTITION NAME USER ST TIME NODES
NODELIST(REASON)``. ``--long``/``-l`` adds the full STATE and TIME_LIMIT.

Common flags:

.. list-table::
   :header-rows: 1
   :widths: 26 10 64

   * - Long
     - Short
     - Description
   * - ``--user``
     - ``-u``
     - Show only this user's jobs.
   * - ``--partition``
     - ``-p``
     - Filter by partition.
   * - ``--states``
     - ``-t``
     - Filter by state codes or names (comma list). ``-t all`` shows every
       state.
   * - ``--account``
     - ``-A``
     - Filter by account.
   * - ``--name``
     - ``-n``
     - Filter by job name.
   * - ``--format``
     - ``-o``
     - Custom column format (see below).
   * - ``--long``
     - ``-l``
     - Long form; adds STATE and TIME_LIMIT.
   * - ``--noheader``
     - ``-h``
     - Omit the header line.

When ``--states`` is omitted the default filter is **PENDING, RUNNING,
SUSPENDED, COMPLETING**.

``--format``/``-o`` uses ``%[.][-][width]<letter>`` fields. The resolved letters:

.. list-table::
   :header-rows: 1
   :widths: 18 40 18 40

   * - Letter
     - Column
     - Letter
     - Column
   * - ``%i`` / ``%A``
     - JOBID
     - ``%C``
     - CPUS
   * - ``%j`` / ``%n``
     - NAME
     - ``%N``
     - NODELIST
   * - ``%u``
     - USER
     - ``%R``
     - NODELIST(REASON)
   * - ``%a``
     - ACCOUNT
     - ``%r``
     - REASON
   * - ``%P``
     - PARTITION
     - ``%p``
     - PRIORITY
   * - ``%q``
     - QOS
     - ``%Z``
     - WORK_DIR
   * - ``%t``
     - ST (short code)
     - ``%o``
     - COMMAND
   * - ``%T``
     - STATE (full)
     - ``%v``
     - RESERVATION
   * - ``%M``
     - TIME
     - ``%S``
     - START_TIME
   * - ``%l``
     - TIME_LIMIT
     - ``%V``
     - SUBMIT_TIME
   * - ``%D``
     - NODES
     - ``%e``
     - END_TIME
   * - ``%Y``
     - SCHEDNODES
     - ``%L``
     - TIME_LEFT
   * - ``%b``
     - GRES
     -
     -

.. code-block:: bash

   spur queue -u alice -t R
   squeue -p gpu -o "%.18i %.9P %.8T %.10M %R"
   squeue --states=PD,R --noheader

Projected Start Times — ``squeue --start``
-------------------------------------------

``--start`` answers "when will my job run, and where" for the whole queue at
once, rather than one job at a time through ``scontrol show job``:

.. code-block:: console

   $ squeue --start
        JOBID PARTITION     NAME     USER ST          START_TIME  NODES           SCHEDNODES NODELIST(REASON)
         1024       gpu  train.sh    alice PD 2026-08-27T17:09:56      2          node[07-08] (Resources)
         1031       gpu   eval.sh      bob PD                 N/A      1               (null) (Priority)

It restricts the view to pending jobs and orders them by projected start,
soonest first; jobs with no reserved slot report ``N/A`` and sort last. Each of
those defaults is overridable by passing ``-t``, ``-S``, or ``-o`` explicitly.

``%S`` and ``%e`` carry the same projection in any format string, so
``squeue -o "%.18i %.19S %.19e"`` works without ``--start``.

.. note::

   Projections come from the backfill pass on the controller's leader, and
   assume every running job uses its full time limit. A job that finishes early
   moves every projection behind it earlier.

**Job state codes.** The ``ST`` column uses these short codes:

.. list-table::
   :header-rows: 1
   :widths: 12 30 12 30

   * - Code
     - State
     - Code
     - State
   * - ``PD``
     - PENDING
     - ``CA``
     - CANCELLED
   * - ``R``
     - RUNNING
     - ``TO``
     - TIMEOUT
   * - ``CG``
     - COMPLETING
     - ``NF``
     - NODE_FAIL
   * - ``CD``
     - COMPLETED
     - ``PR``
     - PREEMPTED
   * - ``F``
     - FAILED
     - ``S``
     - SUSPENDED
   * - ``DL``
     - DEADLINE
     - ``OOM``
     - OUT_OF_MEMORY

For a pending job the ``NODELIST(REASON)`` column shows why it is waiting.
Common reasons include ``Priority`` (waiting its turn), ``Resources`` (waiting
for nodes to free up), ``Dependency`` (waiting on another job), ``Reservation``
(waiting for a reservation window), ``Preempted`` (requeue-preempted and held
until its eligibility window reopens), ``JobArrayTaskLimit`` (held back by the
array's own ``%N`` concurrency cap, see :ref:`submit-arrays`), and various QOS
or association limit reasons (``QOSMax*``, ``AssocMax*``, ``AssocGrp*``).

View Nodes & Partitions — ``sinfo``
-----------------------------------

``spur nodes`` (Slurm ``sinfo``) shows partition and node state. The default is a
partition-oriented view; ``-N`` switches to one line per node.

.. code-block:: bash

   spur nodes

.. code-block:: text

   PARTITION AVAIL TIMELIMIT NODES STATE NODELIST
   default*     up  infinite     4  idle node[01-04]
   gpu          up  1-00:00:00   2   mix node[05-06]

Flags:

.. list-table::
   :header-rows: 1
   :widths: 26 10 64

   * - Long
     - Short
     - Description
   * - ``--partition``
     - ``-p``
     - Filter by partition.
   * - ``--nodes``
     - ``-n``
     - Filter by node.
   * - ``--node-oriented``
     - ``-N``
     - One line per node instead of per partition.
   * - ``--list-reasons``
     - ``-R``
     - List unavailable nodes (down/drain/draining/error) grouped by reason.
   * - ``--format``
     - ``-o``
     - Custom column format.
   * - ``--long``
     - ``-l``
     - Long form; adds CPUS and MEMORY.
   * - ``--noheader``
     - ``-h``
     - Omit the header line.

Default columns (partition view): ``PARTITION AVAIL TIMELIMIT NODES STATE
NODELIST``; ``-l`` adds CPUS and MEMORY. Node view (``-N``): ``NODELIST NODES
PARTITION STATE CPUS MEMORY GRES``.

.. code-block:: bash

   sinfo -N -o "%N %.6D %.11T %c %m %G"
   sinfo -p gpu -l

``-R`` / ``--list-reasons`` lists only nodes that are unavailable (``down``,
``drain``, ``drng``, ``err``), grouped by the admin reason set when the node was
drained or downed, one row per reason with a compressed nodelist:

.. code-block:: text

   REASON               USER      TIMESTAMP           NODELIST
   Memory errors        alice     2026-08-27T08:03:07 node[05,08]
   Not responding       root      2026-08-27T09:15:44 node[01-03]

All four columns are populated from live state. ``USER`` (``%u``, or ``%U`` for
``user(uid)``) is the user who set the reason, resolved from the recorded UID;
automatic reasons (for example a heartbeat timeout) are attributed to ``root``.
``TIMESTAMP`` (``%H``) is when the reason was set. A UID that cannot be resolved
to a name, or a node whose reason predates provenance tracking, renders as
``Unknown``. ``-R -l`` adds a ``STATE`` column (``%t``).

``scontrol show node`` appends the same provenance to the reason line as
``Reason=<text> [<user>@<timestamp>]`` when a set-time is recorded, and the REST
node object carries ``reason_uid`` and ``reason_time`` fields.

Not every reason comes from an admin. A node still reporting ``idle`` or ``mix``
may nonetheless be skipped by the scheduler, and says so in the same field
(``Reason=`` in ``scontrol show node``, ``%E`` in ``sinfo``):

.. list-table::
   :header-rows: 1
   :widths: 42 58

   * - Reason
     - Meaning
   * - ``reconciling with the controller``
     - The agent has just registered and declared what it holds; the node takes
       no new work until the controller has compared that against its own
       records. Usually immediate, but a controller still replaying its own log
       waits for that first, and the whole pass is capped at a minute.
   * - ``holding claims the controller has no record of: <ids>``
     - The node is holding resources for runs the controller cannot place, and
       could not resolve them itself. The node keeps working; the named runs are
       what an operator should look at. It clears itself once the node reports
       them or they are otherwise settled.
   * - ``dispatch cooldown after a failed launch (<n>s remaining)``
     - A launch failed on this node, so the scheduler skips it for
       ``controller.dispatch_reject_cooldown_secs`` instead of re-picking it
       every cycle. It returns to service on its own when the countdown ends;
       the launch failure itself is reported on the affected job.

A reason an admin set with ``scontrol update`` takes precedence over both: a
drained node reports the drain, not the transient skip.

Node states are shown as short abbreviations: ``idle`` (free), ``alloc`` (fully
allocated), ``mix`` (partly allocated), ``down``, ``drain`` (offline, not
accepting jobs), ``drng`` (draining), ``err`` (error), ``unk``
(unknown/unreachable), ``susp`` (suspended), and — all three only shown while
the node is currently idle — ``resv`` or ``maint`` for a node held by an admin
reservation, else ``plnd`` for a node currently held by the scheduler for a
specific pending job's upcoming start. The idle gate is checked live, but the
job and start time shown for ``plnd`` reflect the most recent scheduling
cycle (``scheduler.interval_secs``), not the current instant.

When a node's resources come back
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

A job's CPUs, GPUs and memory go back to the node when the controller has
committed the job's completion — not when the job's processes exit. The agent
reports the exit, waits for the controller to acknowledge it, and only then frees
the slice.

The visible effect is a lag: for a moment after a job finishes, ``sinfo`` and
``scontrol show node`` still count its resources as allocated on a node where
nothing is running. With no epilog configured this is one round trip and you will
rarely catch it. Where the node runs an epilog, the slice stays booked for as long
as that hook takes, so the next job cannot start on top of the cleanup.

A node still running a finished job's epilog reports as ``mix`` or ``alloc`` with
a nonzero ``CPUAlloc`` and nothing for it in ``squeue`` — the tasks are gone, the
hook is not. It reads as ``drng`` only if an operator has also drained it. The
hold ends when the node reports, when the job is requeued, or when the node is
removed from the cluster — never on a timer, and not merely because the node
stopped answering.

If the controller is unreachable, the lag lasts as long as the outage. The agent
keeps retrying the completion report — backing off to at most a minute between
attempts, indefinitely, across an agent restart as well — and keeps the resources
booked until one is acknowledged. Nothing frees them locally on a timer: the
agent having lost track of a job is not evidence the job's work finished, so a
node nobody can speak for holds its claims rather than reissuing them. Held
resources on an unreachable node are not schedulable anyway, since the node goes
``down`` once its heartbeat times out, and they are resolved when it reconnects
and reconciles.

A report the controller will not accept holds that job's resources for good. The
agent calls this out in its log every fifteen minutes, naming the job and how
long it has been held — worth an alert, because the node shrinks silently
otherwise.

Accounting History — ``sacct``
-------------------------------

``spur history`` (Slurm ``sacct``) reports finished and running jobs from the
accounting service.

.. code-block:: bash

   spur history -u alice -S now-7days

Flags:

.. list-table::
   :header-rows: 1
   :widths: 26 10 64

   * - Long
     - Short
     - Description
   * - ``--user``
     - ``-u``
     - Filter by user.
   * - ``--account``
     - ``-A``
     - Filter by account.
   * - ``--starttime``
     - ``-S``
     - Earliest submit/start time to include.
   * - ``--endtime``
     - ``-E``
     - Latest time to include.
   * - ``--state``
     - ``-s``
     - Filter by state (comma list). Accepted values (long or short code):
       ``COMPLETED``/``CD``, ``FAILED``/``F``, ``CANCELLED``/``CA``,
       ``TIMEOUT``/``TO``, ``NODE_FAIL``/``NF``, ``PREEMPTED``/``PR``,
       ``DEADLINE``/``DL``, ``RUNNING``/``R``, ``PENDING``/``PD``.
   * - ``--jobs``
     - ``-j``
     - Filter by job ID (comma list; step suffixes like ``30.0`` match the base job).
   * - ``--format``
     - ``-o``
     - Comma-separated field names (see below).
   * - ``--long``
     - ``-l``
     - Long form; adds DerivedExitCode, Start, End, TimeLimit.
   * - ``--brief``
     - ``-b``
     - Brief form: JobID, State, ExitCode.
   * - ``--noheader``
     - ``-n``
     - Omit the header line.
   * - ``--limit``
     -
     - Maximum rows to return. Default ``100``.

Unlike ``squeue``, ``--format`` here takes **comma-separated field names**, not
``%`` letters. Available fields include ``JobID``, ``JobName``, ``User``,
``Account``, ``Partition``, ``State``, ``Elapsed``, ``NNodes``, ``ExitCode``,
``DerivedExitCode``, ``Start``, ``End``, ``Submit``, ``TimeLimit``, ``NodeList``,
``NCPUS``, ``QOS``, ``PreemptedBy``, ``PreemptMode``, and ``PreemptQOS``. Set a
per-field width with ``Field%N``, e.g. ``JobName%20``.

``PreemptedBy`` is the job ID of the higher-priority job that caused the
preemption (``N/A`` when the job was not preempted). ``PreemptMode`` is one of
``Requeue``, ``Cancel``, or ``Suspend``. ``PreemptQOS`` is the QOS name that
authorized the preemption under ``preempt_type = qos_priority``; ``N/A`` for
plain priority-based preemption. All three appear in the long format (``-l``).

The default columns are ``JobID JobName User Account Partition State Elapsed
NNodes ExitCode``.

Time arguments accept an absolute date (``YYYY-MM-DD`` or
``YYYY-MM-DDTHH:MM:SS``) or a relative offset (``now-7days``, ``now-6hours``).

.. code-block:: bash

   sacct -S 2026-07-01 -E 2026-07-25 -s FAILED --limit 500
   sacct --format=JobID,JobName,State,Elapsed,ExitCode
   sacct -s PREEMPTED --format=JobID,JobName,State,ExitCode,PreemptedBy,PreemptMode,PreemptQOS
   sacct -s PR,CA     # short codes also accepted

Running-Job Stats — ``sstat``
-----------------------------

``spur stat`` (Slurm ``sstat``) reports live resource usage for running jobs.
``--jobs``/``-j`` is required (comma list).

.. code-block:: bash

   spur stat -j 12345
   sstat -j 12345,12346 -o JobID,NTasks,GPUAlloc,Elapsed -p

Default fields: ``JobID NTasks NCPUS MemAlloc GPUAlloc Elapsed Nodelist``.
``--format``/``-o`` takes field names; ``--parsable``/``-p`` produces
``|``-delimited output.

.. note::

   The per-process ``Ave*`` and ``Max*`` fields (``AveCPU``, ``AveRSS``,
   ``MaxRSS``, and similar) always report ``N/A`` — Spur does not collect
   per-process metrics.

Detailed Records — ``scontrol show``
------------------------------------

``spur show`` (Slurm ``scontrol show``) prints the full ``Key=Value`` record for
an entity: ``job``, ``node``, ``partition``, ``reservation``, ``step``, or
``assoc_mgr``.

.. note::

   The diagnostic fields below are printed on every job, with ``(null)`` for an
   unset string, so a parser must key on the field name rather than on a line
   being absent. Some fields still appear only when set (``Comment``,
   ``Reservation``, ``ArrayJobId``, ``SchedNodeList``, ``StdIn``, the GPU line,
   and the preemption line), and ``MinMemoryNode`` is spelled ``MinMemoryCPU``
   for a ``--mem-per-cpu`` job.

.. code-block:: bash

   scontrol show job 1024
   spur show node node01
   scontrol show partition gpu

Diagnosing a job that will not start
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

A job pending on ``Reason=Resources`` looks the same whether the cluster is full
or the job is pinned to a busy subset of it. ``scontrol show job`` reports the
request as submitted, which distinguishes the two:

.. list-table::
   :header-rows: 1
   :widths: 24 76

   * - Field
     - Meaning
   * - ``ReqNodeList``
     - Nodes requested with ``-w/--nodelist``. ``(null)`` when unrestricted.
   * - ``ExcNodeList``
     - Nodes excluded with ``-x/--exclude``.
   * - ``NodeList``
     - Nodes actually **allocated**. Empty (``(null)``) while pending — do not
       confuse it with ``ReqNodeList``.
   * - ``Features``
     - Node feature constraint from ``-C/--constraint``.
   * - ``ReqTRES``
     - Requested resource totals, e.g. ``cpu=16,mem=32000M,node=2,gres/gpu=8``.
   * - ``MinCPUsNode`` / ``MinMemoryNode``
     - Per-node minima implied by the request. Memory is in MB with an ``M``
       suffix; a bare ``0`` means none was requested. A ``--mem-per-cpu`` job
       reports ``MinMemoryCPU`` instead, as Slurm does.
   * - ``Dependency``, ``Requeue``, ``Restarts``, ``BatchFlag``, ``Exclusive``
     - Submission flags and the requeue count for this job.
   * - ``RunTime`` / ``TimeLimit`` / ``TimeMin`` / ``Deadline``
     - Elapsed and requested wall time. ``TimeLimit`` reads ``UNLIMITED`` when
       none was set; ``TimeMin`` and ``Deadline`` read ``N/A`` when unset.
   * - ``Command``
     - First executable line of the batch script.
   * - ``SubmitLine``
     - The submit command as invoked, so any submission-flag question is
       answerable from one command.
   * - ``EligibleTime``
     - Earliest the job may start (``--begin``, otherwise submit time).
   * - ``AccrueTime``
     - When the job began accruing age priority.
   * - ``StartTime`` / ``SchedNodeList``
     - While pending, the slot the scheduler is holding: when it projects the
       job will start, and on which nodes. With no slot reserved, ``StartTime``
       reads ``N/A`` and ``SchedNodeList`` is omitted. ``StartTime`` becomes the
       real start once the job runs. A slot the scheduler held only because its
       search ran past a year out also reads ``N/A``: the slot is still
       reserved, but that date is where the search stopped, not a projection.
   * - ``EndTime``
     - The recorded end once the job finishes, otherwise its start plus its
       time limit. ``N/A`` for an unlimited job, or a pending one with no
       projected start.
   * - ``LastSchedEval``
     - The last scheduling cycle that considered this job, so it advances even
       on a cycle that places nothing. ``N/A`` before the first cycle, frozen
       once the job starts, and reset by a controller restart or failover.
       On a deep queue it also covers jobs classified but left untried past
       ``scheduler.max_jobs_per_cycle``.

.. note::

   ``StartTime``, ``SchedNodeList``, and ``LastSchedEval`` live only on the
   leader and are never replicated, so a read served by a follower reports them
   unset even in a healthy cluster.

So a job pinned to a busy node reads (excerpt — the full record has more
fields between these lines):

.. code-block:: text

      JobState=PENDING Reason=Resources Dependency=(null)
      ...
      StartTime=2026-08-27T17:09:56 EndTime=2026-08-27T17:14:56 Deadline=N/A
      ...
      ReqNodeList=node07 ExcNodeList=(null)
      NodeList=(null) SchedNodeList=node07
      ...
      SubmitLine=sbatch -w node07 --exclusive -t 5 job.sh

The controller logs the matching scheduler-side view at ``info`` level, one line
per job when its placement outcome changes (not every cycle). A cycle with no
schedulable nodes at all is skipped before this runs, so it logs nothing. Jobs beyond the
per-cycle scheduling depth limit are not reported:

.. code-block:: text

   backfill did not start job job_id=1024 reason=no_suitable_nodes needed_nodes=1 candidate_nodes=0

``reason`` is one of ``het_group_incomplete``, ``no_suitable_nodes``,
``requested_nodes_unavailable``, ``too_few_candidates``, ``no_capacity_at_start``,
or ``future_slot_reserved``. The last means the job can be placed and the
scheduler is holding a future slot for it, reported with ``planned_start``.

.. note::

   Spur accrues age priority from submit time for every pending job, including
   held and dependency-blocked ones. Slurm suspends accrual for those, so
   ``AccrueTime`` can read earlier here than on an equivalent Slurm job.

Preemption provenance
~~~~~~~~~~~~~~~~~~~~~

When a job has been preempted, ``scontrol show job`` includes three additional
fields:

* ``PreemptedBy=<job_id>`` — the ID of the higher-priority job that triggered
  the preemption. Only shown when the job has been preempted.
* ``PreemptMode=Requeue|Cancel|Suspend`` — how the preemption was carried out.
* ``PreemptQOS=<name>`` — the QOS that authorized the preemption under
  ``preempt_type = qos_priority``; ``N/A`` for plain priority-based preemption.

**Quick reference — one command for any preempted job:**

.. code-block:: bash

   scontrol show job <id>

This works regardless of preemption mode (requeue, cancel, or suspend) as long
as the job is still known to the controller (running, pending, suspended, or
recently terminal). For cancel-preempted jobs that have been evicted from
memory, fall back to:

.. code-block:: bash

   sacct -j <id> --format=JobID,State,PreemptedBy,PreemptMode,PreemptQOS

The accounting database keeps the record permanently. In practice
``scontrol show job`` is the right first instinct; use ``sacct`` only if
``scontrol`` returns ``Invalid job id``.

.. note::

   **Suspend-mode preemption and accounting.** When ``PreemptMode=Suspend``,
   the job receives SIGSTOP and stays running — no accounting end-record
   is written. The provenance fields (``PreemptedBy``, ``PreemptMode``,
   ``PreemptQOS``) are visible in ``scontrol show job`` while the job is
   suspended, but are cleared when the job resumes so that a subsequent normal
   completion is not miscounted as a preemption. As a result, ``sacct`` has
   no record of the preemption for suspend-mode jobs: the accounting row for
   that run will show the final completion state only. Requeue and cancel modes
   are unaffected — both write an accounting end-record (``PREEMPTED``) at the
   time of preemption.

Limits Against Usage — ``scontrol show assoc_mgr``
--------------------------------------------------

``scontrol show assoc_mgr`` answers the question ``squeue`` cannot: how much of a
QOS or an association each user is holding *right now*, next to the caps that
govern them. Without it an operator has to infer per-user totals by counting
``squeue`` rows.

.. code-block:: bash

   scontrol show assoc_mgr
   scontrol show assoc_mgr alice
   scontrol show assoc_mgr users=alice

Output is two sections of ``Key=Value`` blocks — QOS records, then association
records. Each block opens with the scope itself and lists a line per user under
it:

.. code-block:: text

   QOS Records
   QOS=highprio MaxWall=01:00:00 MaxJobsPU=2 MaxSubmitJobsPU=N MaxTRESPU=node=4
      GrpJobs=N(9) GrpSubmitJobs=N(11) GrpTRES=cpu=N(36),node=16(9)
      User=alice MaxJobsPU=2(6) MaxSubmitJobsPU=N(7) MaxTRESPU=cpu=N(24),node=4(6) OverLimit=MaxJobsPU,MaxTRESPU
      User=bob MaxJobsPU=2(1) MaxSubmitJobsPU=N(1) MaxTRESPU=cpu=N(4),node=4(1)

   Association Records
   Account=tenant-a MaxWall=N
      GrpJobs=N(1) GrpSubmitJobs=N(1) GrpTRES=node=N(1)
      User=alice MaxJobs=4(1) MaxSubmitJobs=N(1) MaxTRES=cpu=N(4),node=N(1)

Reading it:

* On the ``Grp*`` line and the ``User=`` lines every cap is printed as
  **``Limit(Consumed)``**, as in Slurm: ``node=4(6)`` is a cap of four nodes with
  six in use, and ``N`` marks no cap. ``MaxWall`` and the per-user caps on the
  scope line print bare, with no consumption beside them.
* For the count caps a literal ``0`` is a real cap that blocks every job it
  governs. A TRES dimension is the exception: ``0`` there is treated as *unset* —
  it renders as ``N`` and is not enforced, so ``GrpTRES=node=0`` does not block a
  dimension the way ``MaxJobs=0`` blocks jobs.
* Consumption is live, measured exactly as the scheduler measures it when it
  admits a job. Node counts are distinct occupied nodes, so two jobs sharing a
  node hold one node, not two. A TRES dimension appears when either the cap or
  the usage has something to say about it.
* The scope line carries what belongs to the scope: its ``MaxWall``, and — for a
  QOS, which caps every user identically — the per-user caps it enforces. An
  association's caps are per ``(user, account)``, so they appear on each user's
  line instead.
* ``Grp*`` figures are the whole scope's, summed across every user, and stay on
  the scope line. Filtering to one user narrows the ``User=`` lines but never the
  group figures, since a group cap cannot be judged from one user's share.
* ``OverLimit`` lists caps that are **already exceeded**, on the scope line for
  group caps and on a user's line for that user's caps. It is absent when
  everything is within its cap. Usage over a cap is not a contradiction: caps are
  applied when a job is admitted and running jobs are never re-checked, so
  tightening a cap under running work leaves exactly this state.

Records come from the accounting definitions as well as from the queue, so a QOS
nobody is using still appears with its caps and no ``User=`` lines — an
over-tight cap on an idle QOS is exactly the kind of mistake that hides
otherwise. A QOS deleted after its jobs queued also stays reported for as long as
those jobs hold resources.

A ``LimitsReadable=NO`` line above the sections means accounting is enabled but a
cache has not loaded yet, so some caps below may be missing; the usage figures
are still current. A cluster with accounting disabled has no caps to read and so
never prints this line.

An unprivileged caller sees only their own usage: ``scontrol show assoc_mgr``
scopes the view to the caller and lists only the QOS and accounts they take part
in. Administrators see every scope, and may pass ``users=<name>`` to inspect one
user.

Cluster Metrics — ``/metrics``
------------------------------

Beyond the per-job CLI commands above, ``spurctld`` exports cluster-wide metrics
over HTTP in Prometheus/OpenMetrics text format. This is the surface a
monitoring stack (Prometheus, Grafana, an OpenMetrics scraper) consumes to chart
queue depth, node utilization, and resource allocation over time.

The server is controlled by the ``[metrics]`` section of ``spur.conf`` (see
:doc:`/admin-guide/configuration`). It listens on port ``6822`` and by default
binds to loopback only; set ``bind = "all"`` to expose it to a scraper on
another host. Only the Raft **leader** serves data — followers return
``503 Service Unavailable`` — so point your scraper at all controllers and let it
follow the leader.

.. code-block:: bash

   curl http://127.0.0.1:6822/metrics/jobs
   curl http://127.0.0.1:6822/metrics/nodes

Endpoints
~~~~~~~~~

.. list-table::
   :header-rows: 1
   :widths: 34 16 50

   * - Path
     - Status
     - Contents
   * - ``/metrics``
     - Live
     - Alias for ``/metrics/jobs``.
   * - ``/metrics/jobs``
     - Live
     - Job counts by state and aggregate allocated resources.
   * - ``/metrics/nodes``
     - Live
     - Node counts by state, cluster resource totals, and per-node gauges.
   * - ``/metrics/partitions``
     - Planned
     - Route exists but currently returns an empty body.
   * - ``/metrics/scheduler``
     - Planned
     - Route exists but currently returns an empty body.
   * - ``/metrics/k8s``
     - Live
     - Spur-managed k0s cluster lifecycle and per-node health (``spur_k8s_*``).
   * - ``/metrics/jobs-users-accts``
     - Planned
     - Per-user/per-account breakdown. Returns ``404`` until implemented, even
       with ``high_cardinality = true``.

Job metrics — ``/metrics/jobs``
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

All gauges. ``<state>`` expands to one metric per job state: ``pending``,
``running``, ``completing``, ``completed``, ``failed``, ``cancelled``,
``timeout``, ``node_fail``, ``preempted``, ``suspended``, ``deadline``,
``out_of_memory``.

.. list-table::
   :header-rows: 1
   :widths: 44 56

   * - Metric
     - Description
   * - ``spur_jobs``
     - Total number of jobs.
   * - ``spur_jobs_<state>``
     - Number of jobs in each state.
   * - ``spur_jobs_cpus_alloc``
     - CPUs allocated to running/completing jobs.
   * - ``spur_jobs_memory_alloc_bytes``
     - Memory (bytes) allocated to running/completing jobs.
   * - ``spur_jobs_gpus_alloc``
     - GPUs allocated to running/completing jobs.

Node metrics — ``/metrics/nodes``
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

All gauges. Cluster-wide totals carry no labels; per-node gauges carry a
``node=<name>`` label. ``<state>`` expands to one metric per node state:
``idle``, ``alloc``, ``mixed``, ``down``, ``drain``, ``draining``, ``error``,
``unknown``, ``suspended``.

.. list-table::
   :header-rows: 1
   :widths: 44 56

   * - Metric
     - Description
   * - ``spur_nodes``
     - Total number of nodes.
   * - ``spur_nodes_<state>``
     - Number of nodes in each state.
   * - ``spur_nodes_cpus`` / ``spur_nodes_cpus_alloc``
     - Total and allocated CPUs across all nodes.
   * - ``spur_nodes_memory_bytes`` / ``spur_nodes_memory_alloc_bytes``
     - Total and allocated memory (bytes) across all nodes.
   * - ``spur_nodes_gpus`` / ``spur_nodes_gpus_alloc``
     - Total and allocated GPUs across all nodes.
   * - ``spur_node_cpus`` / ``spur_node_cpus_alloc``
     - Total and allocated CPUs on the labeled node.
   * - ``spur_node_memory_bytes`` / ``spur_node_memory_alloc_bytes``
     - Total and allocated memory (bytes) on the labeled node.
   * - ``spur_node_gpus`` / ``spur_node_gpus_alloc``
     - Total and allocated GPUs on the labeled node.
   * - ``spur_node_cpu_load``
     - CPU load reported by the node agent.
   * - ``spur_node_free_memory_bytes``
     - Free memory (bytes) reported by the node agent.

k0s metrics from spurctld — ``/metrics/k8s``
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

When Spur manages a k0s cluster (``spur k8s up``, see
:doc:`/deployment/managed-kubernetes`), ``spurctld`` exports its own view of the
cluster lifecycle and per-node health at ``/metrics/k8s``. Every series carries
``distribution="k0s"`` and ``cluster="<cluster-name>"``.

.. list-table::
   :header-rows: 1
   :widths: 46 54

   * - Metric
     - Description
   * - ``spur_k8s_cluster_phase{phase}``
     - Current cluster phase as a one-hot set (``down``, ``provisioning``,
       ``ready``, ``degraded``); value 1 on the active phase.
   * - ``spur_k8s_cluster_up``
     - 1 when the cluster phase is Ready (primary alerting signal).
   * - ``spur_k8s_control_plane_replicas``
     - Configured control-plane replica count.
   * - ``spur_k8s_nodes_total`` / ``spur_k8s_nodes_by_role{role}``
     - Total nodes with a k0s role, and the per-role count.
   * - ``spur_k8s_provision_attempts_total`` / ``spur_k8s_provision_failures_total``
     - Provisioning attempts and attempts that gave up before Ready.
   * - ``spur_k8s_phase_transitions_total{from,to}``
     - Phase transitions labeled by source and destination.
   * - ``spur_k8s_reconcile_duration_seconds`` / ``spur_k8s_reconcile_errors_total``
     - Reconcile-loop iteration wall time (histogram) and error count.
   * - ``spur_k8s_node_up{node}``
     - Whether a node's k0s systemd unit reports active.
   * - ``spur_k8s_node_restart_total{node}`` / ``spur_k8s_install_duration_seconds{node}``
     - Per-node k0s unit restarts and install time (histogram).

k0s component metrics *(incoming)*
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

.. note::

   The endpoints in this section are **incoming / in progress** and are separate
   from the ``spurctld`` ``/metrics/k8s`` surface above. The bundled k0s
   components each expose their own upstream Kubernetes ``/metrics`` endpoint;
   Spur does not yet aggregate or proxy these — the ports and paths below are the
   standard Kubernetes surfaces documented here for planning a scrape
   configuration.

These endpoints are served by the k0s-managed components, not by ``spurctld``.
Control-plane endpoints live on the control-plane node; the kubelet endpoints
live on every node. Most require TLS and authentication.

.. list-table::
   :header-rows: 1
   :widths: 26 20 20 34

   * - Component
     - Port / Path
     - Status
     - Contents
   * - kube-apiserver
     - ``6443`` ``/metrics``
     - Incoming
     - API request latency/counts, etcd cache, admission timings.
   * - kubelet
     - ``10250`` ``/metrics``
     - Incoming
     - Node agent, pod lifecycle, and volume stats.
   * - kubelet (cAdvisor)
     - ``10250`` ``/metrics/cadvisor``
     - Incoming
     - Per-container CPU/memory/network/disk usage.
   * - kubelet (resource)
     - ``10250`` ``/metrics/resource``
     - Incoming
     - Node/pod CPU and memory for the metrics pipeline.
   * - kube-scheduler
     - ``10259`` ``/metrics``
     - Incoming
     - Scheduling attempts, latency, and queue depth.
   * - kube-controller-manager
     - ``10257`` ``/metrics``
     - Incoming
     - Controller work queues and reconcile timings.
   * - etcd
     - ``2379`` ``/metrics``
     - Incoming
     - Datastore health, latency, and DB size.
   * - CoreDNS
     - ``9153`` ``/metrics``
     - Incoming
     - Cluster DNS query counts, latency, and cache stats.

Controlling Jobs
----------------

**Cancel jobs** with ``spur cancel`` (Slurm ``scancel``), by job ID or by filter:

.. code-block:: bash

   scancel 1024 1025
   spur cancel -u alice -p gpu --state PENDING
   scancel --signal SIGTERM 2048

Filter flags include ``--user``/``-u`` (defaults to the current user in filter
mode), ``--partition``/``-p``, ``--state``/``-t`` (only ``PD`` or ``R``),
``--name``/``-n``, ``--account``/``-A``, and ``--signal``/``-s`` (``KILL``/9,
``TERM``/15, ``INT``/2, and others). You must supply at least job IDs,
``--user``, or ``--name``. In filter mode, jobs already in a terminal state are
silently skipped.

**Change job state** with ``spur control`` (Slurm ``scontrol``):

.. code-block:: bash

   scontrol hold 1024       # prevent from starting
   scontrol release 1024    # allow a held job to start
   scontrol requeue 1024    # return a job to the queue
   scontrol suspend 1024    # SIGSTOP, keep the allocation
   scontrol resume 1024     # SIGCONT

**Update a job or node** with ``scontrol update`` and ``Key=Value`` pairs. Job
updates need ``JobId=`` and accept ``Priority=``, ``TimeLimit=``, ``Partition=``,
``Account=``, ``Comment=``, and ``QOS=``. Node updates need ``NodeName=`` and
accept ``State=``, ``Reason=``, and ``Reconcile=``.

.. code-block:: bash

   scontrol update JobId=1024 TimeLimit=2:00:00 Priority=100
   scontrol update NodeName=node01 State=drain Reason="maintenance"
   scontrol update NodeName=node01 Reconcile=yes

``--controller`` is parsed as a flag wherever it appears, including after the
``key=value`` pairs. A token among those pairs that is not a ``key=value`` pair
is an error naming the token, rather than being silently dropped.

``Reconcile=yes`` asks the node for a fresh account of what it believes it is
running and compares that against the controller's own record, resolving any
difference. This is not a read-only audit. What happens to work the node is
holding that the controller has no record of depends on what the node says
about it:

- still running — cancelled on that node with ``SIGKILL``;
- finished, with its resources not yet handed back — the controller answers
  that it is not accounting for the run, which releases those resources. This
  is the acknowledgement such a run is waiting for, and without it the node
  holds those CPUs, GPUs and memory indefinitely, including across a restart.
  The node keeps a veto it alone can exercise: while a cleanup hook is still
  running under the job, it declines and the next reconcile asks again;
- neither — the node cannot account for the claim and the controller has no
  record of it, so nothing may end it and nothing proves it is over. It is left
  alone and named in the node's reason as ``holding claims the controller has
  no record of``, followed by the job ids. Only an operator can clear the
  underlying condition.

The reason is written only where the controller has no other reason to
overwrite, and is cleared only once a reconcile that could see every claim finds
none left. A node carrying any other reason — an operator's, or one the
controller set for something else such as a missed heartbeat — keeps it, and the
drift is reported in the controller log instead. Naming needs an
attested reconcile: under the default ``open`` node admission a ledger that
arrives with a registration cannot license it, so on those clusters the reason
is written by a reconcile the controller initiated. See
:doc:`/admin-guide/configuration` for how ``admission.mode`` decides that.

The controller already reconciles a node on its own:

- when the node registers;
- when a new controller takes over;
- once an hour, across every node;
- when the node refuses a launch because it is holding something the controller
  cannot explain;
- when the node asks for one on its heartbeat, having found evidence it cannot
  resolve alone. A node that keeps asking is answered at most once a minute.

``Reconcile=yes`` is the way to ask for one immediately — for instance after an
incident, when you want to confirm a node is not still holding resources for a
job that has finished.

The two halves do not degrade together. If the node could not read all of its own
records, it says so, and the controller stops settling jobs the node did not
mention — it cannot tell absence from a gap in the account. It still cancels the
claims the node *did* report that it cannot explain, because a claim the node
named is evidence whether or not the rest of the account is complete. So a node
with a damaged spool is not uniformly left alone: expect cancellations from it,
but no settlements.

Whether a reconcile may cancel and settle, or only report what it found, depends
on ``[admission] mode`` for the registration case; see
:doc:`/admin-guide/configuration`. Every other trigger above acts in either mode.

Asking for one by hand requires admin rights, because it can end running work::

    scontrol update NodeName=node01 Reconcile=yes

The command returns once the pass is done. It reports nothing about what the pass
found — read the controller log, or the node's reason, for that.

The reconcile a node runs as part of registering gates that node: it reports a
reason of ``reconciling with the controller`` and accepts no new work until the
comparison completes. A controller still replaying its own log waits up to ten
seconds for that before comparing anything, and that pass as a whole is capped
at a minute, after which the node is let back in regardless.

``Reconcile=yes`` is not gated that way. The node stays schedulable throughout,
and the command blocks until the comparison finishes rather than returning
immediately.

See Also
--------

- :doc:`submitting-jobs`
- :doc:`/admin-guide/accounting`
