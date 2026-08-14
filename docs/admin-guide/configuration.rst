Configuration Reference (spur.conf)
===================================

``spur.conf`` is a TOML file describing controller, node, accounting, scheduling,
and network settings. The default location is ``/etc/spur/spur.conf`` (the Ansible
layout installs it at ``<spur_home>/etc/spur.conf``). Only ``cluster_name`` is
required; every section has a default and may be omitted, and unknown keys are
silently ignored. The controller validates the file on load.

The sections below are grouped by subsystem. Every field lists its type, default,
and meaning.

.. note::

   ``spurctld`` reads every section of ``spur.conf``. ``spurd`` reads the same file
   but only for local agent settings (``[hooks]``, ``[devices]``, ``rlimits.memlock``,
   ``[cluster]``, and ``[mpi]``); its identity and networking come from CLI flags.
   Node CPU, memory, and GRES are reported by each agent when it registers, not
   declared here — ``[[nodes]]`` only overlays scheduling policy onto nodes that
   have already registered.

Minimal configuration
----------------------

A working single-node configuration needs a cluster name and one partition; nodes
join by registering, so the ``[[nodes]]`` block below is optional and only tags
them for ``--constraint`` matching. Accounting, WireGuard, and the k0s cluster
manager are all off unless explicitly configured.

.. code-block:: toml

   cluster_name = "mi300x-cluster"

   [controller]
   listen_addr = "[::]:6817"
   state_dir = "/var/spool/spur"
   max_batch_requeue = 5

   [scheduler]
   plugin = "backfill"
   interval_secs = 1
   max_jobs_per_cycle = 10000
   fairshare_halflife_days = 14

   [accounting]
   database_url = "postgresql://spur:spur@localhost/spur"

   [auth]
   plugin = "none"

   [[partitions]]
   name = "gpu"
   default = true
   state = "UP"
   nodes = "mi300,mi300-2"
   max_time = "7-00:00:00"
   default_time = "1:00:00"
   min_nodes = 1
   priority_tier = 1

   [[nodes]]
   names = "mi300,mi300-2"
   features = ["mi300x", "rocm6"]
   weight = 1

The full annotated example — including label selectors, account restrictions, and
the k0s cluster manager — lives at ``examples/spur.conf`` in the repository.

.. _reload-scope:

Applying configuration changes
------------------------------

After editing ``spur.conf``, ``scontrol reconfigure`` re-reads the file and applies
it to the running controller, which makes the file authoritative: runtime-only
changes made with ``scontrol update`` are overwritten by the file's values.
``spurctld`` does not reload on ``SIGHUP`` — ``scontrol reconfigure`` is the only
trigger.

Not every field can be applied to a running daemon. Each section below records what
its fields need in a **Reload** column — or, where every field in a section shares
the same scope, in a single ``Reload:`` line above the table:

.. list-table::
   :header-rows: 1
   :widths: 22 78

   * - Reload
     - Meaning
   * - Live
     - Applied by ``scontrol reconfigure``; no restart needed.
   * - Restart
     - Read once when ``spurctld`` starts. ``reconfigure`` re-reads the value but
       does not apply it; restart the controller.
   * - Agent restart
     - Consumed by ``spurd`` on each compute node. ``reconfigure`` reaches only the
       controller, so restart ``spurd`` on every node.
   * - Client
     - Read by the ``spur`` CLI from the submitting host on each invocation. Neither
       ``reconfigure`` nor a daemon restart applies.
   * - Not implemented
     - Parsed and validated, but no code consumes it. Setting it has no effect.

The restart-only set mirrors Slurm, where ports, ``StateSaveLocation``, ``AuthType``,
and the plugin set also require a daemon restart.

**Leader-only, in an HA cluster.** ``reconfigure`` is handled by the Raft leader and
swaps only that controller's in-memory config; no Raft log entry carries the new
file, so followers keep the config they loaded at startup until they restart (in
Kubernetes they re-read the same ConfigMap). ``[[partitions]]`` is the exception —
partition changes replicate through the write-ahead log — but a follower re-derives
node features and weight from its own pre-reconfigure ``[[nodes]]`` blocks. Do not
rely on reconfigured non-partition state surviving an immediate failover; roll the
controllers to converge them.

.. warning::

   A partition removed from ``spur.conf`` is skipped rather than deleted when it
   still has active jobs. ``scontrol reconfigure`` reports success either way, and
   the skip is recorded only in the controller log. Drain a partition before
   removing it from the file.

Top-level keys
--------------

.. list-table::
   :header-rows: 1
   :widths: 18 18 14 14 36

   * - Field
     - Type
     - Default
     - Reload
     - Description
   * - ``cluster_name``
     - string
     - **(required)**
     - Live
     - Cluster name. An empty value fails to load with ``missing required field:
       cluster_name``. Changing it live re-labels metrics mid-series; prefer a
       restart.
   * - ``licenses``
     - table<string, integer>
     - ``{}``
     - Live
     - Cluster-wide license pool, e.g. ``{ fluent = 20, comsol = 5 }``. Jobs
       consume licenses via ``--licenses``. Availability is derived as total minus
       in-use, so changing a total cannot strand a running job's holding.

