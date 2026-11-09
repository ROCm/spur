Upgrading Spur
==============

This page covers upgrading Spur binaries, whether on a single host or across a whole
cluster. It describes the ``spur self-update`` self-updater, re-running ``install.sh``,
and the Ansible playbooks that upgrade a running cluster with or without an outage.

.. note::

   There are two upgrade scopes: **upgrading the binaries on a single host** and
   **upgrading a whole cluster**. There is **no in-process hot-swap** that preserves
   running jobs inside a single daemon process. ``spur self-update`` only swaps the
   binaries on disk — it does **not** restart the daemons. The drain-aware, jobs-preserving
   cluster path is the Ansible ``rolling_upgrade.yml`` playbook (see below).

Single-Host: ``spur self-update``
---------------------------------

``spur self-update`` downloads the latest release and replaces the ``spur``, ``spurctld``,
``spurd``, ``spurstepd``, and ``spurauthd`` binaries on the current host. It is a
single-host convenience: it does not restart daemons and gives no drain or quorum
protection, so it is not a substitute for the cluster playbooks below.

Check whether an update is available:

.. code-block:: bash

   spur version --check

.. code-block:: text

   update available: 0.3.0 → v0.3.1
   Run `spur self-update` to install.

Install the update:

.. code-block:: bash

   spur self-update

``spur update`` is an alias for ``spur self-update``. Add ``--nightly`` to pull from the
nightly channel instead of the latest stable release:

.. code-block:: bash

   spur self-update --nightly

The updater downloads the release tarball, verifies it against its published SHA256
checksum, then replaces each binary atomically: the current binary is renamed to
``<name>.spur-old`` as a backup, the new binary is copied into place, and the backups are
deleted on success. If any copy fails, the ``.spur-old`` backup is restored. The install
directory is auto-detected as wherever the running ``spur`` binary already lives.

After a successful update the CLI prints:

.. code-block:: text

   Updated spur to v0.3.1
   Note: Restart running daemons (spurctld, spurd, spurauthd) to use the new version.

.. warning::

   ``spur self-update`` never restarts daemons. Running ``spurctld`` and ``spurd``
   processes keep executing the old binary until you restart them yourself:

   .. code-block:: bash

      sudo systemctl restart spurctld spurd

   If you run ``spurauthd``, restart it too.

The ``[update]`` config block
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

The optional ``[update]`` block in ``spur.conf`` controls the daemon startup update check.
Its fields are:

.. list-table::
   :header-rows: 1

   * - Field
     - Default
     - Effect
   * - ``check_on_startup``
     - ``true``
     - Check the GitHub releases API when the daemon starts and log if an update exists.
   * - ``auto_update``
     - ``false``
     - Download and install an available update automatically. Even when ``true``, the
       daemon is **never** auto-restarted.
   * - ``channel``
     - ``"stable"``
     - Release channel to check: ``"stable"`` or ``"nightly"``.
   * - ``cache_dir``
     - ``"/var/cache/spur"``
     - Directory for the update-check cache (1-hour TTL).

.. note::

   Even with ``auto_update = true``, a new binary on disk does not take effect until the
   daemon is restarted. The config block applies to ``spurctld``; ``spurd`` never
   auto-installs updates.

See :doc:`/admin-guide/configuration` for the full ``[update]`` field reference.

Single-Host: Re-running ``install.sh``
--------------------------------------

Re-running the one-line installer upgrades an existing single-host install in place. It
downloads the requested release, verifies its checksum, and copies the binaries over the
install directory (default ``~/.local/bin``):

.. code-block:: bash

   curl -fsSL https://raw.githubusercontent.com/ROCm/spur/main/install.sh | bash

Pass ``nightly`` or a pinned ``vX.Y.Z`` to select a specific release, or set
``INSTALL_DIR`` to install elsewhere:

.. code-block:: bash

   curl -fsSL https://raw.githubusercontent.com/ROCm/spur/main/install.sh | bash -s -- nightly
   curl -fsSL https://raw.githubusercontent.com/ROCm/spur/main/install.sh | bash -s -- v0.3.1

Like ``spur self-update``, ``install.sh`` does not manage systemd units or restart
daemons. Restart ``spurctld`` and ``spurd`` yourself after re-installing.

Cluster Upgrades with Ansible
-----------------------------

For a multi-node cluster, the Ansible toolkit is the recommended upgrade path. Two
playbooks are supported; both reuse the same install, config, and health-check roles as
``deploy.yml``, so their behavior stays consistent.

Rebuild all binaries from the same source tree together — they share a Raft
write-ahead-log schema, and mixing binaries from different builds can leave a controller
unable to parse a log written by a differently-versioned peer:

.. code-block:: bash

   cargo build --release -p spur-cli -p spurctld -p spurd -p spur-stepd

Binaries roll out by content, not version string: Ansible compares checksums, so an
unchanged re-run is a near no-op.

Full convergence (``deploy.yml``)
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

``deploy.yml`` is the simplest upgrade: it re-installs binaries and restarts every daemon
on every host in one play. This causes a **brief cluster-wide outage** — in-flight jobs are
disrupted, and in an HA setup all controllers bounce together, briefly losing the Raft
leader. It is non-destructive by default (state is preserved). Use it for topology changes
or when a short blip is acceptable:

.. code-block:: bash

   ansible-playbook playbooks/deploy.yml -i inventory/hosts.ini -e spur_binary_src=/path/to/target/release

.. note::

   Spur 0.3.0 has no online Raft membership change. Adding, removing, or reordering a
   **controller** fails early unless you also pass ``-e spur_wipe_state=true`` (a Raft
   reinit that wipes state). Compute agents are not Raft members, so they can be added or
   removed freely without a wipe.

