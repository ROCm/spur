Migrating from Slurm
====================

Spur is drop-in compatible with Slurm's command-line interface, REST API, and C
FFI. Most Slurm workloads move over unchanged. This page covers what works as-is,
where the configuration model differs, and the behavioral differences a Slurm user
should know about before switching.

What Works Unchanged
--------------------

The following work without modification:

- **Commands** — ``sbatch``, ``srun``, ``salloc``, ``squeue``, ``sinfo``,
  ``sacct``, ``scancel``, ``scontrol``, and ``sacctmgr`` (via the symlinks
  described in :doc:`/user-guide/slurm-compatibility`).
- **Job script directives** — ``#SBATCH`` directives in batch scripts are parsed
  the same way. ``#PBS`` directives are also converted on a best-effort basis.
- **Environment variables** — Spur sets a ``SLURM_*`` twin for every ``SPUR_*``
  variable it injects into a job, so Slurm-aware software (MPI launchers, training
  frameworks) sees the ``SLURM_*`` names it expects.
- **REST API and C FFI** — the REST surface and the FFI library remain
  Slurm-compatible.

Configuration Differences
-------------------------

Slurm's ``slurm.conf`` and ``slurmdbd.conf`` are replaced by a single TOML file,
``spur.conf`` (default location ``/etc/spur/spur.conf``). Key differences:

- **One config file, one set of daemons.** There is no ``slurmdbd`` and no
  ``slurmrestd`` — accounting and the REST API are served by the controller
  (``spurctld``). Accounting storage is configured under ``[accounting]`` in
  ``spur.conf`` rather than in a separate ``slurmdbd.conf``.
- **Raft-based state.** Controller state is replicated through Raft and persists
  across restarts, so there is no ``StateSaveLocation`` handling to manage
  separately.
- **Built-in high availability.** HA is provided by Raft (openraft) and is always
  on — even a single controller runs a one-member cluster. There is no separate
  backup-controller configuration; add controllers to the Raft cluster instead.

See :doc:`/admin-guide/configuration` for the full ``spur.conf`` reference.

Behavioral Differences and Current Limitations
----------------------------------------------

The commands below accept Slurm syntax, but the following differences apply. Flags
noted as "accepted for compatibility" parse without error but have no effect yet.