``[controller]``
----------------

Controller daemon (``spurctld``) network endpoints, state storage, job-ID range,
and Raft high-availability topology.

.. list-table::
   :header-rows: 1
   :widths: 24 12 18 14 32

   * - Field
     - Type
     - Default
     - Reload
     - Description
   * - ``listen_addr``
     - string
     - ``"[::]:6817"``
     - Restart
     - gRPC listen address serving ``SlurmController`` and ``SlurmAccounting``.
   * - ``rest_addr``
     - string
     - ``"[::]:6820"``
     - Restart
     - REST API listen address.
   * - ``hosts``
     - [string]
     - ``["localhost"]``
     - Client
     - Controller hostname(s); the first is primary. The CLI builds failover
       endpoints from these hosts plus the port of ``listen_addr``. Read by the
       CLI on each invocation, not by ``spurctld``.
   * - ``state_dir``
     - string
     - ``"/var/spool/spur"``
     - Not implemented
     - Ignored. The controller always uses its ``--state-dir`` flag, which
       defaults to ``/var/spool/spur``.
   * - ``max_job_id``
     - integer
     - ``999999999``
     - Not implemented
     - Intended as the job-ID wrap point. No code consumes it; the counter does
       not wrap.
   * - ``first_job_id``
     - integer
     - ``1``
     - Restart
     - Job ID assigned to the first submitted job.
   * - ``peers``
     - [string]
     - ``[]``
     - Restart
     - Raft HA peers as ``"host:port"``. Empty means single-node. The list must be
       identically ordered on every controller — node IDs derive from position.
       Example: ``["node1:6821", "node2:6821", "node3:6821"]``.
   * - ``node_id``
     - integer
     - none
     - Restart
     - This controller's Raft ID. Normally unset (single-node always uses ``1``).
       When set it must fall in ``1..=peers.len()`` and equal this host's position
       in ``peers``.
   * - ``raft_listen_addr``
     - string
     - ``"[::]:6821"``
     - Restart
     - Internal Raft gRPC listen address, separate from the client API.
   * - ``heartbeat_timeout_secs``
     - integer
     - none
     - Restart
     - Seconds without a heartbeat before a node is marked Down. Unset by
       default; the controller applies a 90-second fallback when absent.
   * - ``max_batch_requeue``
     - integer
     - ``5``
     - Live
     - Maximum automatic requeues (excluding preemption) before a job is held with
       ``JobHoldMaxRequeue``. Must be ``>= 1``; ``0`` is a validation error.
   * - ``max_launch_backoff_secs``
     - integer
     - ``300``
     - Live
     - Upper bound on the exponential backoff applied before retrying a failed
       job launch.
   * - ``hold_on_prolog_fail``
     - bool
     - ``true``
     - Live
     - Hold a job whose ``prolog_slurmctld`` hook fails instead of requeuing it.
   * - ``terminal_job_retention_secs``
     - integer
     - ``3600``
     - Live
     - How long a completed job stays in controller memory before eviction.
       Accounting rows in PostgreSQL are unaffected.
   * - ``dispatch_reject_cooldown_secs``
     - integer
     - ``30``
     - Live
     - How long a node is skipped for dispatch after rejecting a launch as
       resources-unavailable.

``[accounting]``
----------------

PostgreSQL-backed accounting, fairshare, and QOS enforcement. Accounting runs
in-process inside ``spurctld`` (served on port 6817) — there is no separate
``slurmdbd``.