Rolling upgrade (``rolling_upgrade.yml``)
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

``rolling_upgrade.yml`` is the seamless, no-full-outage path: it upgrades one host at a
time so the cluster keeps scheduling and running jobs throughout.

.. code-block:: bash

   ansible-playbook playbooks/rolling_upgrade.yml -i inventory/hosts.ini -e spur_binary_src=/path/to/target/release

The playbook proceeds in order:

1. **Guard rails.** Abort if ``spur_wipe_state=true`` (never wipe Raft mid-upgrade), if
   ``spur_transport=wireguard`` (not yet supported by this playbook), or if the cluster is
   not already healthy (``spur nodes`` must return success).
2. **Upgrade controllers one at a time** (``serial: 1``, no failures tolerated), preserving
   Raft quorum. On a multi-controller cluster the playbook probes each controller first and
   upgrades **the Raft leader last** (see :ref:`upgrading-leader-last`). Each controller's
   binaries are force-reinstalled and the daemon restarted; the "wait for Raft leader" step
   is the health gate before moving to the next controller. The existing ``spur.conf`` is
   preserved unless you pass ``-e spur_overwrite_conf=true``.
3. **Drain and upgrade agents in batches.** For each agent: drain it from the controller
   (``spur node drain <node> --reason "ansible rolling upgrade"``), wait until its state is
   ``DRAINED`` or ``DOWN`` (running jobs finish first — drain never force-kills), swap the
   binary, restart ``spurd``, wait for the node to re-register, then resume it
   (``scontrol update NodeName=<node> State=RESUME``).

   ``spur_in_place_upgrade=true`` changes this step: the node is **never drained and never
   resumed**, so it stays schedulable for the whole swap. See :ref:`upgrading-in-place`.
4. **Verify.** Submit a real test job to confirm the upgraded cluster schedules work.

The rolling upgrade is controlled with these ``-e`` flags:

.. list-table::
   :header-rows: 1

   * - Flag
     - Default
     - Effect
   * - ``spur_binary_src=<dir>``
     - *(unset)*
     - Directory of pre-built ``spur``/``spurctld``/``spurd`` binaries to roll out.
       Unset → install the published release via ``install.sh`` (``spur_version``).
   * - ``spur_rolling_batch_size=<N>``
     - ``1``
     - Agents upgraded per batch. Controllers are always upgraded one at a time regardless.
   * - ``spur_ignore_unreachable_agents=true``
     - ``false``
     - Skip agents unreachable over SSH instead of aborting.
   * - ``spur_skip_busy_agents=true``
     - ``false``
     - Leave a still-busy node on its current binary and continue, rather than aborting.
   * - ``spur_force_upgrade_busy_agents=true``
     - ``false``
     - Kill running jobs and containers on a busy node and upgrade it anyway. Affected jobs
       are marked ``NODE_FAIL``.
   * - ``spur_in_place_upgrade=true``
     - ``false``
     - Non-disruptive mode: never drain any agent, and upgrade a busy node's ``spurd``
       without killing its running job, trusting ``spurstepd`` to carry it across.
       Ignores ``spur_drain_wait_secs`` — nothing drains, so there is no wait.
       Mutually exclusive with the two flags above. See :ref:`upgrading-in-place`.
   * - ``spur_overwrite_conf=true``
     - ``false``
     - Re-render ``spur.conf`` from the Ansible template. Needed only when inventory
       variables changed; otherwise the existing config is preserved.

.. note::

   A larger ``spur_rolling_batch_size`` upgrades faster but drains more capacity at once. A
   single-controller cluster still has a short outage while its own controller restarts —
   true zero-downtime requires an HA quorum of 3 or more controllers.

.. _upgrading-in-place:

Non-disruptive in-place upgrade
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

.. code-block:: bash

   ansible-playbook playbooks/rolling_upgrade.yml -i inventory/hosts.ini \
     -e spur_binary_src=/path/to/target/release -e spur_in_place_upgrade=true

Every other busy-node strategy drains the node first and resumes it afterwards, which makes
it unschedulable for the length of the swap. ``spur_in_place_upgrade=true`` issues neither,
so the controller's view of the node never changes and an operator's own pre-existing drain
is never cleared.

A running job is carried across by ``spurstepd``, the per-job supervisor that lives outside
``spurd``'s process lifetime: ``spurd`` restarts, the new binary rediscovers the live
session and re-adopts it, and the payload is never signalled. The mode still refuses a busy
node that has no ``spurstepd`` installed yet — trusting a supervisor that is not there would
be unsafe — and is mutually exclusive with ``spur_skip_busy_agents`` and
``spur_force_upgrade_busy_agents``.

The trade-off runs the opposite way from the drained modes. Because nothing stops the
scheduler, a **new** job can be dispatched onto the node during the few seconds ``spurd`` is
down and may fail to launch. Already-running supervised work is unaffected.

Two further consequences of not draining:

- ``spur_rolling_batch_size`` behaves differently. Elsewhere it trades speed for drained
  capacity; here nothing drains, so a batch of N instead puts N agents' ``spurd`` down
  simultaneously while all N stay schedulable, widening the dispatch window above N-fold
  rather than serialising it. Keep the default of ``1``.
- An agent unreachable over SSH is left schedulable and not upgraded. Every other mode still
  drains it, because the drain is a controller-side call that works on a host Ansible cannot
  reach. That is usually desirable — a node whose ``spurd`` is healthy but whose SSH is down
  should keep taking work — but a genuinely dead node stays schedulable until it crosses its
  own heartbeat timeout and is marked ``DOWN``.

