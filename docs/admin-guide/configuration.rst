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
   Node CPU, memory, and GRES are declared to the controller under ``[[nodes]]`` here.

Minimal configuration
----------------------

A working single-node configuration needs a cluster name, one partition, and the
node(s) that back it. Accounting, WireGuard, and the k0s cluster manager are all
off unless explicitly configured.

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
   names = "mi300"
   cpus = 256
   memory_mb = 2321924
   gres = ["gpu:mi300x:8"]

   [[nodes]]
   names = "mi300-2"
   cpus = 256
   memory_mb = 2321904
   gres = ["gpu:mi300x:8"]

   [network]
   wg_enabled = false
   agent_port = 6818

   [logging]
   level = "info"
   format = "text"

The full annotated example — including label selectors, account restrictions, and
the k0s cluster manager — lives at ``examples/spur.conf`` in the repository.

Top-level keys
--------------

.. list-table::
   :header-rows: 1
   :widths: 20 20 20 40

   * - Field
     - Type
     - Default
     - Description
   * - ``cluster_name``
     - string
     - **(required)**
     - Cluster name. An empty value fails to load with ``missing required field:
       cluster_name``.
   * - ``licenses``
     - table<string, integer>
     - ``{}``
     - Cluster-wide license pool, e.g. ``{ fluent = 20, comsol = 5 }``. Jobs
       consume licenses via ``--licenses``.

``[controller]``
----------------

Controller daemon (``spurctld``) network endpoints, state storage, job-ID range,
and Raft high-availability topology.

.. list-table::
   :header-rows: 1
   :widths: 24 16 22 38

   * - Field
     - Type
     - Default
     - Description
   * - ``listen_addr``
     - string
     - ``"[::]:6817"``
     - gRPC listen address serving ``SlurmController`` and ``SlurmAccounting``.
   * - ``rest_addr``
     - string
     - ``"[::]:6820"``
     - REST API listen address.
   * - ``hosts``
     - [string]
     - ``["localhost"]``
     - Controller hostname(s); the first is primary. Clients and agents build
       failover endpoints from these hosts plus the port of ``listen_addr``.
   * - ``state_dir``
     - string
     - ``"/var/spool/spur"``
     - Directory where the controller persists Raft log and scheduler state.
   * - ``max_job_id``
     - integer
     - ``999999999``
     - Highest job ID before the counter wraps.
   * - ``first_job_id``
     - integer
     - ``1``
     - Job ID assigned to the first submitted job.
   * - ``peers``
     - [string]
     - ``[]``
     - Raft HA peers as ``"host:port"``. Empty means single-node. The list must be
       identically ordered on every controller — node IDs derive from position.
       Example: ``["node1:6821", "node2:6821", "node3:6821"]``.
   * - ``node_id``
     - integer
     - none
     - This controller's Raft ID. Normally unset (single-node always uses ``1``).
       When set it must fall in ``1..=peers.len()`` and equal this host's position
       in ``peers``.
   * - ``raft_listen_addr``
     - string
     - ``"[::]:6821"``
     - Internal Raft gRPC listen address, separate from the client API.
   * - ``heartbeat_timeout_secs``
     - integer
     - none
     - Seconds without a heartbeat before a node is marked Down. Unset by
       default; the controller applies a 90-second fallback when absent.
   * - ``max_batch_requeue``
     - integer
     - ``5``
     - Maximum automatic requeues (excluding preemption) before a job is held with
       ``JobHoldMaxRequeue``. Must be ``>= 1``; ``0`` is a validation error.

``[accounting]``
----------------

PostgreSQL-backed accounting, fairshare, and QOS enforcement. Accounting runs
in-process inside ``spurctld`` (served on port 6817) — there is no separate
``slurmdbd``.