.. list-table::
   :header-rows: 1
   :widths: 24 10 20 14 32

   * - Field
     - Type
     - Default
     - Reload
     - Description
   * - ``database_url``
     - string
     - ``""``
     - Restart
     - PostgreSQL connection string. A non-empty value enables accounting; empty
       disables it entirely. Example: ``"postgresql://spur:spur@localhost/spur"``.
       The connection pool is built at startup.
   * - ``fairshare_refresh_secs``
     - integer
     - ``300``
     - Restart
     - How often (seconds) to refresh fairshare and QOS caches from the database.
       The interval is baked into the refresh loops when they are spawned.
   * - ``default_qos``
     - string
     - ``""``
     - Live
     - Cluster-wide fallback QOS, applied at submit when a job resolves to no QOS
       (the analog of Slurm's ``normal``). Must name an existing QOS; empty means
       no fallback.
   * - ``require_qos``
     - bool
     - ``false``
     - Live
     - Reject at submit any job that still has no QOS after the resolution chain.
       Mirrors Slurm's ``AccountingStorageEnforce=qos``.

See :doc:`accounting` for how ``default_qos`` and ``require_qos`` interact with the
per-job QOS resolution chain.

``[scheduler]``
---------------

Scheduling loop cadence, per-cycle limits, and fairshare decay.

.. list-table::
   :header-rows: 1
   :widths: 28 10 16 14 32

   * - Field
     - Type
     - Default
     - Reload
     - Description
   * - ``plugin``
     - string
     - ``"backfill"``
     - Restart
     - Scheduler plugin name. Backfill is the only implemented scheduler; this
       value is a display label reported by ``sdiag`` and does not select an
       algorithm.
   * - ``interval_secs``
     - integer
     - ``1``
     - Restart
     - How often (seconds) the scheduler runs. The loop cadence and the
       preemption requeue hold are fixed at startup; the launch-backoff base
       does re-read this value live.
   * - ``max_jobs_per_cycle``
     - integer
     - ``10000``
     - Restart
     - Maximum number of jobs evaluated per scheduling cycle.
   * - ``fairshare_halflife_days``
     - integer
     - ``14``
     - Restart
     - Fairshare usage decay half-life, in days.
   * - ``default_time_limit_minutes``
     - integer
     - ``0``
     - Live
     - Cluster-wide fallback wall-time (minutes) for a job that sets no ``-t`` and
       lands on a partition with no ``DefaultTime``. ``0`` disables the fallback,
       leaving such jobs unbounded. Set > 0 to bound otherwise-unlimited jobs.
       When enabled, a ``-t``-less job on a partition that has a finite ``MaxTime``
       but no ``DefaultTime`` defaults to that partition's ``MaxTime`` (for a
       multi-partition request, the smallest ``MaxTime`` among them), not this
       flat value. Prior to this release the setting was inert (never applied);
       it now takes effect, and its default changed from ``60`` to ``0`` so
       ``-t``-less jobs stay unbounded exactly as before. A site that had set it
       expecting an effect will now see that effect.
   * - ``enforce_part_limits``
     - string
     - ``NO``
     - Live
     - Whether partition wall-time limits are enforced at submit. ``NO`` admits
       over-limit jobs and lets them pend with a ``PartitionTimeLimit`` reason.
       ``ALL`` rejects unless the job fits every requested partition; ``ANY``
       rejects only when it fits none. Mirrors Slurm's ``EnforcePartLimits``.
   * - ``complete_wait_secs``
     - integer
     - ``300``
     - Live
     - Maximum seconds a job may sit in COMPLETING before it is force-finished.
   * - ``inactive_limit_secs``
     - integer
     - ``0``
     - Live
     - Reap an interactive allocation (``salloc``/``srun``) whose client has sent
       no keepalive for this many seconds, freeing the nodes. ``0`` (the default)
       disables reaping. Mirrors Slurm's ``InactiveLimit``. Once enabled, *every*
       interactive allocation is subject to reaping regardless of client version:
       a client too old to send keepalives is reaped once idle past the limit, so
       upgrade all ``spur`` CLI clients before enabling this. Must be at least
       twice the client keepalive interval (60 seconds); smaller non-zero values
       are rejected at startup so a live client is never reaped between pings.
   * - ``resv_overrun_minutes``
     - integer
     - ``0``
     - Live
     - Grace minutes after a reservation ends before its still-running jobs are
       cancelled.

``[auth]``
----------

Authentication plugin for client requests.

.. list-table::
   :header-rows: 1
   :widths: 20 10 16 14 40

   * - Field
     - Type
     - Default
     - Reload
     - Description
   * - ``plugin``
     - string
     - ``"jwt"``
     - Not implemented
     - Intended to select an authentication plugin. No code reads it, and no
       plugin is enforced regardless of its value — see the warning below.
   * - ``jwt_key``
     - string
     - none
     - Restart
     - Signing key for node admission tokens, given as a file path or inline
       value. Deliberately not reloadable: swapping it live would immediately
       invalidate every outstanding node token.
   * - ``allow_root_jobs``
     - bool
     - ``false``
     - Agent restart
     - Permit jobs to run as UID 0. Consumed by ``spurd`` at its own startup.

.. warning::

   ``[auth] plugin`` is inert. Setting it to ``"jwt"`` does **not** authenticate
   RPCs — the controller accepts requests from anyone who can reach its gRPC port,
   and ``spurctld`` warns about this at startup whenever it binds a non-loopback
   address. Restrict access to the controller port at the network layer. See
   :doc:`accounting` for how identity maps to accounts and admin rights.

.. note::

   ``jwt_key`` is used for node admission tokens (``[admission] mode = "token"``),
   which is a separate mechanism from the unimplemented ``plugin`` field. When
   ``jwt_key`` is unset, admission tokens are signed with a well-known built-in
   key and are therefore forgeable; set an explicit key before enabling token
   admission.

``[[partitions]]``
------------------

An array of tables — one ``[[partitions]]`` block per partition (queue). Membership
is the union of the ``nodes`` hostlist pattern and the ``selector`` label match.

**Reload: Live** for every field below. Partitions are the only section that also
replicates to follower controllers, because ``reconfigure`` applies them through the
write-ahead log rather than the in-memory config swap. A partition still running
jobs is skipped rather than deleted (see :ref:`reload-scope`).