.. note::

   ``sbatch`` work — plain, containerised and multi-node — rides the swap
   untouched. A buffered ``srun`` client reconnects after a controller restart
   while its allocation remains active, making up to 30 attempts with a two-second
   backoff (roughly a minute, plus connection and RPC time). If the controller
   returns within that retry window, the client resumes waiting for the step
   or retrieves its recorded result. A longer outage can exhaust the retries
   and leave a live payload without its client; ``scancel`` reaps it cleanly.
   This is more likely on a single-controller cluster, where there is no peer
   to fail over to. See :ref:`srun-controller-restart` for output retention and
   replay limits.

.. _upgrading-leader-last:

Controller order: the leader goes last
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

On a multi-controller cluster ``rolling_upgrade.yml`` probes each controller before touching
any of them and upgrades the Raft leader **last**. This applies to every busy-node mode, not
only ``spur_in_place_upgrade``: it is gated solely on there being more than one controller
and no explicit ``spur_controller_upgrade_order``.

This matters because of how a mixed-version cluster replicates. ``WalOperation`` is an
externally tagged enum with no tolerant unknown-variant path, and the Raft transport
deserializes an entire ``AppendEntries`` payload in one call. If a release adds a WAL
variant and the new build reaches the leader first, every still-old follower rejects the
appends carrying that variant and **stops replicating** until it is itself upgraded. On a
three-controller cluster that costs quorum. While the leader is still on the old build it
cannot emit a new variant at all, which is what closes the window.

