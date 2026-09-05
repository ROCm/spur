Accounting, Accounts, Users, and QOS
====================================

Accounting tracks cluster usage and enforces resource limits. It is enabled by
setting ``[accounting] database_url`` (a PostgreSQL DSN) in ``spur.conf``; when
that key is empty, accounting is off and no limits are applied. All accounting
management goes through ``sacctmgr``, which talks to the controller on port 6817.
Point it at a non-default controller with ``--controller <url>`` or the
``SPUR_CONTROLLER_ADDR`` environment variable (default
``http://localhost:6817``). See :doc:`configuration` for the full
``[accounting]`` block.

.. note::

   ``sacctmgr`` is the Slurm-compatible name and is reachable directly or as
   ``spur accounts``. ``scontrol`` is likewise reachable as ``spur control``.
   This page uses the Slurm-compatible names throughout, since Spur mirrors
   Slurm's accounting model.

Concepts
--------

Spur uses Slurm's accounting model. Learn the entities in dependency order —
each builds on the one before it:

- **Cluster** — the top of the hierarchy. A Spur deployment is one cluster,
  named by ``cluster_name`` in ``spur.conf``. You do not create clusters with
  ``sacctmgr``; the cluster already exists once the controller is running.
- **Account** — a group that owns usage and limits (a project, team, or lab).
  Accounts form a tree: an account may have a **parent** account, and limits and
  fairshare flow down the hierarchy.
- **User** — a person who submits jobs. A user is attached to one or more
  accounts.
- **Association** — the tuple ``(cluster, account, user, partition)`` that binds
  a user to an account. Per-association limits and the QOS allow-list are carried
  on the association. Associations are created **implicitly** by
  ``sacctmgr add user`` — there is no standalone ``add association``.
- **QOS** (Quality of Service) — a named policy with its own priority,
  preemption behavior, and limits. A job runs under exactly one QOS (or none).
  QOS limits layer on top of association limits.
- **Limits / TRES** — the caps themselves. **TRES** (Trackable RESources) are
  the countable quantities — CPU, memory, GPUs, nodes — that limits are
  expressed against.

The term **association** is used throughout this page to mean the
``(cluster, account, user, partition)`` tuple defined above.

.. _limit-values:

Limit values
~~~~~~~~~~~~~

Numeric limits (job counts, wall time) share one convention throughout this
page:

- **Unset / omitted** — on ``add``, any limit you do not name defaults to no
  limit. On ``modify``, an omitted field is left unchanged (partial patch); only
  the fields you name are written. To lift an existing cap on ``modify``, set it
  to ``-1`` explicitly rather than omitting it.
- ``-1`` — clears a limit back to "no limit". Use it on ``modify`` to lift a cap.
- ``0`` — a literal value meaning **block all**: a ``maxsubmitjobs=0`` rejects
  every submission for that association (likewise ``grpsubmitjobs=0`` for the
  whole account). ``maxwall=0`` likewise blocks every job, including one that
  requests no wall time. ``0`` does **not** mean "no limit"; use ``-1`` to lift
  it.

``show`` renders an unset limit as a blank cell and a literal ``0`` as ``0``.

Limit changes are not instant: the controller reads accounting and QOS limits
from a cache that refreshes every ``fairshare_refresh_secs`` (default 300s,
floored at 10s), so a ``sacctmgr modify`` can take up to one refresh interval
to affect new submissions.

Enabling accounting
-------------------

Accounting is configured in the ``[accounting]`` block of ``spur.conf``. It is
disabled until ``database_url`` names a reachable PostgreSQL database.

.. code-block:: toml

   [accounting]
   database_url = "postgresql://spur:spur@localhost/spur"
   default_qos = "normal"
   require_qos = false
   require_association = false
   fairshare_refresh_secs = 300
   grp_wall_window_days = 14

``database_url``
   PostgreSQL DSN for the accounting store. A non-empty value enables accounting
   and serves the ``SlurmAccounting`` API on port 6817. Empty disables accounting
   entirely.

   :Default: ``""`` (accounting off)

``default_qos``
   Cluster-wide fallback QOS applied at submit when a job otherwise resolves to
   no QOS. Mirrors Slurm's fallback QOS (its built-in ``normal``). Must name an
   existing QOS.

   :Default: ``""`` (no fallback)

``require_qos``
   Reject at submit any job that resolves to no QOS. Mirrors Slurm's
   ``AccountingStorageEnforce=qos``.

   :Default: ``false``

``require_association``
   Reject at submit any job whose user resolves to no account — a submission
   with no ``--account`` and no default account on file. This check is
   unconditional, like ``require_qos``: it applies even before accounting has
   finished loading, so enabling it without accounting fully configured
   rejects every such submission. Mirrors Slurm's
   ``AccountingStorageEnforce=associations``. A submission naming an account
   the user is not associated with is rejected regardless of this setting,
   once the association cache has loaded.

   :Default: ``false``

``fairshare_refresh_secs``
   How often, in seconds, to refresh the fairshare and QOS caches from the
   database.

   :Default: ``300``

``grp_wall_window_days``
   Trailing window over which a QOS's wall-clock consumption is measured for
   ``grpwall``. Independent of ``scheduler.fairshare_halflife_days``; see
   `Group wall-clock budgets (GrpWall)`_.

   :Default: ``14``

.. warning::

   A ``default_qos`` that does not name an existing QOS is a hard error at submit
   time. Create the QOS first (see `QOS`_), then set ``default_qos``.

Managing accounts
-----------------

An account groups usage and limits. Create accounts with ``sacctmgr add
account``; the equivalent Slurm command is ``sacctmgr add account``.

Create a top-level account
~~~~~~~~~~~~~~~~~~~~~~~~~~~~

.. code-block:: bash

   sacctmgr add account name=research description="Research group" organization=science fairshare=100

Create a child account (hierarchy)
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

Build the account tree with ``parent=``. An account with no parent renders as a
child of ``root``.

.. code-block:: bash

   sacctmgr add account name=ml parent=research fairshare=50 grptres=gres/gpu=16

Modify an account
~~~~~~~~~~~~~~~~~~

.. code-block:: bash

   sacctmgr modify account name=ml set fairshare=80

Delete an account
~~~~~~~~~~~~~~~~~~

.. code-block:: bash

   sacctmgr delete account name=ml

Show accounts
~~~~~~~~~~~~~~

.. code-block:: bash

   sacctmgr show account

.. code-block:: text

   Account      Descr            Org      Parent  Share  GrpTRES
   research     Research group   science  root    100
   ml                                     research 50    gres/gpu=16

Account keys
~~~~~~~~~~~~~

.. list-table::
   :header-rows: 1
   :widths: 22 20 58

   * - Key
     - Default
     - Meaning
   * - ``name`` (alias ``account``)
     - required
     - Account name.
   * - ``description``
     - ``""``
     - Free-text description.
   * - ``organization``
     - ``""``
     - Organization the account belongs to.
   * - ``parent``
     - ``root``
     - Parent account; builds the account tree.
   * - ``fairshare``
     - ``1.0``
     - Fairshare weight. Shown in the ``Share`` column as an integer.
   * - ``maxrunningjobs`` (alias ``maxjobs``)
     - unset (no limit)
     - Maximum jobs running at once for the account. See :ref:`limit-values`.
   * - ``grptres``
     - ``""``
     - Aggregate TRES cap for the account (see `TRES`_).

.. note::

   ``modify`` sends only the fields you name; every field you omit is preserved
   (re-read from the stored record, not reset). To lift a numeric limit, set it
   to ``-1``; to clear a text field, set it empty (e.g. ``grptres=``).

Managing users
--------------

A user attached to an account **is an association**. Add users with ``sacctmgr
add user``; the equivalent Slurm command is ``sacctmgr add user``. Both ``name``
and ``account`` are required — a user cannot exist without an account.

Add a user with a QOS allow-list
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

``qos=`` is a comma-separated allow-list of QOS the user may request;
``defaultqos=`` is the QOS applied when the user does not name one.

.. code-block:: bash

   sacctmgr add user name=alice account=ml qos=highprio,normal defaultqos=highprio

Set an admin level
~~~~~~~~~~~~~~~~~~~

``adminlevel`` is passed through as given (``Operator``, ``Administrator``, or
``none``).

.. code-block:: bash

   sacctmgr add user name=bob account=research adminlevel=Operator

Set per-association limits
~~~~~~~~~~~~~~~~~~~~~~~~~~~~

.. code-block:: bash

   sacctmgr add user name=carol account=ml maxjobs=4 maxsubmitjobs=8 maxwall=1-00:00:00 grptres=cpu=64

Modify a user
~~~~~~~~~~~~~~

.. code-block:: bash

   sacctmgr modify user name=alice account=ml set maxjobs=6

Delete a user
~~~~~~~~~~~~~~

Omit ``account=`` to remove the user from **all** accounts.

.. code-block:: bash

   sacctmgr delete user name=alice account=ml

Show users
~~~~~~~~~~~

Filter by ``account=`` or ``name=``.

.. code-block:: bash

   sacctmgr show user account=ml

``show user`` also prints the per-association limits (``MaxJobs``,
``MaxSubmit``, ``GrpSubmit``, ``MaxWall``, ``MaxTRES``, ``GrpTRES``), so you can
read back what ``add``/``modify user`` set. An unset limit renders as a blank
cell.

.. code-block:: text

   User    Account  Admin  Default Acct  QOS              Def QOS   MaxJobs  MaxSubmit  GrpSubmit  MaxWall  MaxTRES  GrpTRES
   alice   ml        None   ml            highprio,normal  highprio
   carol   ml        None   ml                                       4        8                     1440     cpu=64

User keys
~~~~~~~~~

.. list-table::
   :header-rows: 1
   :widths: 26 18 56

   * - Key
     - Default
     - Meaning
   * - ``name`` (alias ``user``)
     - required
     - User name.
   * - ``account`` (alias ``defaultaccount``)
     - required
     - Account to attach the user to. Using ``defaultaccount`` marks it as the
       user's default account.
   * - ``adminlevel``
     - ``none``
     - Admin level: ``none``, ``Operator``, or ``Administrator``.
   * - ``defaultqos``
     - ``""``
     - QOS applied when the user does not request one.
   * - ``qos``
     - ``""``
     - Comma-separated allow-list of QOS the user may request.
   * - ``maxrunningjobs`` (alias ``maxjobs``)
     - unset (no limit)
     - Maximum jobs running at once for this association. See
       :ref:`limit-values`.
   * - ``maxsubmitjobs``
     - unset (no limit)
     - Maximum jobs the user may have submitted (pending + running).
   * - ``grpsubmit`` (alias ``grpsubmitjobs``)
     - unset (no limit)
     - Aggregate submitted jobs (pending + running) across the association.
   * - ``maxtresperjob``
     - ``""``
     - TRES cap for a single job (see `TRES`_).
   * - ``grptres``
     - ``""``
     - Aggregate TRES cap across the association's jobs.
   * - ``maxwall`` (alias ``maxwallduration``)
     - unset (no limit)
     - Maximum wall-clock time per job.

.. note::

   If both ``qos=`` and ``defaultqos=`` are given, the default **must** appear in
   the allow-list, or the command is rejected. ``defaultqos`` alone (no ``qos=``
   list) is valid and scopes the user to that single QOS.

.. note::

   On ``modify user``, every field you omit is **preserved**. QOS
   (``qos``/``defaultqos``) and numeric limits alike are re-read from the stored
   association rather than reset. To lift a limit, set it to ``-1``; to clear the
   QOS allow-list, set ``qos=`` empty.

QOS
---

A QOS (Quality of Service) is a named policy with its own priority, preemption
mode, usage factor, and limits. Manage QOS with ``sacctmgr add qos``; the
equivalent Slurm command is ``sacctmgr add qos``. Every limit defaults to unset
(no limit); ``0`` means block all and ``-1`` clears a limit (see
:ref:`limit-values`).

Create a high-priority QOS
~~~~~~~~~~~~~~~~~~~~~~~~~~~~

.. code-block:: bash

   sacctmgr add qos name=highprio priority=100 maxjobsperuser=8 maxwall=1-00:00:00 \
     maxtresperjob=node=2,cpu=64 grptres=gres/gpu=8 preemptmode=cancel

Modify a QOS
~~~~~~~~~~~~

.. code-block:: bash

   sacctmgr modify qos name=highprio set priority=120 usagefactor=1.5

Delete a QOS
~~~~~~~~~~~~

.. code-block:: bash

   sacctmgr delete qos name=highprio

Show QOS
~~~~~~~~

``show qos`` accepts a ``format=`` list to select and order columns. When no QOS
exists and no name filter is given, Spur prints a synthetic ``normal`` QOS
(preemption off, usage factor 1.0).

.. code-block:: bash

   sacctmgr show qos
   sacctmgr show qos name=highprio
   sacctmgr show qos format=Name,Priority,GrpTRES,MaxTRES,MaxTRESPU,MaxJobsPU,MaxWall

QOS keys
~~~~~~~~

.. list-table::
   :header-rows: 1
   :widths: 30 18 52

   * - Key
     - Default
     - Meaning
   * - ``name`` (alias ``qos``)
     - required
     - QOS name.
   * - ``description``
     - ``""``
     - Free-text description.
   * - ``priority``
     - ``0``
     - Base priority seed for jobs submitted under this QOS. When a job is
       submitted without an explicit ``--priority``, this value becomes the
       job's base priority and is amplified by the multiplicative scheduling
       formula (fair-share × age × partition tier). Higher values run sooner
       and are more likely to preempt lower-priority running jobs. ``0``
       leaves the job at the scheduler default (1000).
   * - ``preemptmode``
     - ``off``
     - What happens to a job in this QOS when it gets kicked out by a
       higher-priority job. This setting overrides whatever the partition says,
       but only for *how* the job is removed — it does not control *whether*
       preemption happens (that depends on the partition's ``preempt_mode``
       and the priority gap).

       ``cancel`` — the job is stopped and removed from the queue. Its final
       state is ``CANCELLED`` (``PREEMPTED`` in accounting records).
       ``requeue`` — the job is stopped and put back in the queue. It will
       start again automatically once a slot is free.
       ``suspend`` — the job is paused, keeping its node allocation. It
       resumes automatically once the higher-priority job finishes.
       ``off`` (default) — no change from what the partition says. Setting
       ``preemptmode=off`` on a QOS is the same as leaving it unset. It does
       **not** protect the job from being preempted.

       **Example of the override:** a partition is set to ``cancel`` but a
       specific QOS is set to ``preemptmode=requeue``. When a job in that QOS
       is kicked out, it goes back to the queue instead of being cancelled.

       **How to actually protect a QOS from preemption:** simply do not add it
       to any other QOS's ``preempt`` allow-list. When
       ``preempt_type = "qos_priority"`` is enabled (see :doc:`configuration`),
       a QOS that nobody has permission to preempt will never lose its running
       jobs, no matter how large the priority gap is.
   * - ``preempt``
     - ``""`` (no restriction)
     - Comma-separated list of QOS names that jobs in this QOS are allowed to
       preempt. Only enforced when ``scheduler.preempt_type = "qos_priority"``
       (see :doc:`configuration`). An empty value means this QOS may not preempt
       any other QOS under that mode. Example: ``preempt=low,batch`` allows jobs
       in this QOS to preempt ``low`` and ``batch`` jobs. To clear the list:
       ``sacctmgr modify qos name=<name> set preempt=`` (empty value).
   * - ``preemptexempttime``
     - unset (inherits partition / global)
     - Per-QOS override for the minimum seconds a job must have been running
       before it is eligible for preemption. Overrides the partition-level
       ``preempt_exempt_time`` and the global ``scheduler.preempt_exempt_time``
       (see :doc:`configuration`). ``0`` means immediately preemptable (no
       exemption). To revert to inheriting from the partition or global default:
       ``sacctmgr modify qos name=<name> set clearpreemptexempttime=1``.
   * - ``usagefactor``
     - ``1.0``
     - Multiplier applied to usage charged under this QOS.
   * - ``maxjobsperuser`` (alias ``maxjobspu``)
     - unset (no limit)
     - Maximum running jobs per user under this QOS. See :ref:`limit-values`.
   * - ``maxwall``
     - unset (no limit)
     - Maximum wall-clock time per job.
   * - ``maxtresperjob``
     - ``""``
     - TRES cap for a single job (see `TRES`_).
   * - ``maxsubmitjobsperuser``
     - unset (no limit)
     - Maximum submitted jobs (pending + running) per user.
   * - ``maxsubmitjobsperaccount`` (aliases ``maxsubmitpa``, ``maxsubmitjobspa``)
     - unset (no limit)
     - Maximum submitted jobs (pending + running) per account.
   * - ``grpsubmit`` (alias ``grpsubmitjobs``)
     - unset (no limit)
     - Aggregate submitted jobs (pending + running) across all jobs under this
       QOS.
   * - ``maxtresperuser``
     - ``""``
     - TRES cap across all of one user's jobs under this QOS.
   * - ``grptres``
     - ``""``
     - Aggregate TRES cap across all jobs under this QOS.
   * - ``grpwall``
     - unset (no limit)
     - Aggregate wall-clock budget across all jobs under this QOS. A bare integer
       is minutes; the Slurm time formats ``hh:mm``, ``hh:mm:ss``, ``d-hh:mm`` and
       ``d-hh:mm:ss`` are also accepted (seconds are truncated), so ``grpwall=600``
       and ``grpwall=10:00`` both mean ten hours. See
       `Group wall-clock budgets (GrpWall)`_ for how consumption is measured and
       where the behaviour departs from Slurm.
   * - ``flags``
     - ``""``
     - Comma-separated QOS flags. ``DenyOnLimit`` is supported (see
       :ref:`deny-on-limit`).

.. _deny-on-limit:

How limits are enforced at submit
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

Spur checks limits at submit time, matching Slurm's ``acct_policy_validate``.
Three families behave differently:

- **Submit-count limits** (``maxsubmitjobs``, ``grpsubmit``,
  ``maxsubmitjobsperuser``, ``maxsubmitjobsperaccount``) and the association
  ``maxwall`` always **reject** the submission when breached. A rejected job is
  never queued. These count pending **and** running jobs.
- **Standalone resource limits** (per-job TRES on both the QOS **and** the
  association, plus QOS wall) reject at submit only when the governing QOS
  carries the ``DenyOnLimit`` flag; otherwise the job is accepted and **pends**
  until it fits. This matches Slurm, where ``DenyOnLimit`` applies to QOS and
  association limits alike, so a permissive QOS (``DenyOnLimit`` off) pends an
  association TRES breach instead of rejecting it. A job that resolves to **no
  QOS** is treated as ``DenyOnLimit`` on, so a request that can never fit is
  rejected rather than pending forever.
- **Running-count limits** (``maxjobs``, ``maxjobsperuser``) never reject at
  submit. They count only **running** jobs, so the submit is always accepted and
  the job **pends** once the running cap is reached.

When the submit gate rejects a job, the denial carries the canonical Slurm
reason code alongside a human-readable sentence. The codes the gate can emit
are: ``AssocMaxSubmitJobLimit``, ``AssocGrpSubmitJobsLimit``,
``AssocMaxWallDurationPerJobLimit``, ``QOSMaxSubmitJobPerUserLimit``,
``MaxSubmitJobsPerAccount``, ``QOSGrpSubmitJobsLimit``,
``QOSMaxWallDurationPerJobLimit``, and the QOS per-job TRES codes
(e.g. ``QOSMaxCpuPerJobLimit``, ``QOSMaxNodePerJobLimit``, ``QOSMaxMemoryPerJob``,
``QOSMaxGRESPerJob``). Running-count reasons (``AssocMaxJobsLimit``,
``QOSMaxJobsPerUserLimit``) never appear at submit; they surface as pending
reasons instead.

Set ``DenyOnLimit`` through the QOS ``flags`` key:

.. code-block:: bash

   sacctmgr modify qos name=highprio set flags=DenyOnLimit

Group wall-clock budgets (GrpWall)
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

``grpwall`` caps the total wall-clock time a QOS may consume. Once consumption
reaches the cap, jobs in that QOS stop being scheduled and wait with reason
``QOSGrpWallLimit`` (visible in ``squeue``); they become eligible again once
consumption has fallen back below the cap *and* the next refresh has read it, not
at the moment the older usage ages out of the window.

Consumption is summed from job history over a trailing window, set by
``grp_wall_window_days`` under ``[accounting]`` (default ``14``). Running jobs
contribute the time they have accrued so far, and a job that began before the
window contributes only the part inside it. The figure is refreshed on the same
interval as the other accounting caches, ``fairshare_refresh_secs``.

The window is deliberately independent of ``scheduler.fairshare_halflife_days``.
The half-life fades old usage for priority scoring, a soft curve that never
reaches zero; this window is a hard cutoff on a budget. Changing one must not
move the other, so a cluster can pair, say, a seven-day half-life for responsive
priority with a thirty-day window for monthly budget enforcement.

Three deliberate differences from Slurm:

* **Running jobs are never killed.** Slurm cancels running jobs when the limit is
  reached; Spur only stops admitting new ones. Work already under way finishes.
* **Consumption uses a rolling window, not decayed usage.** Slurm decays usage by
  ``PriorityDecayHalfLife``/``PriorityUsageResetPeriod``, which Spur has no
  equivalent of. A trailing window is a hard budget with no decay curve.
* **The limit applies to a QOS only.** Slurm also accepts ``GrpWall`` on an
  association; Spur does not store it there, so it cannot be set or enforced per
  account.

Four more behaviours worth knowing before relying on a budget:

* **Admission can overshoot between refreshes.** The scheduler admits on every
  pass but re-reads consumption only on the refresh interval, so with a QOS
  sitting just under its cap a large batch can start before the next read. Size
  the cap with that in mind, or shorten ``fairshare_refresh_secs``.
* **A requeue discards the job's earlier run.** Restarting a job rewrites its
  history row with the new start time and clears the end time, so the first run
  drops out of the budget entirely.
* **A job whose end is never recorded keeps accruing.** If a job's completion
  never reaches the database and reconciliation cannot repair the row, it counts
  as still running and contributes the full window from then on, which can hold a
  QOS below its cap indefinitely. ``sacct`` showing a job with no end time is the
  symptom.
* **Spend is keyed on the QOS name and cannot be reset.** Deleting and recreating
  a QOS under the same name inherits up to ``grp_wall_window_days`` of the old
  one's consumption, and there is no administrative override. The only ways out
  are a new name or waiting for the window to pass.

Enforcement needs a consumption figure, and until one has been read there is
none: with accounting disabled the budget is never applied, and scheduling
continues. Once a figure has been read, a later refresh failure leaves the last
one in place rather than discarding it, exactly as the QOS cache retains its
definitions, so a budget keeps applying across an outage — but on the last figure
read, however old that now is, and consumption accrued during the outage is
invisible until the database returns.

If the database is unreachable when the controller *starts*, the refresh loop is
never started at all: the budget is not applied for the life of that process and
does not begin applying when PostgreSQL comes back. Restart the controller once
the database is reachable. Where a budget must hold precisely, keep the accounting
database available.

.. note::

   Consumption is attributed per QOS from the QOS recorded on each job, which
   earlier releases did not store. Jobs that ran before the upgrade therefore
   count for nothing and cannot be backfilled, so a budget set immediately after
   upgrading starts from zero and fills over the following
   ``grp_wall_window_days``.

QOS preemption hierarchy
~~~~~~~~~~~~~~~~~~~~~~~~

By default, Spur's preemption eligibility is based purely on the numeric
priority gap: any job with an effective priority more than 2× the running
job's may preempt it (subject to the partition's ``preempt_mode`` gate).