.. list-table::
   :header-rows: 1
   :widths: 22 18 20 40

   * - Field
     - Type
     - Default
     - Description
   * - ``name``
     - string
     - **(required)**
     - Partition (queue) name.
   * - ``default``
     - bool
     - ``false``
     - Mark this as the cluster default partition.
   * - ``state``
     - string
     - ``"UP"``
     - Partition state, parsed case-insensitively: ``UP``, ``DOWN``, ``DRAIN``;
       anything else becomes Inactive.
   * - ``nodes``
     - string
     - ``""``
     - Hostlist pattern of member nodes, e.g. ``"gpu[001-008]"`` or
       ``"mi300,mi300-2"``.
   * - ``selector``
     - table<string, string>
     - ``{}``
     - Label selector; a node joins if it matches **all** key=value pairs. Unioned
       with ``nodes``.
   * - ``max_time``
     - string
     - UNLIMITED
     - Maximum wall time. Slurm format: ``"72:00:00"``, ``"7-00:00:00"``, ``"60"``
       (minutes), or ``INFINITE`` / ``UNLIMITED``. Suffixed durations are also
       accepted: ``"1h"``, ``"90m"``, ``"1h40m"``, ``"2d12h"``, ``"30s"``.
   * - ``default_time``
     - string
     - UNLIMITED
     - Default wall time for jobs that omit ``--time``. Same format as ``max_time``.
   * - ``max_nodes``
     - integer
     - none
     - Maximum nodes per job.
   * - ``min_nodes``
     - integer
     - ``1``
     - Minimum nodes per job.
   * - ``allow_accounts``
     - [string]
     - ``[]``
     - Accounts permitted to submit to this partition (allow-list).
   * - ``deny_accounts``
     - [string]
     - ``[]``
     - Accounts denied submission to this partition (deny-list).
   * - ``priority_tier``
     - integer
     - ``0``
     - Partition priority tier; a higher tier preempts a lower one.
   * - ``preempt_mode``
     - string
     - ``"off"``
     - Preemption mode: ``cancel``, ``requeue``, ``suspend``; anything else is off.

``[[nodes]]``
-------------

An array of tables overlaying scheduling policy onto nodes. Match nodes by hostlist
pattern (``names``) or by label (``selector``); an entry applies if **either**
matches, and the first matching entry wins.

.. list-table::
   :header-rows: 1
   :widths: 20 16 12 16 36

   * - Field
     - Type
     - Default
     - Reload
     - Description
   * - ``names``
     - string
     - ``""``
     - Live
     - Hostlist pattern, e.g. ``"gpu[001-008]"``, or the literal ``ALL``. Optional
       when ``selector`` is used.
   * - ``selector``
     - table<string, string>
     - ``{}``
     - Live
     - Apply this entry to nodes matching **all** key=value pairs.
   * - ``cpus``
     - integer
     - ``0``
     - Not implemented
     - CPU count. Reported by the agent at registration; this value is ignored.
   * - ``memory_mb``
     - integer
     - ``0``
     - Not implemented
     - Memory in MB. Reported by the agent at registration; this value is ignored.
   * - ``gres``
     - [string]
     - ``[]``
     - Not implemented
     - Generic resources. Reported by the agent at registration; declare local
       GRES pools under ``[devices]`` on the node instead.
   * - ``features``
     - [string]
     - ``[]``
     - Live
     - Node features/tags for ``--constraint`` matching. A node matching no entry
       has its features cleared.
   * - ``address``
     - string
     - none
     - Live
     - Fallback address used until the agent registers one. It never overrides an
       address an agent has already reported.
   * - ``weight``
     - integer
     - ``1``
     - Live
     - Scheduling weight; higher is preferred. Reset to ``1`` for a node matching
       no entry.

.. note::

   ``[[nodes]]`` is not a node roster. A node joins the cluster when ``spurd``
   registers with the controller, so adding a block here does not create a node,
   and removing one does not remove a node — it only clears that node's features
   and weight. Remove a node with ``spur node remove <node>``. This differs from
   Slurm, where ``NodeName=`` lines in ``slurm.conf`` define the roster.

``[network]``
-------------

WireGuard mesh networking and the agent port.

.. list-table::
   :header-rows: 1
   :widths: 24 10 18 16 32

   * - Field
     - Type
     - Default
     - Reload
     - Description
   * - ``wg_enabled``
     - bool
     - ``false``
     - Not implemented
     - Intended to enable WireGuard mesh networking. No code reads it; the mesh
       is driven by ``[cluster] enabled`` instead.
   * - ``wg_cidr``
     - string
     - ``"10.44.0.0/16"``
     - Restart
     - CIDR for WireGuard address allocation. Validated as an IPv4 CIDR when
       ``[cluster]`` is enabled.
   * - ``wg_interface``
     - string
     - ``"spur0"``
     - Not implemented
     - Superseded by the ``SPUR_WG_INTERFACE`` environment variable read by
       ``spurd``, which defaults to ``spur0``.
   * - ``wg_port``
     - integer
     - ``51820``
     - Not implemented
     - Intended as the WireGuard listen port. No code reads it.
   * - ``agent_port``
     - integer
     - ``6818``
     - Not implemented
     - Each agent advertises its own port at registration, and the controller
       falls back to ``6818`` when it does not. Set the agent's port through its
       ``--listen`` address instead.
   * - ``reject_loopback_comm_addr``
     - bool
     - ``false``
     - Live
     - Reject a node registration whose advertised address is a loopback
       address, which would otherwise make the node unreachable from the
       controller.