Detection is a read-only ``GET`` to each controller's own loopback metrics listener
(``http://127.0.0.1:6822/metrics/nodes``), which answers ``200`` on the leader and ``503``
on a follower. It never blocks the upgrade: if no controller answers as leader, or more than
one does, the playbook warns and falls back to inventory order. Pass
``-e spur_controller_upgrade_order=ctl-2,ctl-0,ctl-1`` to override it entirely.

.. warning::

   Leader-last shrinks the mixed-version window but cannot remove it. If leadership moves
   onto an already-upgraded controller mid-run and something then writes a new WAL variant —
   ``scancel`` and preemption are the usual triggers — a not-yet-upgraded follower stalls.
   Quorum survives, and the stalled follower catches up as soon as it is upgraded, so this
   is degraded-but-self-healing rather than an outage. The stalled follower logs nothing
   itself; the rejection is only visible on the leader.

Safe Upgrade Order
~~~~~~~~~~~~~~~~~~~

Follow this order for any cluster upgrade:

1. **Rebuild all binaries together** from the same source tree — they share a Raft
   WAL schema and must stay version-matched.
2. **Upgrade controllers before agents.** Both playbooks do this automatically, one
   controller at a time to preserve quorum.
3. **Drain agents before swapping binaries.** The rolling playbook drains automatically; a
   running job blocks the swap unless you force it. The exception is
   ``spur_in_place_upgrade=true``, which deliberately never drains and relies on
   ``spurstepd`` to carry running jobs across instead (see :ref:`upgrading-in-place`).
4. **Never wipe state during an upgrade.** Keep the default ``spur_wipe_state=false``.
   Wiping resets the Raft job-id counter and destroys accounting history.
5. **HA membership is fixed at init** in Spur 0.3.0. You can roll new binaries onto the
   existing controller set freely, but changing *which* hosts are controllers requires
   ``deploy.yml -e spur_wipe_state=true``.
6. **Only roll forward across an accounting schema migration.** The first upgraded
   controller applies pending PostgreSQL migrations on startup, so upgrade controllers
   before agents and let each migration finish before the next controller starts. One
   such migration widens the ``jobs`` job-id columns to 64-bit: it rewrites the table
   under an ``ACCESS EXCLUSIVE`` lock, so its duration scales with the row count and
   accounting writes block until it completes. **Rolling back to a pre-migration
   controller is not supported** — an older controller reads the widened columns as
   32-bit and its accounting queries fail against a migrated database. Take a database
   backup before upgrading if you need a recovery path.
7. **Roll forward, not back.** The Raft log gains entries — new job states, pending
   reasons, operations — as Spur evolves, and a controller replaying a log written by a
   newer build cannot parse them. A newer controller reads older logs fine, so the
   supported recovery from a bad upgrade is to roll forward, not to reinstall the
   previous version over a log the new one has already written.

.. warning::

   **Upgrading to the release that introduces** ``spurstepd`` **requires an empty
   cluster.** Drain every node and let all running jobs finish, or cancel them,
   before swapping binaries. Sessions written by the previous build are not
   adopted, the two builds disagree on where a step's processes live, and a
   mixed-version cluster is not supported across this upgrade — a new controller
   dispatching to a not-yet-upgraded agent tears the job down. Upgrade every
   controller and agent in the same maintenance window.

   ``spurstepd`` is a new binary and must be installed next to ``spurd`` on every
   compute node. The agent resolves it beside its own executable and does not
   search ``$PATH``, so if it is missing every job launch on that node fails.

   ``spur_mpi_pmix.so`` is versioned against the binaries that load it, and this
   release bumps that version. Replace the plugin in ``[mpi].plugin_dir`` in the
   same window, or ``--mpi=pmix`` jobs fail with ``unsupported MPI plugin API
   version``. The plugin is now loaded by ``spurstepd`` rather than ``spurd``, for
   steps as well as jobs.

.. note::

   *Once this release is in place*, restarting ``spurd`` no longer kills the work
   on that node: jobs, ``srun`` steps and held allocations run under supervisors
   that outlive the agent, and the restarted agent re-adopts them. This requires
   ``KillMode=process`` in the ``spurd`` unit — the systemd default,
   ``control-group``, kills the whole cgroup on stop and takes the supervisors
   with it. See :doc:`native-host` for the unit file and for the two launches
   that remain unsupervised. Draining first is still the recommended order
   because it keeps new work off a node mid-swap.

Behavior Changes Between Releases
---------------------------------

Some releases change how an existing ``spur.conf`` behaves without that file being
edited. Review these before rolling binaries out.

.. _cgroup-upgrade-notes:

Job resource enforcement (``[cgroup]``)
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

The release introducing the ``[cgroup]`` section changed what ``spurd`` writes for
a config that has **no** ``[cgroup]`` section. The first two can affect jobs that
ran fine before the upgrade; the rest relax an existing bound.

.. list-table::
   :header-rows: 1
   :widths: 22 26 26 26

   * - Control file
     - Before
     - After
     - Effect
   * - ``memory.max``
     - unset for ``--mem-per-cpu`` jobs
     - the memory the scheduler allocated
     - **A ``--mem-per-cpu`` job that ran unbounded is now capped, and is
       OOM-killed if it overruns.** Only the per-node request was read before,
       which those jobs do not set.
   * - ``memory.high``
     - unset
     - equal to ``memory.max``
     - **Memory-heavy jobs stall in reclaim before the OOM kill**, costing
       throughput.
   * - ``cpu.max``
     - CFS quota sized from ``--cpus-per-task``
     - unset
     - Relaxed: the cpuset is the CPU bound, as in Slurm.
   * - ``cpuset.cpus``
     - sized from ``--cpus-per-task``
     - sized from the node allocation
     - Relaxed: a multi-task job gets all the cores it was granted, not one
       task's worth.

To keep the previous behavior, put this in ``spur.conf`` on every compute node
**before** restarting ``spurd``:

.. code-block:: toml

   [cgroup]
   cpu_quota = true  # restore the CFS quota

``spurd`` reads ``[cgroup]`` only at startup, so this has to be in place before the
restart — ``scontrol reconfigure`` will not apply it afterwards.

.. note::

   Neither memory change has a knob of its own: ``memory.max`` and ``memory.high``
   are both written whenever ``constrain_ram_space`` is on, matching Slurm's
   ``cgroup/v2`` plugin. Setting ``constrain_ram_space = false`` restores the old
   behaviour but drops the memory ceiling for *every* job, not just the
   ``--mem-per-cpu`` ones. Prefer raising ``allowed_ram_percent`` (for example
   ``125``), which gives jobs headroom above their allocation and starts reclaim
   there, over turning memory constraints off.

Rolling back is safe with the section left in place: older binaries do not know
``[cgroup]`` and ignore it.

Job device access (``constrain_devices``)
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

The release introducing ``constrain_devices`` attaches a default-deny BPF device
filter to every job cgroup, and it defaults to **on** — including for a
``spur.conf`` with no ``[cgroup]`` section, or one written before the field
existed.

**Effect:** a batch payload can open only the device nodes its allocation granted,
plus the base pseudo-devices and a built-in host-infrastructure set. A job that had
been reaching a GPU it was not allocated — by re-exporting ``ROCR_VISIBLE_DEVICES``,
or because it never read the variable — now fails that ``open`` with ``EPERM``.

**A job can also be denied a host device node its allocation never covered**, and
that is the risk to plan the rollout around. Some device nodes belong to the node
rather than to any job, so no allocation hands them out and nothing puts them in the
allow-list. The main case is RDMA/InfiniBand verbs under ``/dev/infiniband/`` —
needed by MPI, and by NCCL or RCCL over InfiniBand, which makes this reach
**non-GPU** jobs. These are allowed by default, so the common cases keep working with
no configuration; see :ref:`device-filter-implicit-allow` for the full implicit set.

The vendor control nodes a GPU runtime initializes through, such as ``/dev/nvidiactl``
and ``/dev/nvidia-uvm``, reach a job through its allocation's CDI device edits. A node
configured through GRES rather than a CDI spec does not list them as devices, so a GPU
job there fails with ``EPERM`` on those nodes until they are named in
``extra_device_paths``.

A site whose jobs open some other host device node will still see ``EPERM``. Name
those paths rather than turning the filter off:

.. code-block:: toml

   [cgroup]
   extra_device_paths = ["/dev/some-site-device"]

Roll the agent out node by node and watch job output for ``EPERM`` on device paths.
To defer the change entirely:

.. code-block:: toml

   [cgroup]
   constrain_devices = false

Installing the filter needs ``CAP_BPF`` and ``CAP_NET_ADMIN`` (or ``CAP_SYS_ADMIN``);
a cgroup-device program is a net-admin program type, so ``CAP_BPF`` alone is not enough.
An agent without them logs ``device filter not installed`` and runs the job with no
device isolation, so an unprivileged ``spurd`` behaves as before — unless ``required = true``, which
turns that degradation into a refused launch.

Reserved step IDs aligned to Slurm
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

This release changes the integer IDs of the reserved batch, extern, and interactive
steps to Slurm's exact ``slurm.h`` sentinels (``SLURM_BATCH_SCRIPT``,
``SLURM_EXTERN_CONT``, ``SLURM_INTERACTIVE_STEP``). These IDs are persisted in the Raft
snapshot, embedded in each node's stepd runtime state (session directory names, launch
descriptors, PMIx namespaces), and carried on the gRPC wire, so old and new builds
disagree on which integer means which step.

.. warning::

   **This upgrade requires an empty cluster.** Drain every node and let all running jobs
   finish, or cancel them, before swapping binaries, and upgrade every controller and
   agent in the same window. A job in flight across the upgrade has its reserved step
   recorded under the old ID: the new build no longer recognizes it as
   batch/extern/interactive, so it reports the raw integer and its supervisor and cgroup
   are not reaped correctly. Rolling back is not supported once a new controller has
   written a snapshot carrying the new IDs.