.. list-table::
   :header-rows: 1
   :widths: 24 12 24 40

   * - Field
     - Type
     - Default
     - Description
   * - ``database_url``
     - string
     - ``""``
     - PostgreSQL connection string. A non-empty value enables accounting; empty
       disables it entirely. Example: ``"postgresql://spur:spur@localhost/spur"``.
   * - ``fairshare_refresh_secs``
     - integer
     - ``300``
     - How often (seconds) to refresh fairshare and QOS caches from the database.
   * - ``default_qos``
     - string
     - ``""``
     - Cluster-wide fallback QOS, applied at submit when a job resolves to no QOS
       (the analog of Slurm's ``normal``). Must name an existing QOS; empty means
       no fallback.
   * - ``require_qos``
     - bool
     - ``false``
     - Reject at submit any job that still has no QOS after the resolution chain.
       Mirrors Slurm's ``AccountingStorageEnforce=qos``.

See :doc:`accounting` for how ``default_qos`` and ``require_qos`` interact with the
per-job QOS resolution chain.

``[scheduler]``
---------------

Scheduling loop cadence, per-cycle limits, and fairshare decay.

.. list-table::
   :header-rows: 1
   :widths: 30 12 18 40

   * - Field
     - Type
     - Default
     - Description
   * - ``plugin``
     - string
     - ``"backfill"``
     - Scheduler plugin name.
   * - ``interval_secs``
     - integer
     - ``1``
     - How often (seconds) the scheduler runs.
   * - ``max_jobs_per_cycle``
     - integer
     - ``10000``
     - Maximum number of jobs evaluated per scheduling cycle.
   * - ``fairshare_halflife_days``
     - integer
     - ``14``
     - Fairshare usage decay half-life, in days.
   * - ``default_time_limit_minutes``
     - integer
     - ``0``
     - Cluster-wide fallback wall-time (minutes) for a job that sets no ``-t`` and
       lands on a partition with no ``DefaultTime``. ``0`` disables the fallback,
       leaving such jobs unbounded. Set > 0 to bound otherwise-unlimited jobs.
   * - ``enforce_part_limits``
     - string
     - ``NO``
     - Whether partition wall-time limits are enforced at submit. ``NO`` admits
       over-limit jobs and lets them pend with a ``PartitionTimeLimit`` reason.
       ``ALL`` rejects unless the job fits every requested partition; ``ANY``
       rejects only when it fits none. Mirrors Slurm's ``EnforcePartLimits``.
   * - ``complete_wait_secs``
     - integer
     - ``300``
     - Maximum seconds a job may sit in COMPLETING before it is force-finished.
   * - ``resv_overrun_minutes``
     - integer
     - ``0``
     - Grace minutes after a reservation ends before its still-running jobs are
       cancelled.

``[auth]``
----------

Authentication plugin for client requests.

.. list-table::
   :header-rows: 1
   :widths: 20 16 20 44

   * - Field
     - Type
     - Default
     - Description
   * - ``plugin``
     - string
     - ``"jwt"``
     - Authentication plugin. ``jwt`` and ``none`` are the supported plugins;
       ``none`` trusts the caller's OS identity (username and real UID/GID, with
       admin granted to UID 0). The struct default is ``"jwt"``, but
       ``examples/spur.conf`` ships ``"none"``.
   * - ``jwt_key``
     - string
     - none
     - JWT secret key, given as a file path or inline value. Required by the ``jwt``
       plugin.

.. note::

   ``munge`` is accepted as a value but is not documented here as a supported
   plugin. Use ``jwt`` for cryptographic authentication or ``none`` to trust OS
   identity. See :doc:`accounting` for how identity maps to accounts and admin
   rights.

``[[partitions]]``
------------------

An array of tables — one ``[[partitions]]`` block per partition (queue). Membership
is the union of the ``nodes`` hostlist pattern and the ``selector`` label match.

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

An array of tables declaring node capacity to the controller. Match nodes by
hostlist pattern (``names``) or by label (``selector``).

.. list-table::
   :header-rows: 1
   :widths: 22 18 16 44

   * - Field
     - Type
     - Default
     - Description
   * - ``names``
     - string
     - ``""``
     - Hostlist pattern, e.g. ``"gpu[001-008]"``. Optional when ``selector`` is used.
   * - ``selector``
     - table<string, string>
     - ``{}``
     - Apply this config to nodes matching **all** key=value pairs.
   * - ``cpus``
     - integer
     - ``0``
     - CPU count.
   * - ``memory_mb``
     - integer
     - ``0``
     - Memory in MB.
   * - ``gres``
     - [string]
     - ``[]``
     - Generic resources, e.g. ``["gpu:mi300x:8"]``.
   * - ``features``
     - [string]
     - ``[]``
     - Node features/tags for ``--constraint`` matching.
   * - ``address``
     - string
     - none
     - Override address when it differs from the hostname.
   * - ``weight``
     - integer
     - ``1``
     - Scheduling weight; higher is preferred.