``[logging]``
-------------

**Reload: Not implemented** for every field below. The section is parsed but no
daemon reads it.

.. list-table::
   :header-rows: 1
   :widths: 20 14 20 46

   * - Field
     - Type
     - Default
     - Description
   * - ``level``
     - string
     - ``"info"``
     - Intended log level. Use the ``--log-level`` flag or the ``RUST_LOG``
       environment variable instead.
   * - ``format``
     - string
     - ``"text"``
     - Intended log format. Output format is not configurable.
   * - ``file``
     - string
     - none
     - Intended log file path. Logging to a file is not implemented; daemons log
       to stderr, so redirect via the service manager (for example systemd's
       journal) instead.

``[rlimits]``
-------------

POSIX ``RLIMIT_*`` values ``spurd`` applies to job steps at launch.

**Reload: Agent restart.**

.. list-table::
   :header-rows: 1
   :widths: 18 12 22 48

   * - Field
     - Type
     - Default
     - Description
   * - ``memlock``
     - string
     - ``"unlimited"``
     - ``RLIMIT_MEMLOCK`` for job processes. ``"unlimited"`` (also ``""`` or
       ``"0"``) sets ``RLIM_INFINITY``; ``"inherit"`` leaves whatever ``spurd``
       inherited; a byte-count string (e.g. ``"1073741824"`` for 1 GiB) sets a fixed
       cap. An invalid value errors at parse time.

.. note::

   ``memlock = "unlimited"`` lets RDMA and NCCL workloads pin memory out of the box.
   Lower it only when a hard cap is required.

``[cgroup]``
------------

How ``spurd`` enforces a job's cgroup limits. The limits themselves come from the
allocation (``--mem``, ``--cpus-per-task``); this section only sets policy. The
fields mirror ``ConstrainSwapSpace`` and ``AllowedSwapSpace`` in Slurm's
``cgroup.conf``, including their defaults.

.. list-table::
   :header-rows: 1
   :widths: 24 10 12 54

   * - Field
     - Type
     - Default
     - Description
   * - ``constrain_swap_space``
     - bool
     - ``false``
     - Cap a job's swap along with its memory. While ``false``, ``memory.max``
       bounds resident memory only: on a node with swap, a job that outgrows
       ``--mem`` keeps running on swap instead of being killed.
   * - ``allowed_swap_space``
     - integer
     - ``0``
     - Swap a job may use, as a percentage (0-100) of its memory allocation.
       Only consulted when ``constrain_swap_space`` is set; ``0`` denies swap
       outright. A value above 100 is rejected when spurd starts.

.. note::

   Enabling ``constrain_swap_space`` is what makes ``--mem`` a hard cap and lets a
   runaway job be reported as ``OUT_OF_MEMORY`` rather than completing slowly on
   swap. It is off by default because turning it on starts killing jobs that
   previously survived by swapping. Both fields are read at ``spurd`` startup, so
   changing them needs an agent restart.

``[mpi]``
---------

PMIx plugin settings for ``--mpi=pmix`` jobs (batch launch and ``srun`` steps).

Plugin loading happens on the node, so those fields need an agent restart; the
per-step directory and timeouts are sent by the controller with each dispatch and
are reloadable.

.. list-table::
   :header-rows: 1
   :widths: 26 10 18 16 30

   * - Field
     - Type
     - Default
     - Reload
     - Description
   * - ``plugin_dir``
     - string
     - ``"/usr/lib/spur"``
     - Agent restart
     - Directory searched for the PMIx plugin when ``pmix_plugin`` is unset.
   * - ``pmix_plugin``
     - string
     - ``""``
     - Agent restart
     - Explicit path to the PMIx plugin. When empty, the plugin resolves to
       ``<plugin_dir>/spur_mpi_pmix.so``.
   * - ``pmix_min_version``
     - string
     - ``"4.1.0"``
     - Agent restart
     - Minimum PMIx library version accepted when loading the plugin.
   * - ``pmix_tmpdir``
     - string
     - ``"/tmp/spur-pmix"``
     - Live
     - Base directory for per-step PMIx scratch (namespace and rank state).
   * - ``modex_connect_timeout_secs``
     - integer
     - ``5``
     - Live
     - Timeout for a step's initial connection to the PMIx modex.
   * - ``modex_fence_timeout_secs``
     - integer
     - ``120``
     - Live
     - Timeout for a collective fence across the step's ranks.
   * - ``modex_verify_timeout_secs``
     - integer
     - ``30``
     - Live
     - Timeout for post-fence modex verification.

``[update]``
------------

Startup update checks and optional auto-download.

**Reload: Restart** for every field below — the controller configures its update
checker once at startup.

.. note::

   These fields apply to ``spurctld`` only. ``spurd`` runs its own startup update
   check with built-in defaults and does not read this section, so
   ``check_on_startup = false`` does not stop agents from checking.