On a drained cluster the change is invisible: user-facing output already renders these
steps as ``batch``/``extern``/``interactive`` rather than the integer, so no scripts or
CLI output change.

Job and usage visibility (``private_data``)
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

Earlier releases pinned identified callers with no way to turn it off:

- v0.9 to v0.14 pinned the gRPC job list (``squeue``, ``sprio``) to the caller's
  own jobs, Operators included through v0.12.
- v0.11 to v0.14 pinned ``scontrol show assoc_mgr`` to the caller's own scopes.
- v0.13 and v0.14 also pinned REST ``/jobs`` and returned not found for another
  user's job from ``scontrol show job``, ``scontrol show step``, ``sstat``, and
  REST ``/job/{id}``.

The release introducing :ref:`[auth] private_data <private-data>` makes this
opt-in and defaults to Slurm's behavior: every user sees every job and every
assoc_mgr scope.

**Effect:** with an unchanged ``spur.conf``, a User now sees other tenants' jobs
and steps, including their command (the first non-comment line of the batch
script), working directory, and output paths. A step command such as
``srun --jobid`` against another user's job now fails with a permission error
instead of "job not found". To keep the previous visibility, set this on every
controller **before** upgrading:

.. code-block:: toml

   [auth]
   private_data = ["jobs", "usage"]

.. warning::

   ``rolling_upgrade.yml`` and ``deploy.yml`` regenerate ``spur.conf`` from the
   toolkit template, which has no ``[auth]`` section, and restart the controllers.
   On a toolkit-managed cluster, every job is visible from that restart until you
   re-add ``private_data`` and restart every controller again. Every later run of
   either playbook drops it again.

A request from a User that names another user no longer falls back to the caller's
own jobs. In v0.14, ``squeue -u bob``, ``sprio -u bob``, and REST
``/jobs?user=bob`` returned the caller's own jobs, and ``scancel -u bob``
cancelled them. Under ``"jobs"`` these now return nothing and cancel nothing, as
in Slurm. Without ``private_data`` they list bob's jobs.

Without ``private_data``, ``scancel -p``, ``-A``, or ``-n`` also selects other
users' jobs. Each one the caller does not own is refused and reported on
``stderr``.

A cluster that already sets ``"jobs"`` now also hides other users' ``sacct``
history and ``sreport job`` sizes. A cluster that already sets ``"usage"`` now
also hides other users' ``sshare`` and ``sreport`` utilization figures. ``sprio``
stays with ``"jobs"``. Set the categories only where that narrower view is what
you want, and upgrade ``spur-cli`` together with ``spurctld``.

``scontrol reconfigure`` applies ``private_data`` on the leader only; restart every
controller after changing it. During a rolling upgrade, visibility depends on which
controller serves the request, but it is never wider than the new configuration
allows.

``spurctld`` refuses to start on a category it does not support. Agents and the CLI
ignore the field, so a ``spur.conf`` naming a category from a newer release still
loads on them.

Rolling back to v0.13 or v0.14 is safe with the field left in place: those releases
ignore it and pin Users to their own jobs and assoc_mgr scopes regardless. v0.12 and
earlier also pin Operators to their own job list, but leave REST ``/jobs`` and
``scontrol show job`` unpinned.

scancel exit status on a refused cancel
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

This release makes ``scancel`` exit ``1`` when the controller refuses a cancel or signal
(for example, cancelling a job you do not own). It previously always exited ``0`` and only
printed to ``stderr``. Scripts that call ``scancel`` and branch on its exit code now see the
failure. A job that is already finished or unknown still does not fail a plain cancel (or
``-s KILL``), and ``--quiet``/``-Q`` hides the report but not the exit status.
Job preemption policy (``preempt_type``)
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

The release introducing ``scheduler.preempt_type`` makes preemption **opt-in**
and narrows what makes a running job eligible. Both changes apply to a
``spur.conf`` that is not edited.

.. list-table::
   :header-rows: 1
   :widths: 22 26 26 26

   * - Behavior
     - Before
     - After
     - Effect
   * - Whether preemption runs
     - On whenever the pending job's partition set a non-``off``
       ``preempt_mode``
     - Off unless ``scheduler.preempt_type = "qos_priority"``
     - **A cluster that preempted before stops preempting entirely** until the
       new field is set.
   * - What makes a job eligible
     - An effective-priority gap of more than 2×
     - The pending job's QOS lists the victim's QOS in ``preempt`` **and** has
       a strictly higher QOS ``priority``
     - **Preemption no longer fires without a QOS allow-list.** Fair-share, job
       age, and ``--priority`` no longer affect eligibility at all;
       ``priority_tier`` matters only for the active-reservation guard.

To restore preemption, enable the engine in ``spur.conf`` on the controller:

.. code-block:: toml

   [scheduler]
   preempt_type = "qos_priority"

then give each preempting QOS an allow-list and a higher priority than the QOS
it should displace:

.. code-block:: bash

   sacctmgr modify qos name=burst set priority=100
   sacctmgr modify qos name=normal set priority=10000 preempt=burst

Both are live: ``preempt_type`` applies on ``scontrol reconfigure``, and QOS
changes take effect without a restart. A victim still needs a resolved action
that is not ``off``: its QOS ``preemptmode`` if set, else the most aggressive
of its partitions' ``preempt_mode``. See :doc:`/admin-guide/accounting` for
the full decision table.

A pending job's own partition ``preempt_mode`` no longer gates whether it may
preempt. Previously a job sitting in an ``off`` partition could never trigger
preemption; now only the victim's scope decides. Partitions used to hold
pending work away from preemption need their *victims* protected instead.