To restrict which QOS tiers may preempt which, enable the QOS preemption
hierarchy in ``spur.conf``:

.. code-block:: toml

   [scheduler]
   preempt_type = "qos_priority"

With ``preempt_type = "qos_priority"``, a job may only preempt a running job
when the pending job's QOS lists the running job's QOS name in its ``preempt``
allow-list. An empty allow-list means the QOS may not preempt anything.

**Example: two-tier system**

.. code-block:: bash

   # "burst" pool — high-throughput, low-priority, immediately preemptable.
   sacctmgr add qos name=burst priority=100 preemptmode=cancel

   # "priority" pool — reserved capacity; may preempt burst jobs only.
   sacctmgr add qos name=priority priority=10000 preempt=burst

   # "reserved" pool — guaranteed; cannot be preempted by anyone,
   # and cannot preempt anyone (no preemptmode, no preempt list).
   sacctmgr add qos name=reserved priority=50000

With this setup:

- A ``priority`` job preempts ``burst`` jobs when the priority gap is large enough.
- A ``priority`` job cannot preempt ``reserved`` jobs (``reserved`` is not in
  ``priority``'s allow-list).
- A ``burst`` job never preempts anyone (empty allow-list).
- ``reserved`` jobs are never kicked out because no other QOS lists
  ``reserved`` in its ``preempt`` field. That is what provides the protection
  — not ``preemptmode=off``, which simply means "use the partition default".

**Minimum exempt time**

To protect recently-started jobs from being immediately evicted, set
``preempt_exempt_time`` at the global, partition, or QOS level. Resolution
order: QOS > partition > global.

.. code-block:: bash

   # Global: no job can be preempted in its first 5 minutes.
   # (Set in spur.conf: scheduler.preempt_exempt_time = 300)

   # Per-QOS: burst jobs are protected for 2 minutes regardless of global.
   sacctmgr modify qos name=burst set preemptexempttime=120

   # Per-partition via scontrol (runtime, no restart needed):
   scontrol update PartitionName=gpu PreemptExemptTime=600

   # Clear a per-QOS override (revert to partition/global):
   sacctmgr modify qos name=burst set clearpreemptexempttime=1

   # Clear a per-partition override (revert to global):
   scontrol update PartitionName=gpu ClearPreemptExemptTime=yes

**Burst QOS pattern — overflow capacity**

A common design is to give users two pools: a *normal* pool with guaranteed
capacity, and a *burst* pool they can use for extra work when the cluster has
free nodes. Burst jobs run opportunistically and are kicked out as soon as a
normal job needs the slot.

The burst QOS is not a special feature — it is just a QOS with a very low
priority (so normal jobs always outrank it) that normal-pool QOSes list in
their ``preempt`` allow-list (so they are allowed to kick it out).

.. code-block:: bash

   # In spur.conf:
   [scheduler]
   preempt_type = "qos_priority"

   # Burst pool: very low priority; requeue so burst jobs go back to the
   # queue instead of being lost when kicked out.
   sacctmgr add qos name=burst priority=-5000 preemptmode=requeue

   # Normal pool: allowed to kick out burst jobs when it needs a slot.
   sacctmgr add qos name=normal priority=0 preempt=burst

With this setup:

- Users submit overflow work with ``--qos=burst``. Those jobs run on any
  free nodes.
- When a ``normal`` job arrives and a ``burst`` job holds the only available
  node, the ``burst`` job is sent back to the queue and the ``normal`` job
  starts. The burst job will restart automatically once a node is free again.
- ``burst`` jobs can never kick out ``normal`` jobs — ``burst`` has no
  entries in its ``preempt`` allow-list.
- A QOS that no other QOS lists in its ``preempt`` field (for example, a
  ``reserved`` tier) will never have its jobs kicked out, regardless of how
  large a priority gap exists.

How a job's QOS is resolved
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

At submit, Spur resolves the job's QOS in this order, matching Slurm:

1. **Explicit** ``--qos`` on the job. The named QOS must exist **and** be
   permitted for the association (present in the association's allow-list or its
   default). Otherwise the job is rejected with ``QOS 'X' does not exist`` or
   ``QOS 'X' is not permitted for user '…' under account '…'``.
2. Otherwise the **association's default QOS**, if it still exists. A stale
   default (deleted QOS) is ignored with a warning.
3. Otherwise the cluster fallback **``accounting.default_qos``**. A configured
   fallback that exists but is not permitted for the association is ignored with
   a warning.
4. Otherwise, if ``accounting.require_qos`` is ``true``, the job is **rejected**
   (``no QOS specified and no default QOS is configured for this user/account``).
   If ``require_qos`` is ``false``, the job runs with no QOS.

.. note::

   A configured ``accounting.default_qos`` that does **not** name an existing QOS
   is a hard error, not a silent fallback. Create the QOS before referencing it.

Associations
------------

An association is the ``(cluster, account, user, partition)`` tuple. Associations
are created implicitly by ``sacctmgr add user`` — there is no standalone ``add
association`` command. Inspect them through ``sacctmgr show user``, which lists
each user's account, default account, QOS, and per-association limits.

Limits resolve in two layers:

- **Association limits.** The per-association caps set on the user
  (``maxjobs``, ``maxsubmitjobs``, ``grpsubmit``, ``maxtresperjob``,
  ``grptres``, ``maxwall``) and the association's QOS allow-list together gate
  whether a submit is accepted.
- **QOS limits.** The limits on the job's resolved QOS layer on top of the
  association limits. Both sets are enforced: the submit gate checks the
  association's submit-count and per-job wall limits first, then the QOS limits,
  and rejects on the first breach it finds. A job must satisfy both the
  association and the QOS to be accepted, so a denial may carry an ``Assoc*``
  reason even when a QOS limit also applies.

Associations are managed via ``add user`` and inspected via ``sacctmgr show
user`` (see `Managing users`_).

How a job's account is resolved
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

At submit, Spur resolves the job's account in this order, matching Slurm.
This validation is skipped (fail-open) while the association cache has not
yet loaded, e.g. at controller startup or while the accounting database is
unreachable:

1. **Explicit** ``--account`` on the job. Once the association cache has
   loaded, it must name an account the user is associated with, or the job
   is rejected — with ``user 'X' is not associated with account 'Y'`` if the
   user has other associations, or ``user 'X' has no account associations``
   if the user has none at all.
2. Otherwise the **user's default account**, if the user has one on file.
3. Otherwise, if ``accounting.require_association`` is ``true``, the job is
   **rejected** (``no account resolved for user 'X'``) — even if the user
   does have associations, just none flagged as their default. This step is
   unconditional, unlike step 1: it applies even before the association
   cache has loaded. If ``require_association`` is ``false`` (the default),
   the job runs with no account.

TRES
----

TRES (Trackable RESources) are the countable quantities that limits are
expressed against. Spur ships a fixed built-in TRES table; there is no TRES CRUD.
List them with ``sacctmgr show tres``.

.. list-table::
   :header-rows: 1
   :widths: 16 24 60

   * - ID
     - Type
     - Meaning
   * - 1
     - ``cpu``
     - CPU cores.
   * - 2
     - ``mem``
     - Memory (MB).
   * - 3
     - ``energy``
     - Energy consumed.
   * - 4
     - ``node``
     - Whole nodes.
   * - 1001
     - ``gres/gpu``
     - GPUs.
   * - 1002
     - ``billing``
     - Billing units.

TRES strings appear in the ``grptres``, ``maxtresperjob``, and ``maxtresperuser``
values, comma-separated:

.. code-block:: text

   grptres=cpu=16,mem=32768,gres/gpu=8
   maxtresperjob=node=2,cpu=64

The three TRES caps differ by scope:

- **GrpTRES** (``grptres``) caps **aggregate** usage across all of an entity's
  jobs at once.
- **MaxTRESPerJob** (``maxtresperjob``) caps a **single job**.
- **MaxTRESPerUser** (``maxtresperuser``) caps a **single user's** total across
  their jobs.

.. _grptres-node-packing:

GrpTRES node= counts distinct nodes, not per-job node requests
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

``grptres=node=N`` caps the number of **distinct physical nodes** a QOS or
account may occupy at once — not the sum of each running job's requested node
count. Two running jobs that share a node count that node once; jobs on
disjoint nodes each count their own. A new job is admitted if it can be
satisfied without pushing the group's distinct-node count over the cap,
whether by using brand-new nodes or by packing onto nodes the group already
occupies that still have free CPU/memory/GPU capacity — so a group is never
blocked from all further scheduling just because its existing jobs happen to
use only part of each node's resources. Placement honors this: when a job is
admitted on the basis that it can pack onto specific already-occupied nodes,
scheduling actually places it there (as if those nodes had been given as an
additive ``--nodelist``), rather than spreading it to an idle node elsewhere
in the cluster and silently exceeding the cap. ``MaxTRESPerJob``'s and
``MaxTRESPerUser``'s own ``node=`` caps are unaffected by this and always use
a job's actual requested node count, since they bound a single job's or
user's own footprint rather than group-wide capacity reuse.

Managing nodes at runtime
-------------------------

Change a node's state at runtime with ``scontrol update``; the equivalent Slurm
command is ``scontrol update NodeName=…``.

.. code-block:: bash

   scontrol update nodename=gpu001 state=drain reason="maintenance"
   scontrol update nodename=gpu001 state=resume
   scontrol update nodename=gpu001 state=down reason="hw fault"

Accepted node states are ``drain`` (finish running jobs, accept no new ones),
``resume`` (or ``idle`` — return to service), and ``down`` (mark unavailable). An
unrecognized state defaults to idle with a warning.

.. note::

   Partitions are **static config**, not runtime objects. Unlike Slurm's
   ``scontrol create partition``, Spur has no runtime partition create, delete,
   or modify. To change a partition, edit ``[[partitions]]`` in ``spur.conf`` and
   reload the controller. See :doc:`/deployment/partitioning` and
   :doc:`configuration`.

Auditing administrative actions
-------------------------------

Reservation admin commands (``scontrol create/update/delete-reservation``) are
recorded in the accounting database's ``txn`` (transaction) log, capturing
**who** ran the command, **when**, and the **outcome**. This closes a gap in
stock Slurm, whose ``txn_table`` does not cover ``scontrol`` reservation
operations and whose reservation records carry no actor. Recording is
best-effort: a database outage never blocks the reservation operation itself.

Each record captures:

- **Time** — when the action was attempted.
- **Actor** — the requesting user. Under ``auth.mode = required`` this is the
  JWT-verified identity; under the default ``permissive`` mode an unauthenticated
  caller's name is trusted on the wire (see ``Verified``).
- **Verified** — ``yes`` only when a JWT identity was cryptographically verified;
  ``no`` for permissive/disabled anonymous callers (asserted, trust-on-wire) and
  for internal ``system`` actions such as the expired-reservation purge.
- **Action** — ``create``, ``update``, or ``delete``.
- **Where** — the target, rendered ``entity_type:entity_name`` (e.g.
  ``reservation:daily``).
- **Outcome** — ``success``, ``denied`` (permission/ownership rejected), or
  ``error`` (validation or other failure). Unlike Slurm, which logs only
  committed transactions, Spur also records denied and failed attempts.
- **Info** — a JSON payload of the requested parameters (and the error message on
  failure). These are the values as requested, before server-side normalization.
- **Source** — ``api`` for external RPC/CLI callers, ``system`` for internal
  maintenance.

Viewing the log
~~~~~~~~~~~~~~~~

List records with ``sacctmgr show txn`` (aliases: ``transaction``,
``transactions``), modeled on Slurm's ``sacctmgr show transaction``:

.. code-block:: bash

   sacctmgr show txn
   sacctmgr show txn Actor=alice Action=delete
   sacctmgr show txn Entity=reservation Name=daily Outcome=denied
   sacctmgr show txn Start=2026-01-01 End=now-1hours
   sacctmgr show txn format=Time,Actor,Action,Where,Outcome,Verified,Info

Filters are ``Actor=``, ``Action=``, ``Entity=`` (entity type), ``Name=`` (entity
name), ``Outcome=``, ``Start=``, ``End=``, and ``limit=``. ``Start``/``End``
accept the same formats as ``sacct`` (``YYYY-MM-DD``, ISO datetime,
``now-Ndays``/``now-Nhours``). ``limit=`` defaults to 1000 and is capped at
10000 rows per query (larger requests are clamped). The default columns match
Slurm (``Time,Action,Actor,Where,Info``); additional ``format=`` fields are
``Outcome``, ``Verified``, ``Source``, ``ID``, and ``ActorUID``.

.. note::

   Reads are **not** access-gated — the same as ``sacct`` job history and the
   rest of the accounting service. Confidentiality of the audit log therefore
   requires ``auth.mode = required``; under the default ``permissive`` mode any
   caller that can reach the controller can read it.

Retention
~~~~~~~~~

The log grows without bound by default. Set ``accounting.txn_retention_days`` to
have the controller (leader) periodically delete records older than the given
number of days:

.. code-block:: toml

   [accounting]
   txn_retention_days = 365

Leaving it unset (the default) or ``0`` keeps records forever, matching Slurm's
default purge-off behavior; a positive value enables the purge.

Declarative management with Ansible
-----------------------------------

The Spur toolkit ships an Ansible role, ``spur_accounting_mgmt`` (playbook
``manage_accounts.yml``), that applies accounting entities declaratively. Because
``sacctmgr add`` upserts, the role is idempotent across re-runs. It requires
``spur_accounting_enabled=true`` and reads three list variables —
``spur_qos``, ``spur_accounts``, and ``spur_users`` — plus their ``_absent``
counterparts for removals.

Entities are applied in dependency order — **qos → accounts → users** — and
removed in reverse — **users → accounts → qos**.

.. code-block:: yaml

   spur_qos:
     - { name: high, priority: 100, maxwall: 1440 }
   spur_accounts:
     - { name: research, description: "Research group", fairshare: 100 }
     - { name: ml, parent: research, fairshare: 50 }
   spur_users:
     - { name: alice, account: ml, defaultaccount: ml }
     - { name: bob, account: research, adminlevel: Operator }

See :doc:`/deployment/ansible` for running the toolkit playbooks.

Authentication and admission tokens
-----------------------------------

User authentication
~~~~~~~~~~~~~~~~~~~~~

The ``[auth] plugin`` field selects how the controller establishes a caller's
identity:

``none``
   Trust the caller's local UNIX identity — the ``whoami`` user with its real UID
   and GID. A caller with UID 0 is treated as admin. No cryptographic
   verification is performed.

``jwt``
   Require a signed JSON Web Token. Tokens carry the subject, UID, admin flag, and
   expiry, and are signed with ``jwt_key`` (HS256). Expired tokens are rejected as
   ``token expired``; malformed ones as ``invalid token``.

.. code-block:: toml

   [auth]
   plugin = "jwt"
   jwt_key = "/etc/spur/jwt.key"

``jwt_key`` is the signing secret, given as a file path or inline value. Only
``none`` and ``jwt`` are supported.

Admission tokens for node join
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

Admission tokens gate which nodes may register with the controller, distinct from
user authentication. They are enforced only when ``[admission] mode`` is
``token``; the default ``open`` lets any node register.

.. code-block:: toml

   [admission]
   mode = "token"

Manage tokens with ``spur token``, which talks to the controller on port 6817:

.. code-block:: bash

   spur token create --ttl 24h
   spur token list
   spur token revoke <token_id>

``spur token create`` prints the token to stdout. ``spur token list`` shows each
token's ID, creation time, expiry, and status (``active`` or ``revoked``).

``--ttl`` accepts a duration suffixed with ``d`` (days), ``h`` (hours), ``m``
(minutes), or ``s`` (seconds), or a bare integer number of seconds. The duration
must be positive: ``--ttl 0`` (or ``0d``, ``0h``, ``0s``) is rejected rather than
read as "no expiry". Omitting ``--ttl`` creates a token that never expires.

See Also
--------

- :doc:`configuration`
- :doc:`/user-guide/submitting-jobs`
- :doc:`/deployment/ansible`