.. list-table::
   :header-rows: 1
   :widths: 22 12 22 44

   * - Field
     - Type
     - Default
     - Description
   * - ``check_on_startup``
     - bool
     - ``true``
     - Check for updates on daemon startup.
   * - ``auto_update``
     - bool
     - ``false``
     - Automatically download and install updates.
   * - ``channel``
     - string
     - ``"stable"``
     - Release channel: ``"stable"`` or ``"nightly"``.
   * - ``cache_dir``
     - string
     - ``"/var/cache/spur"``
     - Directory for the update-check cache file.

.. note::

   Daemons never auto-restart, even with ``auto_update = true``. A downloaded update
   takes effect on the next manual restart.

``[admission]``
---------------

Controls which nodes may register with the controller.

**Reload: Live.**

.. list-table::
   :header-rows: 1
   :widths: 16 12 16 56

   * - Field
     - Type
     - Default
     - Description
   * - ``mode``
     - string
     - ``"open"``
     - Node admission mode. ``open`` lets any node register; ``token`` requires a
       registering ``spurd`` to present a valid admission token.

See :doc:`accounting` for managing admission tokens with ``spur token``.

``[devices]``
-------------

GPU and generic-resource discovery.

**Reload: Agent restart** for every field below, including each
``[[devices.gres]]`` entry — the device registry is built once when ``spurd``
starts.

.. list-table::
   :header-rows: 1
   :widths: 20 16 16 48

   * - Field
     - Type
     - Default
     - Description
   * - ``auto_detect``
     - bool
     - ``true``
     - Discover GPUs from AMD KFD sysfs when the CDI cache is empty (AMD only).
   * - ``cdi_spec_dirs``
     - [string]
     - ``[]``
     - Extra directories to scan for CDI specs, beyond ``/etc/cdi`` and
       ``/var/run/cdi``.
   * - ``gres``
     - [table]
     - ``[]``
     - File-based or countable GRES pools; see below.

Each ``[[devices.gres]]`` entry uses Slurm GRES syntax with fields ``name``
(required), ``type``, ``file``, ``multiple_files``, ``count``, ``cores``, ``links``,
and ``flags`` ([string]). Examples:

.. code-block:: toml

   [[devices.gres]]
   name = "gpu"
   file = "/dev/dri/renderD[128-129]"
   flags = ["amd_gpu_env"]

   [[devices.gres]]
   name = "bandwidth"
   type = "lustre"
   count = 4096
   flags = ["count_only"]

``[isolation]``
---------------

Job isolation layers.

.. warning::

   **Reload: Not implemented** for every field in this section. ``spurd`` does not
   read ``[isolation]``, so none of these values changes any behaviour — including
   setting one to ``false`` to disable a layer. Do not treat this section as a
   security control. The table below records the intended meaning of each field
   and how the corresponding behaviour is actually selected today.

.. list-table::
   :header-rows: 1
   :widths: 18 12 14 56

   * - Field
     - Type
     - Default
     - Intended meaning / actual behaviour
   * - ``setuid``
     - bool
     - ``true``
     - Run jobs as the submitting user's UID/GID. Always applied when ``spurd``
       runs as root; not gated by this field.
   * - ``namespaces``
     - bool
     - ``true``
     - PID and mount namespace isolation. Applied whenever ``spurd`` runs as
       root, except for multi-rank ``--mpi=pmix`` wrappers which stay in the host
       namespace; not gated by this field.
   * - ``seccomp``
     - bool
     - ``true``
     - seccomp-BPF syscall filter. Opt-in via the ``SPUR_SECCOMP=1``
       environment variable on ``spurd`` and **off** unless that is set.
   * - ``landlock``
     - bool
     - ``true``
     - Landlock filesystem access control. Actually opt-in via the
       ``SPUR_LANDLOCK=1`` environment variable on ``spurd`` and **off** unless
       that is set.

``[metrics]``
-------------

OpenMetrics HTTP export from ``spurctld``.

.. list-table::
   :header-rows: 1
   :widths: 22 10 18 14 36

   * - Field
     - Type
     - Default
     - Reload
     - Description
   * - ``enabled``
     - bool
     - ``true``
     - Restart
     - Start the metrics HTTP server.
   * - ``listen_addr``
     - string
     - ``"[::]:6822"``
     - Restart
     - Metrics HTTP listen address; the port is used when ``bind = "loopback"``.
   * - ``bind``
     - string
     - ``"loopback"``
     - Restart
     - ``loopback`` binds ``127.0.0.1:<port>``; ``all`` uses ``listen_addr`` as-is.
   * - ``high_cardinality``
     - bool
     - ``false``
     - Live
     - Serve the per-job/user/account metrics route. While ``false`` that route
       returns 404. High cardinality on a busy cluster; enable deliberately.

``[rest_api]``
--------------

.. list-table::
   :header-rows: 1
   :widths: 16 10 14 14 46

   * - Field
     - Type
     - Default
     - Reload
     - Description
   * - ``enabled``
     - bool
     - ``false``
     - Restart
     - Start the Slurm-compatible REST server (default port 6820). Off by
       default: the REST surface performs no authentication, so enabling it on a
       reachable address exposes unauthenticated job submission. Enable it only
       behind an authenticating proxy or on a loopback interface.