**QOS ``preemptmode=off`` is now a real protection, not a no-op.** It used to
mean "no QOS override, defer to the partition"; it now always wins over the
partition, including to protect a job the partition would otherwise let
through. Every pre-upgrade row holding the literal ``off`` behaved as unset
under the old resolution rule, so a one-time migration resets those rows back
to unset — they keep deferring to their partition exactly as before. Only a
QOS an admin sets to ``off`` *after* upgrading gains the new protection.

Leaving ``preempt_type`` at its ``"none"`` default is safe but silent: nothing
logs that preemption was skipped, so verify with a test job rather than
assuming the old behavior carried over.

.. _inactive-limit-upgrade-notes:

Reaping abandoned interactive allocations (``inactive_limit_secs``)
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

This release turns ``scheduler.inactive_limit_secs`` on by default (``300``),
including for a ``spur.conf`` with no such line. It defaulted to ``0`` (off).

.. list-table::
   :header-rows: 1
   :widths: 22 26 26 26

   * - Behavior
     - Before
     - After
     - Effect
   * - An ``salloc`` or standalone ``srun`` whose client is gone or stopped
     - Held its nodes until the time limit
     - Reaped once its client has been silent for 300 seconds; the job ends
       ``TIMEOUT``
     - **A non-PTY standalone ``srun`` whose client is gone no longer runs to
       completion**, for example ``srun ./train.sh &`` started from an SSH
       session that then closed.
   * - Which allocations a set limit reaps
     - Every interactive allocation
     - Only allocations whose client sends keepalives: ``salloc``/``srun`` from
       this release, or an older v0.9+ CLI once it has pinged
     - The K8s operator, third-party API clients that do not send keepalives,
       and CLIs older than v0.9 stay exempt.
   * - A session that outlives its user's token under ``plugin = "jwt"``
     - Ran to its time limit
     - Reaped about 300 seconds after the token expires, even with its client
       still running
     - Issue user tokens that outlive the longest interactive time limit. A
       token's lifetime counts from when it was minted.

To keep the previous behavior, set the limit to ``0`` in ``spur.conf`` on the
controller **before** upgrading:

.. code-block:: toml

   [scheduler]
   inactive_limit_secs = 0

The field is live and applies on ``scontrol reconfigure``. Jobs submitted while
an older controller ran carry no keepalive mark, so they are reaped only if
their client pings the upgraded controller. ``scontrol show config`` prints the
value in effect as ``InactiveLimit``. See :ref:`inactive-limit`.

GPU partition identities (CPX, QPX, DPX)
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

This release changes the allocation id (``stable_id``) of partitions 1 and up of a
GPU that runs in a partitioned compute mode. Partition 0 and GPUs in SPX mode keep
their ids. The id is the GPU allocation key in the Raft log and in the job state of
``spurd``. Old log entries still deserialize.

The ids change when ``spurd`` regenerates the CDI spec (``spur k8s up``), or at a
restart on a node that has no ``/etc/cdi/amd.json``. A binary swap alone does not
change them while that file exists.

.. note::

   ``spurd`` carries a running job's GPU ids across this change, both at a
   restart and when it rewrites the CDI spec: it saves the last
   controller-acknowledged inventory to disk and recognizes the old encoding
   against it, or, on the first restart after upgrade with no saved inventory
   yet, recognizes the old encoding directly. When the node re-registers, the
   controller remaps the job's allocation to the new ids. Upgrade the
   controllers before the agents: an older controller does not remap
   allocations when a node re-registers, and an upgraded agent then rejects
   dispatches with
   ``controller-allocated GPUs are not in this node's gpu inventory``.
   With the controllers upgraded first, jobs keep running, no
   dispatch rejection or ``JobHoldMaxRequeue`` hold results, and draining
   nodes is no longer required for the GPU id change. See
   :ref:`gpu-partition-mode-change` for the separate, still-destructive case
   of actually switching a GPU's partition mode.

Bounded controller shutdown (``shutdown_grace_secs``)
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

After ``SIGTERM``, ``spurctld`` now waits at most ``[controller] shutdown_grace_secs``
(default 10s) for in-flight RPCs to drain, then flushes accounting and exits. Previously it
waited on every in-flight RPC with no bound, and an ``srun`` step holds its RPC open for the
step's whole runtime, so restarting a controller while a step ran (for example during
``rolling_upgrade.yml``) hung until systemd's ``TimeoutStopSec`` killed it, skipping the
accounting flush.

A step still running when the grace period ends keeps running on its node. A buffered
``srun`` re-attaches to it if it reconnects to the restarted controller within its retry
window, and otherwise exits with an error while the step runs on; see
:ref:`srun-controller-restart`. ``srun --pty`` follows a different path and is not covered.
Accounting for requests still in flight at that point is best-effort: anything they write
after the flush is lost.

The default suits most sites. Keep it well below ``heartbeat_timeout_secs``: the controller
answers no heartbeats while it drains, so a grace within about 35 seconds of that timeout
lets a full drain mark nodes DOWN and fail their jobs. ``0`` restores the unbounded wait,
which can run past that timeout too, and which a repeated ``SIGTERM`` or ``SIGINT`` does not
end; only ``SIGKILL`` does. ``spurctld`` warns at startup in either case. Keep the grace,
plus up to 5 seconds for the accounting flush, below ``TimeoutStopSec``. A step that
finishes during the drain does not end it, so raising the value mostly lengthens the
restart. A new value takes effect at the next start.

.. _requeued-run-upgrade-notes:

Stopping a requeued job's run on a recovered node
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

When a node misses heartbeats, its jobs end ``NODE_FAIL`` and a job submitted
with ``--requeue`` goes back to the queue, but the job's nodes are not told to
stop when they go Down. From this release, the controller stops that run when a
node heartbeats again, even while the job is still waiting in the queue. Before,
a node kept the run and its GPUs until the job ended or ran again, and refused
every job placed on those GPUs in the meantime.