``[network]``
-------------

WireGuard mesh networking and the agent port.

.. list-table::
   :header-rows: 1
   :widths: 22 12 24 42

   * - Field
     - Type
     - Default
     - Description
   * - ``wg_enabled``
     - bool
     - ``false``
     - Enable WireGuard mesh networking.
   * - ``wg_cidr``
     - string
     - ``"10.44.0.0/16"``
     - CIDR for WireGuard address allocation. Validated as an IPv4 CIDR when
       ``[cluster]`` is enabled.
   * - ``wg_interface``
     - string
     - ``"spur0"``
     - WireGuard interface name.
   * - ``wg_port``
     - integer
     - ``51820``
     - WireGuard listen port.
   * - ``agent_port``
     - integer
     - ``6818``
     - ``spurd`` agent gRPC listen port.

``[logging]``
-------------

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
     - Log level.
   * - ``format``
     - string
     - ``"text"``
     - Log format.
   * - ``file``
     - string
     - none
     - Log file path. Unset logs to stderr.

``[rlimits]``
-------------

POSIX ``RLIMIT_*`` values ``spurd`` applies to job steps at launch.

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

``[mpi]``
---------

PMIx plugin settings for ``--mpi=pmix`` jobs (batch launch and ``srun`` steps).

.. list-table::
   :header-rows: 1
   :widths: 18 12 22 48

   * - Field
     - Type
     - Default
     - Description
   * - ``plugin_dir``
     - string
     - ``"/usr/lib/spur"``
     - Directory searched for the PMIx plugin when ``pmix_plugin`` is unset.
   * - ``pmix_plugin``
     - string
     - ``""``
     - Explicit path to the PMIx plugin. When empty, the plugin resolves to
       ``<plugin_dir>/spur_mpi_pmix.so``.
   * - ``pmix_tmpdir``
     - string
     - ``"/tmp/spur-pmix"``
     - Base directory for per-step PMIx scratch (namespace and rank state).
   * - ``pmix_min_version``
     - string
     - ``"4.1.0"``
     - Minimum PMIx library version accepted when loading the plugin.

``[update]``
------------

Startup update checks and optional auto-download.

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

Job isolation layers. Each degrades gracefully when the platform does not support it.

.. list-table::
   :header-rows: 1
   :widths: 18 12 14 56

   * - Field
     - Type
     - Default
     - Description
   * - ``setuid``
     - bool
     - ``true``
     - Run jobs as the submitting user's UID/GID (requires a root ``spurd``).
   * - ``namespaces``
     - bool
     - ``true``
     - PID and mount namespace isolation (requires root).
   * - ``seccomp``
     - bool
     - ``true``
     - seccomp-BPF syscall filter (kernel 3.5+; blocks ptrace/mount/bpf).
   * - ``landlock``
     - bool
     - ``true``
     - Landlock filesystem access control (kernel 5.13+, native-host only).

``[metrics]``
-------------

OpenMetrics HTTP export from ``spurctld``.

.. list-table::
   :header-rows: 1
   :widths: 22 12 22 44

   * - Field
     - Type
     - Default
     - Description
   * - ``enabled``
     - bool
     - ``true``
     - Start the metrics HTTP server.
   * - ``listen_addr``
     - string
     - ``"[::]:6822"``
     - Metrics HTTP listen address; the port is used when ``bind = "loopback"``.
   * - ``bind``
     - string
     - ``"loopback"``
     - ``loopback`` binds ``127.0.0.1:<port>``; ``all`` uses ``listen_addr`` as-is.
   * - ``high_cardinality``
     - bool
     - ``false``
     - Reserved for a per-job/user/account metrics route; that route returns 404
       until implemented.

``[rest_api]``
--------------

.. list-table::
   :header-rows: 1
   :widths: 16 12 16 56

   * - Field
     - Type
     - Default
     - Description
   * - ``enabled``
     - bool
     - ``true``
     - Start the Slurm-compatible REST server (default port 6820).

``[hooks]``
-----------

Prolog/epilog scripts. Each field is an optional fully-qualified path; unset means
no hook. These map one-to-one to Slurm's prolog/epilog parameters.