``[hooks]``
-----------

Prolog/epilog and job-submit scripts. Each field is an optional fully-qualified
path; unset means no hook. The prolog/epilog fields map one-to-one to Slurm's
parameters.

Reload scope follows whichever process executes the hook: controller hooks are
live, node hooks need an agent restart, and ``srun`` hooks are read from the
submitting host on each invocation.

.. list-table::
   :header-rows: 1
   :widths: 22 24 32 22

   * - Spur field
     - Slurm equivalent
     - Runs on
     - Reload
   * - ``prolog``
     - ``Prolog``
     - compute node, before job launch
     - Agent restart
   * - ``epilog``
     - ``Epilog``
     - compute node, at job termination
     - Agent restart
   * - ``prolog_slurmctld``
     - ``PrologSlurmctld``
     - controller, at allocation
     - Live
   * - ``epilog_slurmctld``
     - ``EpilogSlurmctld``
     - controller, at termination
     - Live
   * - ``task_prolog``
     - ``TaskProlog``
     - compute node, before each step
     - Agent restart
   * - ``task_epilog``
     - ``TaskEpilog``
     - compute node, after each step
     - Agent restart
   * - ``srun_prolog``
     - ``SrunProlog``
     - srun node, before step dispatch
     - Client
   * - ``srun_epilog``
     - ``SrunEpilog``
     - srun node, after step completion
     - Client
   * - ``job_submit``
     - ``JobSubmitPlugin``
     - controller, at submit
     - Live
   * - ``job_submit_lua``
     - ``job_submit.lua``
     - controller, at submit
     - Live

.. note::

   ``reconfigure`` validates ``job_submit`` and ``job_submit_lua`` before swapping
   the config, so a broken submit hook is rejected rather than applied — the
   previous configuration stays in place and the command reports an error.

``[notifications]``
-------------------

Job-event notification transports.

**Reload: Live** for every field below.

.. list-table::
   :header-rows: 1
   :widths: 22 12 16 50

   * - Field
     - Type
     - Default
     - Description
   * - ``webhook_url``
     - string
     - none
     - URL to POST job-event notifications to.
   * - ``smtp_command``
     - string
     - none
     - SMTP command for mail, e.g. ``"/usr/sbin/sendmail -t"``.
   * - ``from_address``
     - string
     - none
     - From address, e.g. ``"spur@cluster.local"``.

``[power]``
-----------

Idle-node suspend and resume.

.. list-table::
   :header-rows: 1
   :widths: 26 10 14 14 36

   * - Field
     - Type
     - Default
     - Reload
     - Description
   * - ``suspend_timeout_secs``
     - integer
     - none
     - Restart
     - Idle seconds before a node is suspended. Unset disables power management
       entirely. Read once when the power-management loop starts, so
       ``reconfigure`` can neither enable, disable, nor retime it.
   * - ``suspend_command``
     - string
     - none
     - Live
     - Suspend command; ``{node}`` is replaced with the node name, e.g.
       ``"systemctl suspend"``.
   * - ``resume_command``
     - string
     - none
     - Live
     - Resume command; ``{node}`` is replaced, e.g. ``"ipmitool chassis power on"``.

.. note::

   Because ``suspend_timeout_secs`` is restart-only, turning power management on
   for the first time requires a controller restart. Once running, the suspend and
   resume commands can be changed live.

Kubernetes modes
----------------

Spur has two distinct, mutually exclusive Kubernetes modes. ``[kubernetes]`` lets
Spur run **inside** an existing cluster and accept ``SpurJob`` CRDs; ``[cluster]``
lets Spur **own** and provision a k0s cluster.

``[kubernetes]``
~~~~~~~~~~~~~~~~

.. warning::

   **Reload: Not implemented** for every field in this section. The
   ``spur-k8s-operator`` binary does not read ``spur.conf`` at all; it takes its
   node selector from the ``--node-selector`` flag, its namespace from the
   ``POD_NAMESPACE`` environment variable, and its credentials from the ambient
   kubeconfig or in-cluster service account. Configure the operator through its
   deployment manifest, not here.

.. list-table::
   :header-rows: 1
   :widths: 26 12 30 32

   * - Field
     - Type
     - Default
     - Description
   * - ``enabled``
     - bool
     - ``false``
     - Enable K8s integration (accept ``SpurJob`` CRDs).
   * - ``kubeconfig``
     - string
     - none
     - Path to a kubeconfig; empty uses in-cluster config.
   * - ``namespace``
     - string
     - ``"spur"``
     - Namespace for ``SpurJob`` CRDs and Pods.
   * - ``node_label_selector``
     - string
     - ``"spur.amd.com/managed=true"``
     - Label selector for nodes in the Spur pool.

``[cluster]``
~~~~~~~~~~~~~

Spur-managed k0s cluster. When disabled (the default), ``spurd`` never touches
systemd or k0s.

This section is split across both daemons: the controller reads the network and
control-plane fields at startup, while ``spurd`` reads the on-node fields at its
own startup. Only ``allow_admin_kubeconfig`` is reloadable.