Stopping the run on heartbeat needs both an upgraded controller and an upgraded
agent on that node, which reports the run it holds; with either side from an
earlier release, the old behavior stays. An upgraded agent also stops the old
run's processes, including its ``srun`` steps, when the job is placed back on
the same node, which an earlier agent could miss for a run without a cgroup
(``[cgroup] enabled = false``, or ``spurd`` cannot create cgroups, for example
when not running as root).

Before accepting a replacement attempt, the agent fences every older attempt
it can discover in its tracked sessions, persisted session descriptors, and
``/sys/fs/cgroup/spur/job_<id>_<attempt>`` directories. This includes attempts
with reused step IDs after a restart and orphan cgroups without descriptors.
It kills old workloads and removes their complete cgroup trees, including the
parent job cgroup. Same or newer attempts and other jobs are left alone.
Batch launches and allocation registration are refused if an old cgroup cannot
be removed or an old session descriptor cannot be read; the old reservation is
not replaced. Step startup is serialized with this cleanup so an old step
cannot recreate a cgroup during replacement.
Completed steps keep their recorded exit status when a replacement reclaims
their retained sessions. An unreadable exit record also refuses replacement.

.. _kill-wait-upgrade-notes:

Kill waits and the time-limit completing state (``kill_wait_secs``, ``timeout_kill_wait_secs``)
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

This release makes the wait between SIGTERM and SIGKILL configurable with two
settings. ``scheduler.kill_wait_secs`` (default ``5``) covers ``scancel``,
``scontrol requeue``, the final kill of a preemption, idle-fill reclaim,
reservation end, the stop of a finished batch script's other nodes and the other
graceful stops. ``scheduler.timeout_kill_wait_secs``
(default ``30``) covers the stops that end ``TIMEOUT``: the time limit and the
inactivity limit. Both changes apply to a ``spur.conf`` that is not edited, and
with the defaults most waits are the ones a job had before.
``scontrol show config`` prints both values in effect as ``KillWait`` and
``TimeoutKillWait`` (in seconds, floor applied); against a controller older than
this release it omits them rather than showing a misleading ``0``.

.. list-table::
   :header-rows: 1
   :widths: 22 26 26 26

   * - Behavior
     - Before
     - After
     - Effect
   * - ``scancel`` and the final preemption kill
     - SIGKILL 5 seconds after SIGTERM
     - SIGKILL up to ``kill_wait_secs`` (5 seconds) after SIGTERM; a job that
       exits on SIGTERM is gone as soon as it exits
     - Unchanged by default. Raise ``kill_wait_secs`` to give such jobs longer.
   * - A batch job at its time limit
     - Stayed ``RUNNING`` for 30 seconds after SIGTERM, then ended ``TIMEOUT``
       and was SIGKILLed
     - Moves to ``COMPLETING`` (``CG``) at once, waits up to
       ``timeout_kill_wait_secs`` (30 seconds), then ends ``TIMEOUT`` with
       reason ``TimeLimit``
     - ``squeue`` shows ``CG`` instead of ``R`` during the wait. The final state,
       reason, timing, and ``--requeue`` behavior are unchanged. The job does
       not yet wait for its ``srun`` steps: it ends once its batch script exits
       (:ref:`kill wait note <kill-wait-steps-caveat>`).
       The kill is now the agent's, so a
       job on a hung or unreachable agent stays in ``CG`` for up to
       ``complete_wait_secs`` (300 seconds by default) before it ends ``TIMEOUT``,
       where the controller used to end it 30 seconds after SIGTERM.
   * - An idle interactive allocation reaped by ``inactive_limit_secs``
     - Stayed ``RUNNING`` for 30 seconds after SIGTERM, then ended ``TIMEOUT``
       and was SIGKILLed
     - Normally ends ``TIMEOUT`` within a moment of the SIGTERM, without
       spending the wait in ``COMPLETING``, as its work runs in ``srun`` steps
       that the wait does not yet reach (:ref:`kill wait note
       <kill-wait-steps-caveat>`). Like the batch row above, it stays ``CG``
       for up to ``complete_wait_secs`` on a hung or unreachable agent.
     - The allocation is freed sooner than before. ``--requeue`` does not apply.
   * - A requeued job (requeue-mode preemption, ``scontrol requeue``)
     - Held out of the queue for ``2 x interval_secs + 3`` seconds (5 seconds
       with the default 1 second interval)
     - Held for the larger of that and ``kill_wait_secs``: still 5 seconds with
       the defaults
     - Unchanged by default. Raising ``kill_wait_secs`` raises the hold with it,
       so the job does not restart into its own old run's kill wait.
   * - An unsupervised ``srun`` step on cancel or at a time limit
     - SIGKILL after 2 seconds
     - SIGKILL after the full kill wait that applies: 5 seconds on cancel, 30
       at a time limit
     - A step that handles SIGTERM now has time to shut down cleanly, where a
       stop reaches it (:ref:`kill wait note <kill-wait-steps-caveat>`).

Keep ``complete_wait_secs`` above the larger of the two waits plus
``[cgroup] cleanup_timeout_secs`` (60 seconds by default). A node reports only
after the job is killed and its cgroup cleanup finishes, so a shorter
``complete_wait_secs`` force-finishes the job while healthy nodes are still
working, and every node that has not reported is drained. The controller logs a
warning on startup and on reconfigure. The warning uses the controller's own
``[cgroup] cleanup_timeout_secs``, so set it there to the largest value any
node uses. ``kill_wait_secs`` and ``timeout_kill_wait_secs`` both accept
``0``-``65533``, and a value below 5 acts as 5. See
:doc:`/admin-guide/configuration` for the fields, including a
known issue with requeue: a requeued job frees its nodes while the old run is
still in its kill wait, so keep ``kill_wait_secs`` low if that matters with
requeue-mode preemption.