.. list-table::
   :header-rows: 1
   :widths: 24 30 46

   * - Spur field
     - Slurm equivalent
     - Runs on
   * - ``prolog``
     - ``Prolog``
     - compute node, before job launch
   * - ``epilog``
     - ``Epilog``
     - compute node, at job termination
   * - ``prolog_slurmctld``
     - ``PrologSlurmctld``
     - controller, at allocation
   * - ``epilog_slurmctld``
     - ``EpilogSlurmctld``
     - controller, at termination
   * - ``task_prolog``
     - ``TaskProlog``
     - compute node, before each step
   * - ``task_epilog``
     - ``TaskEpilog``
     - compute node, after each step
   * - ``srun_prolog``
     - ``SrunProlog``
     - srun node, before step dispatch
   * - ``srun_epilog``
     - ``SrunEpilog``
     - srun node, after step completion

``[notifications]``
-------------------

Job-event notification transports.

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
   :widths: 26 12 16 46

   * - Field
     - Type
     - Default
     - Description
   * - ``suspend_timeout_secs``
     - integer
     - none
     - Idle seconds before a node is suspended.
   * - ``suspend_command``
     - string
     - none
     - Suspend command; ``{node}`` is replaced with the node name, e.g.
       ``"systemctl suspend"``.
   * - ``resume_command``
     - string
     - none
     - Resume command; ``{node}`` is replaced, e.g. ``"ipmitool chassis power on"``.

Kubernetes modes
----------------

Spur has two distinct, mutually exclusive Kubernetes modes. ``[kubernetes]`` lets
Spur run **inside** an existing cluster and accept ``SpurJob`` CRDs; ``[cluster]``
lets Spur **own** and provision a k0s cluster.

``[kubernetes]``
~~~~~~~~~~~~~~~~

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

.. list-table::
   :header-rows: 1
   :widths: 24 12 24 40

   * - Field
     - Type
     - Default
     - Description
   * - ``enabled``
     - bool
     - ``false``
     - Enable the Spur-managed k0s cluster.
   * - ``distro``
     - string
     - ``"k0s"``
     - Kubernetes distribution. Only ``"k0s"`` is supported.
   * - ``pod_cidr``
     - string
     - ``"10.42.0.0/16"``
     - Pod network CIDR. Prefix must be ``<= /24`` (per-node /24 carving).
   * - ``service_cidr``
     - string
     - ``"10.43.0.0/16"``
     - Service network CIDR.
   * - ``cni``
     - string
     - ``"kuberouter"``
     - CNI mode: ``"kuberouter"`` (k0s default) or ``"calico"`` (bird native routing
       over the mesh).
   * - ``cni_mtu``
     - integer
     - ``1450``
     - CNI MTU, leaving headroom for WireGuard overhead.
   * - ``control_plane_node``
     - string
     - none
     - Hostname running the k0s control plane; empty picks one from inventory.
   * - ``storage_provisioner``
     - string
     - ``"local-path"``
     - ``"local-path"`` ships a default node-local StorageClass; ``"none"`` disables
       it. Other values are rejected.
   * - ``local_path_dir``
     - string
     - ``/var/lib/local-path-provisioner``
     - On-node directory for local-path PVs. Must be absolute and free of quotes,
       backslashes, whitespace, and control characters.

See :doc:`/deployment/managed-kubernetes` for provisioning a Spur-owned cluster.

``[federation]``, ``[topology]``, ``[burst_buffer]``
----------------------------------------------------

``[federation]``
   Peer clusters for cross-cluster job routing. Each ``[[federation.clusters]]``
   entry has ``name`` (string) and ``address`` (string, e.g.
   ``"http://peer-ctrl:6817"``). Defaults to no peers.

``[topology]``
   Optional switch-hierarchy configuration for locality-aware scheduling.
   ``plugin`` (string, default ``"none"``) selects the model: ``"tree"`` for a
   switch hierarchy, ``"block"`` for fixed-size blocks, or ``"none"`` to disable.
   In tree mode, each ``[[topology.switches]]`` entry has ``name`` (string),
   ``nodes`` (hostlist pattern for a leaf switch), and ``switches`` (comma-separated
   child switch names for an aggregation switch). In block mode, ``block_size``
   (integer) sets the number of nodes per block. Defaults to no topology.

``[burst_buffer]``
   Burst-buffer capacity. ``total_gb`` (integer, default ``0``) sets total capacity
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