.. list-table::
   :header-rows: 1
   :widths: 26 10 20 16 28

   * - Field
     - Type
     - Default
     - Reload
     - Description
   * - ``enabled``
     - bool
     - ``false``
     - Restart
     - Enable the Spur-managed k0s cluster. Requires restarting the controller
       **and** every agent.
   * - ``distro``
     - string
     - ``"k0s"``
     - Not implemented
     - Intended to select a Kubernetes distribution. Validated as ``"k0s"`` on
       load but otherwise unused; k0s is always the distribution.
   * - ``pod_cidr``
     - string
     - ``"10.42.0.0/16"``
     - Restart
     - Pod network CIDR. Prefix must be ``<= /24`` (per-node /24 carving).
   * - ``service_cidr``
     - string
     - ``"10.43.0.0/16"``
     - Restart
     - Service network CIDR.
   * - ``cni``
     - string
     - ``"kuberouter"``
     - Restart
     - CNI mode: ``"kuberouter"`` (k0s default) or ``"calico"`` (bird native routing
       over the mesh).
   * - ``cni_mtu``
     - integer
     - ``1450``
     - Restart
     - CNI MTU, leaving headroom for WireGuard overhead.
   * - ``control_plane_node``
     - string
     - none
     - Restart
     - Hostname running the k0s control plane; empty picks one from inventory.
   * - ``control_plane_replicas``
     - integer
     - ``1``
     - Restart
     - Number of control-plane members to provision.
   * - ``k8s_provisioning_timeout_secs``
     - integer
     - ``600``
     - Restart
     - How long a node may stay in provisioning before it is marked Degraded.
   * - ``allow_admin_kubeconfig``
     - bool
     - ``false``
     - Live
     - Allow the controller to hand out a cluster-admin kubeconfig. Reloadable,
       so it can be turned off without a restart.
   * - ``k0s_version``
     - string
     - pinned
     - Agent restart
     - k0s release the agent installs.
   * - ``k0s_binary``
     - string
     - built-in path
     - Agent restart
     - Path to the ``k0s`` binary on the node.
   * - ``storage_provisioner``
     - string
     - ``"local-path"``
     - Agent restart
     - ``"local-path"`` ships a default node-local StorageClass; ``"none"`` disables
       it. Other values are rejected.
   * - ``local_path_dir``
     - string
     - ``/var/lib/local-path-provisioner``
     - Agent restart
     - On-node directory for local-path PVs. Must be absolute and free of quotes,
       backslashes, whitespace, and control characters.

See :doc:`/deployment/managed-kubernetes` for provisioning a Spur-owned cluster.

``[federation]``, ``[topology]``, ``[burst_buffer]``
----------------------------------------------------

``[federation]``
   **Reload: Live.** Peer clusters for cross-cluster job routing. Each
   ``[[federation.clusters]]`` entry has ``name`` (string) and ``address``
   (string, e.g. ``"http://peer-ctrl:6817"``). Defaults to no peers.

``[topology]``
   **Reload: Restart.** Optional switch-hierarchy configuration for locality-aware
   scheduling.
   ``plugin`` (string, default ``"none"``) selects the model: ``"tree"`` for a
   switch hierarchy, ``"block"`` for fixed-size blocks, or ``"none"`` to disable.
   In tree mode, each ``[[topology.switches]]`` entry has ``name`` (string),
   ``nodes`` (hostlist pattern for a leaf switch), and ``switches`` (comma-separated
   child switch names for an aggregation switch). In block mode, ``block_size``
   (integer) sets the number of nodes per block. Defaults to no topology.

``[burst_buffer]``
   **Reload: Live.** Burst-buffer capacity. ``total_gb`` (integer, default ``0``)
   sets total capacity
   in GiB; jobs reserve via ``--bb capacity=NNN``. ``0`` disables burst buffers, and
   requesting jobs stay pending with ``BurstBufferResources``.

Validation
----------

The controller validates ``spur.conf`` on load and refuses to start on error:

- ``cluster_name`` must be non-empty.
- ``controller.max_batch_requeue`` must be ``>= 1``.
- When ``[cluster]`` is enabled:

  - ``distro`` must be ``"k0s"``.
  - ``network.wg_cidr``, ``cluster.pod_cidr``, and ``cluster.service_cidr`` must be
    valid IPv4 CIDRs, and ``pod_cidr`` must be ``<= /24``.
  - The three CIDRs must not overlap.
  - ``storage_provisioner`` must be ``local-path`` or ``none``.
  - ``local_path_dir`` must be absolute and clean when the local-path provisioner is
    used.

Environment overrides
---------------------

.. note::

   Config-file fields are **not** overridable by environment variables.
   ``SPUR_CONTROLLER_ADDR`` is a CLI-level override that sets the controller address
   for client commands (``sacctmgr``, ``scontrol``, ``spur token``); it does not
   affect any ``spur.conf`` field.

See Also
--------

- :doc:`accounting`
- :doc:`/deployment/ansible`
- :doc:`/deployment/native-host`