**Upgrade the leader last.** Every time limit and every inactivity-limit
expiry now writes a new Raft log entry, ``JobTimeLimitExpired``. A controller
that predates it cannot parse the entry and stops replicating until it is
upgraded, so the order in :ref:`upgrading-leader-last` applies, and a rollback
after the entry has been written is not supported: roll forward.

**Time-limited jobs during the rolling upgrade.** The controller sends the kill
wait with each cancel, and an agent that predates it ignores the value and
waits a fixed 5 seconds. Controllers are upgraded before agents, so a job that
reaches its time limit on a node that is not yet upgraded gets 5 seconds
between SIGTERM and SIGKILL instead of the 30 it had before, which can be too
short for a checkpoint. Upgrade the agents promptly, or drain the nodes still
on the old build before their jobs' limits arrive. Once a node is upgraded the
job gets the full ``timeout_kill_wait_secs``.

.. _preempt-grace-upgrade-notes:

Preemption grace time (``GraceTime``)
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

This release makes the per-QOS ``GraceTime`` take effect and adds a per-partition
``GraceTime`` (see :ref:`preemption grace time <qos-preempt-grace>`). A victim's
grace is its QOS value if set, else the highest value among the partitions it runs
in, else ``0``. Nothing is set by default, so nothing changes until an administrator sets
one.

**Upgrade every controller before setting a partition ``GraceTime``.** The
partition's ``grace_time`` is a new field in the partition entries of the Raft
log and snapshots, including the entry that
``scontrol update partition GraceTime=`` writes. A controller that predates it ignores the field, so a
change applied while a follower is not upgraded is lost on that follower, which
would lack the grace if it became leader, until it replays from a new snapshot.
This is already required before setting a QOS ``GraceTime`` (see below).

**Upgrade the leader last.** The first preemption that starts a grace writes a
new Raft log entry, ``JobPreemptSignaled``. A controller that predates it
cannot parse the entry and stops replicating until it is upgraded, so the order
in :ref:`upgrading-leader-last` applies, and a rollback after the entry has been
written is not supported: roll forward. The entry is written only when a
``GraceTime`` greater than ``0`` is in use.

**Upgrade controllers and agents before setting ``GraceTime``.** A node whose
``spurd`` predates this release does not advertise the ``signal-job``
capability. A victim on such a node gets no grace and is preempted at once, and
the controller logs a warning naming the node, so setting ``GraceTime`` before
the agents are upgraded silently does nothing for jobs on those nodes.

**Kubernetes operator nodes get no grace.** Victims on nodes managed by
``spur-k8s`` are preempted at once whatever the QOS or partition ``GraceTime``.

The ``grace_time`` column is added to the QOS table automatically with
``ADD COLUMN IF NOT EXISTS``, and a controller that predates this release
ignores it.

Preemption handoff
~~~~~~~~~~~~~~~~~~

Cancel and requeue preemption now hold the nodes they free for the job that
triggered the eviction, so other pending jobs no longer take them in the gap
(see :doc:`../admin-guide/configuration`). No configuration change is needed
and no new Raft entry type is added: the requeue entry gains an optional
``handoff`` field that a controller predating it ignores.

During a rolling upgrade a controller that predates this release does not hold
the nodes, so the protection applies only once the leader runs the new build.
Victim selection also counts nodes that a draining job has already released,
which changes ordinary preemption as well.

.. _upgrading-partition-resource-limits:

Partition resource limits
~~~~~~~~~~~~~~~~~~~~~~~~~

This release adds per-partition resource caps and memory limits (see
:doc:`/admin-guide/partition-resource-limits`). Existing ``spur.conf`` files
behave as before: with no limit set, nothing changes, and
``enforce_part_limits`` keeps its meaning for wall time.

Jobs already running when you upgrade have no recorded partition. Once a cap
is set, each one is charged to the highest-``priority_tier`` partition it
requested, which is the partition it ran in unless it listed several.

.. warning::

   **Upgrade every controller replica before you configure any cap or memory
   knob**, whether in ``spur.conf``, with ``scontrol``, or through the API.
   An older replica ignores the new fields, so it diverges from the leader:

   - It schedules without the caps if it becomes leader.
   - It ignores the adjusted request a ``def_mem_*`` or ``max_mem_*`` knob
     records when a job starts, so its copy of the job keeps the original
     CPU and memory request.
   - It cannot load a Raft snapshot holding a job blocked by a cap, whose
     ``PartitionTresLimit`` pending reason it does not know, either on
     restart or when the leader sends one to catch it up.

**Rolling back.** Once a cap has blocked a job, a controller from an earlier
release cannot load the snapshot. To downgrade:

#. Remove every cap and memory knob (``spur.conf`` plus ``scontrol
   reconfigure``, or ``scontrol update``).
#. Wait until no job carries the ``PartitionTresLimit`` reason. A job keeps
   its last pending reason after it ends, so this includes finished jobs:
   wait until no pending job shows the reason, then at least
   ``controller.terminal_job_retention_secs`` (default one hour) more, so
   finished jobs that had it are evicted.
#. Wait for a new Raft snapshot: on every controller,
   ``<state_dir>/raft/snapshot.json`` must be newer than the end of the
   previous step. A snapshot is taken every 5000 Raft log entries, so on a
   quiet cluster this can take a while; job submissions and completions
   move it along.
#. Downgrade the controllers.

See Also
--------

- :doc:`ansible`
- :doc:`uninstalling`
- :doc:`/admin-guide/configuration`