.. list-table::
   :header-rows: 1
   :widths: 30 70

   * - Area
     - Difference
   * - Partitions
     - ``scontrol create/update/delete PartitionName=`` work at runtime and persist across restarts, but ``spur.conf`` is never rewritten: the next ``scontrol reconfigure`` resets partitions to the file, deleting runtime-only ones that have no active jobs. See :ref:`runtime-partitions`.
   * - Node deletion
     - ``scontrol delete NodeName=<hostlist|ALL>`` uses the same removal RPC as ``spur node remove``, retaining its existing checks. Entity keys accept Slurm's case-insensitive prefixes of at least three characters and either ``key=value`` or ``key value`` syntax, including ``delete node n1``. Named Slurm ``NodeSet`` groups are not expanded. Slurm limits deletion to dynamic nodes without running jobs or reservation membership; Spur uses its native registered-node removal rules and does not guard reservation membership. Removal does not stop an agent from registering again or edit ``spur.conf``. See :doc:`/admin-guide/accounting`.
   * - ``--noconvert``
     - Accepted for compatibility on ``squeue``, ``sinfo``, ``sacct``, ``sstat``, ``sshare``, and ``sreport``. Spur already reports raw values where applicable (``sinfo``'s memory columns are MB integers; ``squeue`` has no memory column yet), so there is nothing to suppress. ``sstat`` still prints ``MemAlloc`` with an ``M`` suffix whether or not the flag is passed.
   * - ``sacct`` ``ReqMem``
     - The ``ReqMem`` format field has no value yet and renders empty.
   * - ``sstat``
     - Per-process ``Ave*`` and ``Max*`` metrics (for example ``AveRSS``, ``MaxVMSize``) show ``N/A``.
   * - ``sprio``
     - The ``FAIRSHARE`` column is a placeholder and does not yet reflect usage.
   * - ``sacctmgr`` ``-p``/``-P``
     - Delimited output covers ``show account``, ``show qos``, and ``show txn``; for ``show user``, ``show association``, and ``show tres`` it errors instead of printing a table no script can parse. ``-n`` works for every entity.
   * - ``scancel`` arrays
     - Array-element syntax (``123_4``) is not yet supported client-side; cancel by plain job id.
   * - ``scontrol show``
     - Output is always the multi-line ``Key=Value`` block; there is no ``--oneliner``. Unset diagnostic fields print as ``(null)`` rather than being omitted — notably ``NodeList=(null)`` on a pending job, where earlier Spur releases omitted the line (a job whose launch is still being confirmed can briefly show its reserved nodes instead). Note that ``ReqNodeList``, ``ExcNodeList``, and ``SchedNodeList`` all contain the literal ``NodeList=``, and ``ReqNodeList`` is printed first: a script matching ``NodeList=`` without anchoring the field name will now read the requested list instead of the allocated one. Match ``^\s*NodeList=`` (or an equivalent boundary) instead. ``ReqTRES`` omits Slurm's ``billing=`` component, as Spur computes no TRES billing weights. ``MinCPUsNode`` is the per-node CPU floor implied by the task layout, which can read higher than Slurm's ``pn_min_cpus`` for an uneven split such as ``-N2 -n7``. A pending job now reports the scheduler's projected ``StartTime`` (and an ``EndTime`` derived from it) where earlier releases printed ``N/A``, so a script treating ``StartTime != N/A`` as "the job has started" needs to check ``JobState`` instead.
   * - ``-S`` / ``--core-spec``
     - Not supported on ``sbatch``, ``srun`` or ``salloc``; the flag is rejected as an unknown argument. Cores for node services are reserved per node in ``spur.conf`` (``core_spec_count`` / ``cpu_spec_list``), not per job.
   * - ``SlurmdOffSpec``
     - Inverted default. Slurm runs its daemons on the specialized cores unless ``SlurmdOffSpec`` is set; Spur keeps ``spurd`` off them by default (``daemons_off_spec = true``).
   * - ``scontrol show hostnames`` / ``hostlist`` / ``hostlistsorted``
     - Supported as local transforms (no controller call). ``hostnames`` falls back to ``SLURM_JOB_NODELIST`` (or ``SPUR_JOB_NODELIST``) when given no argument. ``hostlist`` and ``hostlistsorted`` both remove duplicates and sort, so they emit the same output; Slurm keeps input order for ``hostlist``. Reading a hostlist from a file path argument is not supported.
   * - ``scontrol show assoc_mgr``
     - Caps print as ``Limit(Consumed)`` with ``N`` for no limit, as in Slurm, and an ``OverLimit`` field names caps already exceeded. Filters to one user (``users=<name>`` or a bare name); Slurm's ``accounts=``, ``qos=`` and ``flags=`` selectors are not accepted (an unknown ``<key>=`` errors rather than being read as a username). By default every caller sees every scope; ``[auth] private_data = ["usage"]`` scopes an unprivileged caller to their own (see :ref:`private-data`). Users are listed on their own lines under a scope rather than nested in a ``User Limits=`` field, and an account's group figures are stated once for the account instead of repeated per association row. Fairshare and priority internals Slurm prints (``SharesRaw``, ``UsageRaw``, ``Lft-Rgt``, ``GrpTRESMins``) are not included.
   * - ``sshare``/``sreport`` usage units
     - ``sshare`` ``RawUsage`` and ``CPURawUsage`` report cpu-seconds. ``sshare -l`` now includes a ``GPURawUsage`` column showing gpu-seconds; GPU time is folded into the fair-share calculation at a 1:1 ratio with CPU time (no per-resource billing weight). The QOS ``usagefactor`` multiplies both CPU and GPU usage. Prior Spur releases displayed cpu-hours under the same column names. ``sreport`` columns are likewise cpu-seconds. Scripts parsing ``sshare --parsable`` output will see values 3600× larger than before. Upgrade ``spurctld`` and ``spur-cli`` together to avoid a mixed-unit window. Note: Spur's fair-share factor ranges from 0 to 100 (1.0 = exactly fair), not Slurm's 0 to 1 (0.5 = exactly fair); the user ranking is identical but the raw values are on a different scale.
   * - QOS ``GrpWall``
     - Enforced at scheduling admission only: an over-budget QOS stops admitting jobs, and running jobs are never killed where Slurm cancels them. Consumption is a trailing window (``grp_wall_window_days`` under ``[accounting]``) rather than decayed usage, and the cap exists on a QOS only, not on an association. See :ref:`grpwall-budgets`.
   * - ``KillWait``
     - Supported as ``[scheduler] kill_wait_secs`` (default ``5``; Slurm's is ``30``). It is the wait between SIGTERM and SIGKILL for ``scancel``, ``scontrol requeue``, the final preemption kill, idle-fill reclaim, reservation end and other graceful stops, and it ends early once the job's processes exit. Valid ``0``-``65533``, with values below ``5`` acting as ``5``. Unlike Slurm's, it does not cover time limits (that wait is ``[scheduler] timeout_kill_wait_secs``, default ``30``, same range, which also covers the inactivity limit) and does not yet reach ``srun`` steps (see :ref:`kill wait note <kill-wait-steps-caveat>`). To get Slurm's single ``KillWait`` everywhere, set both to the same value. ``scontrol show config`` prints both values in effect, in seconds, as ``KillWait`` and ``TimeoutKillWait``. A time-limited job shows ``COMPLETING`` rather than ``RUNNING`` during the wait and still ends ``TIMEOUT``. See :ref:`kill wait upgrade notes <kill-wait-upgrade-notes>`.
   * - ``GraceTime`` (QOS and partition)
     - Supported per QOS (``sacctmgr add|modify qos ... GraceTime=<seconds>``) and applies to the preempted job's QOS, as in Slurm. A partition can also set ``GraceTime`` (``[[partitions]] grace_time`` or ``scontrol update PartitionName=... GraceTime=<seconds>``). Spur honors both together: the victim's QOS value if set (``GraceTime=0`` is a set value meaning no grace; ``cleargracetime=1`` returns the QOS to unset), else the highest value among the partitions it runs in, else ``0``. Slurm reads only the value of the active ``PreemptType`` plugin, the QOS's under ``preempt/qos`` and the partition's under ``preempt/partition_prio``. The grace is ``min(GraceTime, time left on the victim's limit)`` and covers ``cancel`` and ``requeue`` preemption. As in Slurm, the SIGCONT then SIGTERM warning goes to the job's ``srun`` steps only, not the batch script, a job in its grace is preempted at the deadline even if the preemptor no longer needs the nodes, and a job in its grace stays running while some of its nodes finish early (a finished node is freed, the others are not signalled, and an ``srun``-launching batch script ends the job when it exits). Nodes managed by the Kubernetes operator, and nodes running a ``spurd`` that predates the feature, give no grace. See :ref:`preemption grace time <qos-preempt-grace>`.

Accounting and QOS Mapping
--------------------------

Accounting entities map directly from Slurm:

- ``sacctmgr add account``, ``sacctmgr add user``, and ``sacctmgr add qos`` work as
  in Slurm, following the same ``cluster → account → user → association → QOS``
  ordering.
- The ``[accounting] require_qos`` setting is the equivalent of Slurm's
  ``AccountingStorageEnforce=qos``.
- The ``[accounting] require_association`` setting is the equivalent of Slurm's
  ``AccountingStorageEnforce=associations``.
- The ``default_qos`` setting is the equivalent of Slurm's fallback QOS.
- The ``[auth] private_data`` setting is the equivalent of the ``slurm.conf``
  side of Slurm's ``PrivateData`` (see :ref:`private-data`). Like Slurm, it is
  unset by default, so every user sees every job. ``"jobs"``, ``"usage"``,
  ``"users"``, ``"accounts"``, and ``"reservations"`` are supported, written as
  a TOML list (``private_data = ["jobs", "usage"]``), not a comma-separated
  string. ``sacct`` and ``sreport job`` sizes follow ``"jobs"``. ``sshare`` and
  ``sreport`` utilization follow ``"usage"``. ``sprio`` follows ``"jobs"``.
  ``sreport preemptions`` follows both ``"jobs"`` and ``"usage"``.
  User and account listings follow ``"users"`` and ``"accounts"``. ``sshare``
  is not filtered by ``"accounts"``. A coordinator sees the accounts they
  coordinate, including sub-accounts, and the ``sacct`` history,
  ``sreport job`` sizes, usage, and associations in those accounts.
  ``sacctmgr show user`` stays the caller. ``show user withcoord`` and
  ``show account withcoord`` list coordinator assignments, and
  ``show coordinator`` is the same listing. ``squeue`` and ``sprio`` stay the
  caller's own jobs. Unlike Slurm,
  root has no implicit exemption: grant it a role
  (``cluster_admins``, ``admin_groups``, an accounting admin level, or
  ``allow_uid_zero_administrator``). Coordinators do not gain Slurm's write
  powers (add users, change limits, cancel jobs).

See :doc:`/admin-guide/accounting` for the accounting concept guide.

.. tip::

   Before migrating a production workload, test the golden path on a small cluster
   first: submit a job (``sbatch``), check the queue (``squeue``), cancel a job
   (``scancel``), and review accounting history (``sacct``).

See Also
--------

- :doc:`/user-guide/slurm-compatibility`
- :doc:`/admin-guide/accounting`
- :doc:`/admin-guide/configuration`
