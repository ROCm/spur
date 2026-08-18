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

- **Unset / omitted** — no limit. This is the default for any limit you do not
  name.
- ``-1`` — clears a limit back to "no limit". Use it on ``modify`` to lift a cap.
- ``0`` — a literal value meaning **block all**: a ``maxsubmitjobs=0`` rejects
  every submission for that association. ``0`` does **not** mean "no limit".

``show`` renders an unset limit as a blank cell and a literal ``0`` as ``0``.

Enabling accounting
-------------------

Accounting is configured in the ``[accounting]`` block of ``spur.conf``. It is
disabled until ``database_url`` names a reachable PostgreSQL database.

.. code-block:: toml

   [accounting]
   database_url = "postgresql://spur:spur@localhost/spur"
   default_qos = "normal"
   require_qos = false
   fairshare_refresh_secs = 300

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

``fairshare_refresh_secs``
   How often, in seconds, to refresh the fairshare and QOS caches from the
   database.

   :Default: ``300``

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
     - Preemption behavior when this QOS's jobs are the victim:
       ``cancel`` (job's terminal state is ``CANCELLED``; ``PREEMPTED`` is
       recorded in accounting), ``requeue`` (returned to pending),
       ``suspend``, or ``off`` (not preemptable).
       A job from a higher-``priority`` QOS can preempt a running job from a
       lower-``priority`` QOS when its effective priority exceeds twice the
       victim's.
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
     - Aggregate wall-clock time across all jobs under this QOS.
   * - ``flags``
     - ``""``
     - Comma-separated QOS flags. ``DenyOnLimit`` is supported (see
       :ref:`deny-on-limit`).

.. _deny-on-limit:

How limits are enforced at submit
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

Spur checks limits at submit time, matching Slurm's ``acct_policy_validate``.
Two families behave differently:

- **Submit-count limits** (``maxsubmitjobs``, ``grpsubmit``,
  ``maxsubmitjobsperuser``, ``maxsubmitjobsperaccount``) and the association
  ``maxwall`` always **reject** the submission when breached. A rejected job is
  never queued.
- **Standalone resource limits** (per-job TRES and QOS wall) reject at submit
  only when the governing QOS carries the ``DenyOnLimit`` flag; otherwise the
  job is accepted and **pends** until it fits. A job that resolves to **no QOS**
  is treated as ``DenyOnLimit`` on, so an unsatisfiable request is rejected
  rather than pending forever.

Set ``DenyOnLimit`` through the QOS ``flags`` key:

.. code-block:: bash

   sacctmgr modify qos name=highprio set flags=DenyOnLimit

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
- ``reserved`` jobs are never preempted (``preemptmode=off`` is the default).

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
  association limits. Where a QOS limit is defined, it takes precedence over the
  association's corresponding limit — matching Slurm's rule that QOS limits
  override association limits.

Associations are managed via ``add user`` and inspected via ``sacctmgr show
user`` (see `Managing users`_).

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
(minutes), or ``s`` (seconds), or a bare integer number of seconds. Omitting
``--ttl`` creates a token that never expires.

See Also
--------

- :doc:`configuration`
- :doc:`/user-guide/submitting-jobs`
- :doc:`/deployment/ansible`
