Manual Deployment (systemd)
===========================

Deploy Spur by hand across physical or virtual machines: install the binaries, write a
config file, and run the daemons as systemd services. This page is the no-Ansible path.

.. note::

   For production clusters, use the Ansible toolkit instead — see :doc:`ansible`. It
   automates everything below, including systemd units, symlinks, and PostgreSQL
   accounting. Follow this page to understand the internals or to stand up a small,
   ad-hoc cluster.

Get the Binaries
----------------

Install the latest stable release with the one-line installer. By default it installs
to ``~/.local/bin`` (no sudo required):

.. code-block:: bash

   curl -fsSL https://raw.githubusercontent.com/ROCm/spur/main/install.sh | bash
   export PATH="$HOME/.local/bin:$PATH"

This installs the three binaries — ``spur``, ``spurctld``, and ``spurd`` — and makes the
CLI reachable under its Slurm-compatible names (``sbatch``, ``squeue``, ``sinfo``, …).

For ``--mpi=pmix``, use a **nightly** tarball (includes ``spur_mpi_pmix.so``);
see :ref:`mpi-pmix-install`.

To build from source instead, install the Rust toolchain and ``protobuf-compiler``, then
build the three binaries:

.. code-block:: bash

   git clone https://github.com/ROCm/spur.git && cd spur
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y && source "$HOME/.cargo/env"
   sudo apt install -y protobuf-compiler build-essential
   cargo build --release -p spur-cli -p spurctld -p spurd

The binaries land in ``target/release/``. For a fuller build walkthrough see
:doc:`/developer/building`.

.. note::

   Ports used across hosts: **6817** (controller gRPC API and accounting), **6818**
   (agent gRPC), and **6821** (Raft, controller-to-controller). Open these between the
   relevant hosts.

Daemon Flags
------------

The two daemons are configured with command-line flags. The most common are below.

``spurctld``
~~~~~~~~~~~~

.. list-table::
   :header-rows: 1

   * - Flag
     - Default
     - Meaning
   * - ``-f, --config <PATH>``
     - ``/etc/spur/spur.conf``
     - Config file. If it does not exist, built-in defaults are used.
   * - ``--listen <ADDR>``
     - *(from config)*
     - gRPC listen address; overrides the config file.
   * - ``--state-dir <PATH>``
     - ``/var/spool/spur``
     - Raft and scheduler state directory.
   * - ``--log-level <LEVEL>``
     - ``info``
     - Log verbosity.
   * - ``-D, --foreground``
     - off
     - Run in the foreground instead of daemonizing.

``spurd``
~~~~~~~~~

.. list-table::
   :header-rows: 1

   * - Flag
     - Default
     - Meaning
   * - ``-f, --config <PATH>``
     - ``/etc/spur/spur.conf``
     - Config file for local agent settings (see the note below).
   * - ``--controller <ADDR>``
     - ``http://localhost:6817``
     - Controller endpoint(s). Accepts a comma-separated list for HA failover.
   * - ``-N, --hostname <NAME>``
     - *(system hostname)*
     - Node name as it appears in ``spur nodes``.
   * - ``--address <IP>``
     - *(auto-detected)*
     - Advertised IP the controller uses to reach this agent.
   * - ``--listen <ADDR>``
     - ``[::]:6818``
     - Agent gRPC listen address.
   * - ``--log-level <LEVEL>``
     - ``info``
     - Log verbosity.

.. note::

   Node identity and networking (controller address, hostname, listen address) come
   from CLI flags. ``spurd`` also reads ``spur.conf`` for local agent settings —
   ``[hooks]``, ``[devices]`` (GRES and CDI), ``rlimits.memlock``, ``[cluster]``, and
   ``[mpi]``. If the file is absent, the agent logs a warning and falls back to
   defaults for those sections, which is fine when none of them are in use.

Quick Start: Two-Node Cluster
-----------------------------

The fastest way to get a two-node cluster running — one controller node and one
compute node, no WireGuard, plain LAN.

**On the controller node** (e.g. ``10.0.0.1``):

.. code-block:: toml

   # /etc/spur/spur.conf
   cluster_name = "my-cluster"

   [scheduler]
   plugin = "backfill"
   interval_secs = 1

   [[partitions]]
   name = "default"
   default = true
   nodes = "ALL"
   max_time = "24:00:00"

.. code-block:: bash

   sudo mkdir -p /var/spool/spur
   spurctld -D -f /etc/spur/spur.conf    # starts in the foreground; Ctrl-C to stop
                                          # use systemd for production (see below)

**On the compute node** (e.g. ``10.0.0.2``), the same ``spur.conf`` is fine or
omit it entirely — identity and networking come from flags:

.. code-block:: bash

   spurd -D \
       --controller http://10.0.0.1:6817 \
       --hostname compute-1 \
       --address 10.0.0.2

**Verify both are up**:

.. code-block:: bash

   sinfo          # shows compute-1 in idle state
   srun hostname  # runs on the compute node and prints its name

.. note::

   Ports to open between the two machines: **6817** (controller API) and **6818**
   (agent). If you add more controllers for HA, also open **6821** (Raft).

.. note::

   By default ``spurd`` refuses to run jobs submitted as UID 0 (root). This is a
   safety guard for shared clusters. On a private dev cluster where all submitters
   are trusted, enable it in ``spur.conf``:

   .. code-block:: toml

      [auth]
      allow_root_jobs = true

Setting Up the Controller
-------------------------

Initialize the network for encrypted node-to-node communication (skip this for a direct
LAN deployment):

.. code-block:: bash

   sudo spur net init --cidr 10.44.0.0/16 --port 51820

This sets up a WireGuard mesh, prints the server public key, and outputs a join command
template for workers.

Create ``/etc/spur/spur.conf``. The repository includes ``examples/spur.conf`` with the
full annotated set of fields. A minimal example:

.. code-block:: toml

   cluster_name = "gpu-cluster"

   [controller]
   listen_addr = "[::]:6817"
   hosts = ["10.44.0.1"]
   state_dir = "/var/spool/spur"

   [scheduler]
   plugin = "backfill"
   interval_secs = 1

   [network]
   wg_enabled = true
   wg_interface = "spur0"
   agent_port = 6818
   # reject_loopback_comm_addr = true   # optional: refuse agent registrations whose comm address is loopback or link-local

   [[partitions]]
   name = "gpu"
   default = true
   nodes = "gpu-node-[1-2]"
   max_time = "72:00:00"

   [[nodes]]
   names = "gpu-node-[1-2]"
   cpus = 128
   memory_mb = 512000
   gres = ["gpu:mi300x:8"]
   # address = "10.44.0.2"   # optional default comm address before the agent registers

Start the controller in the foreground to check it comes up:

.. code-block:: bash

   sudo mkdir -p /var/spool/spur
   spurctld -D -f /etc/spur/spur.conf

For production, run it as a systemd service. Copy the binary to ``/usr/local/bin`` and
use ``/var/spool/spur`` for state (the daemon default):

.. code-block:: ini

   # /etc/systemd/system/spurctld.service
   [Unit]
   Description=Spur Controller Daemon (spurctld)
   After=network-online.target
   Wants=network-online.target

   [Service]
   Type=simple
   ExecStart=/usr/local/bin/spurctld -f /etc/spur/spur.conf --state-dir /var/spool/spur --log-level info
   Restart=on-failure
   RestartSec=3
   User=root
   LimitNOFILE=65536

   [Install]
   WantedBy=multi-user.target

Enable and start it:

.. code-block:: bash

   systemctl daemon-reload
   systemctl enable --now spurctld

.. note::

   The one-line installer places binaries in ``~/.local/bin`` by default. If you install
   that way, adjust ``ExecStart`` to match — this unit assumes ``/usr/local/bin``.

High Availability
~~~~~~~~~~~~~~~~~

For HA, run ``spurctld`` on 3 (or 5) nodes with Raft consensus. Add all controller
addresses, in the same order on every controller, to the ``peers`` list in the config
(Raft uses port 6821):

.. code-block:: toml

   [controller]
   peers = [
     "10.44.0.1:6821",
     "10.44.0.2:6821",
     "10.44.0.3:6821",
   ]

Raft automatically elects a leader. Workers connect to any controller and are redirected
to the current leader.

Joining Worker Nodes
--------------------

On each worker, join the WireGuard mesh (skip for a direct LAN deployment):

.. code-block:: bash

   sudo spur net join \
       --endpoint 192.168.1.100:51820 \
       --server-key <controller-pubkey> \
       --address 10.44.0.2

Then register the worker on the controller:

.. code-block:: bash

   sudo spur net add-peer \
       --key <node-pubkey> \
       --allowed-ip 10.44.0.2/32 \
       --endpoint 192.168.1.101:51820

Start the agent:

.. code-block:: bash

   spurd -D \
       --controller http://10.44.0.1:6817 \
       --hostname gpu-node-1 \
       --address 10.44.0.2 \
       --listen [::]:6818

``--address`` sets the advertised comm address. Alternatively, set the
``SPUR_NODE_ADDRESS`` environment variable. Pass a routable IP or FQDN,
not the short hostname alone when ``/etc/hosts`` maps it to loopback.

The agent auto-detects CPUs, memory, and GPUs, then registers with the controller over the mesh.

For an HA quorum, pass every controller as a comma-separated list so the agent and CLI
fail over to a surviving node if one is unreachable. The same format works for the
``SPUR_CONTROLLER_ADDR`` environment variable:

.. code-block:: bash

   --controller http://10.44.0.1:6817,http://10.44.0.2:6817,http://10.44.0.3:6817

Repeat for each worker, incrementing the WireGuard address.

For production, run the agent as a systemd service:

.. code-block:: ini

   # /etc/systemd/system/spurd.service
   [Unit]
   Description=Spur Node Agent (spurd)
   After=network-online.target
   Wants=network-online.target

   [Service]
   Type=simple
   ExecStart=/usr/local/bin/spurd --controller http://10.44.0.1:6817 --hostname gpu-node-1 --address 10.44.0.2 --listen 0.0.0.0:6818 --log-level info
   Restart=on-failure
   RestartSec=3
   User=root
   LimitNOFILE=65536

   [Install]
   WantedBy=multi-user.target

Verify:

.. code-block:: bash

   spur net status    # WireGuard peers and handshake times (mesh only)
   spur nodes         # All registered nodes

Resource Limits (rlimits)
-------------------------

By default, ``spurd`` raises ``RLIMIT_MEMLOCK`` to unlimited for every job step
before dropping to the submitting user. This is required for InfiniBand/RDMA
verbs (``ibv_reg_mr``, ``ibv_create_cq``) and NCCL collective communication.
Without it, jobs fail with ``Cannot allocate memory`` from libibverbs.

The default can be changed in ``spur.conf``:

.. code-block:: toml

   [rlimits]
   memlock = "unlimited"   # default: RDMA/NCCL just works
   # memlock = "inherit"   # keep whatever spurd inherited
   # memlock = "1073741824" # fixed cap in bytes

.. note::

   With the default ``"unlimited"`` setting, a ``LimitMEMLOCK=infinity`` line on
   the ``spurd`` systemd unit is no longer required. The agent raises the limit
   itself while still privileged.

MPI (PMIx)
----------

Spur supports Open MPI jobs via ``--mpi=pmix`` on **single-node and multi-node**
allocations. The controller and CLI do not link libpmix; each compute node loads
``spur_mpi_pmix.so`` from ``[mpi].plugin_dir`` when a PMIx job starts.

.. _mpi-pmix-install:

Install Spur with the MPI plugin
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

**Use published tarballs** (GitHub nightly releases or your internal artifactory
mirror of the same artifact). Do **not** copy ``cargo build`` artifacts from a
developer laptop unless you have verified glibc compatibility (see
:doc:`/developer/building`).

Nightly and **stable release** tarballs include ``lib/spur/spur_mpi_pmix.so``
(``BUILD_MPI_PLUGIN=1`` in the release pipeline). After install, confirm the
plugin is present:

.. code-block:: bash

   ls "${INSTALL_ROOT}/lib/spur/spur_mpi_pmix.so"

where ``INSTALL_ROOT`` is the directory that contains ``bin/`` (see layout
below).

**On the controller and every compute agent:**

.. code-block:: bash

   # Example: install under ~/spur (binaries in ~/spur/bin)
   mkdir -p ~/spur/bin ~/spur/etc
   curl -fsSL https://raw.githubusercontent.com/ROCm/spur/main/install.sh \
     | INSTALL_DIR="$HOME/spur/bin" bash -s -- nightly

   # Or pin a specific nightly tag from GitHub / artifactory:
   # ... bash -s -- nightly-YYYYMMDD-<sha>

   export PATH="$HOME/spur/bin:$PATH"
   spur --version
   ls "$HOME/spur/lib/spur/spur_mpi_pmix.so"

``install.sh`` layout (when ``INSTALL_DIR=$HOME/spur/bin``):

.. list-table::
   :header-rows: 1

   * - Path
     - Contents
   * - ``~/spur/bin/``
     - ``spur``, ``spurctld``, ``spurd``, Slurm-compat symlinks
   * - ``~/spur/lib/spur/``
     - ``spur_mpi_pmix.so`` (when shipped in the tarball)

For a system-wide install (``INSTALL_DIR=/opt/spur/bin``), the plugin lands in
``/opt/spur/lib/spur/``.

**Agent OS prerequisites** (not bundled in the Spur tarball):

- **OpenPMIx runtime** — ``libpmix.so`` on the agent (Spur's plugin links
  against it at load time). Version must satisfy ``[mpi].pmix_min_version``.
- **Open MPI** — libraries matching how application binaries were built
  (``mpicc``, ``LD_LIBRARY_PATH``, ``OPAL_PREFIX``).

Add ``[mpi]`` to ``spur.conf`` on **all** hosts (controller and agents), with
``plugin_dir`` matching the install layout. Use an **absolute path** — TOML does
not expand ``$HOME`` or other environment variables:

.. code-block:: toml

   [mpi]
   plugin_dir = "/home/<user>/spur/lib/spur"   # e.g. when INSTALL_DIR=/home/<user>/spur/bin; or /opt/spur/lib/spur
   pmix_tmpdir = "/tmp/spur-pmix"
   pmix_min_version = "4.1.0"

**Start or restart daemons** after install or upgrade (controller first, then
agents). Example on an agent:

.. code-block:: bash

   pkill -x spurd || true
   nohup spurd --listen=[::]:6818 --config=/etc/spur/spur.conf \
     --controller http://controller.example:6817 >> /var/log/spurd.log 2>&1 &

**Verify MPI wiring** from a host with CLI access:

.. code-block:: bash

   scontrol ping
   sinfo                                    # all agents idle/ready
   srun --mpi=list                          # expect: none, pmix
   srun --mpi=pmix -n4 /path/to/hello_mpi   # single-node smoke test
   srun --mpi=pmix -N2 -n4 /path/to/hello_mpi   # multi-node smoke test

Multi-node ``--mpi=pmix`` requires a **uniform task layout**: ``-n`` must be
evenly divisible by ``-N`` (same number of tasks on every node). For example,
``-N2 -n4`` (two tasks per node) is valid; ``-N2 -n3`` is rejected at prepare
time because ranks cannot be split evenly across nodes.

For multi-node ``srun``, the command path and any binaries or scripts it
``exec``\ s must exist at the **same path on every participating agent** (for
example ``/tmp/hello_mpi`` on each node, not only on the submission host).

Expected ``hello_mpi`` output for ``-n4``: four lines with ``rank=0`` …
``rank=3`` and ``size=4`` on each.

Upgrade / rollout
~~~~~~~~~~~~~~~~~

1. Pick the new nightly (or pinned) tarball on GitHub or artifactory.
2. **Stop** ``spurctld`` and ``spurd`` on each host before replacing binaries
   (SCP or ``install.sh`` fails with "text file busy" while daemons are running).
   When copying manually, stage to ``/tmp`` then ``mv`` into ``~/spur/bin/``.
3. Run ``install.sh`` with the **same** ``INSTALL_DIR`` on the controller and
   **every** agent (replaces binaries and ``spur_mpi_pmix.so`` together).
4. Restart ``spurctld`` on the controller, then ``spurd`` on each agent.
5. Re-run the smoke tests above before returning the cluster to users.

Keep ``spurctld``, ``spurd``, and ``spur_mpi_pmix.so`` on the **same build**
across the cluster during an upgrade.

Architecture
~~~~~~~~~~~~

1. **``spurd``** loads ``spur_mpi_pmix.so`` and calls ``PMIx_server_init`` when a
   job with ``mpi = pmix`` is launched.
2. The plugin registers a namespace (``spur.<job_id>``) with Slurm-style
   topology metadata (``PMIX_NODE_MAP``, ``PMIX_PROC_MAP``, job/local size
   keys, ``PMIX_LOCAL_PEERS``, ``PMIX_LOCALLDR``, ``PMIX_TMPDIR``), then serves
   PMIx to application processes.
3. For ``-n > 1``, ``spurd`` wraps the user command in a bash script that
   **forks one process per rank**. Each child receives a full
   ``PMIx_server_setup_fork`` environment (Slurm ``mpi_p_slurmstepd_task``
   parity) via ``spur_mpi_pmix_setup_fork_env`` in the plugin.
4. The wrapper exports ``PMIX_SERVER_URI4`` / ``PMIX_SERVER_URI3`` aliases.
   Slurm-compatible ``SLURM_*`` twins remain set (same as Slurm under
   ``--mpi=pmix``).

The embedded PMIx server registers ``fence_nb`` once at ``PMIx_server_init``.
Single-node jobs never call it (OpenPMIx GDS handles modex locally). Multi-node
jobs use ``fence_nb`` to exchange modex blobs over TCP between agents (peer
addresses come from the controller allocation). The plugin does **not**
finalize/reinit PMIx when switching between single- and multi-node jobs on the
same agent.

Multi-node bootstrap uses a **two-phase** controller dispatch:

1. **PreparePmix** — each agent starts its PMIx server, binds the modex TCP
   listener, and verifies peer reachability before any rank exec.
2. **LaunchJob** with ``pmix_prepared=true`` — joins the prepared namespace and
   starts user processes.

If prepare fails on any node, the controller rolls back with **ReleasePmix** on
agents that succeeded and evicts the job with a descriptive ``state_reason``.
Partial launch failures also release prepared-but-unlaunched agents.

Modex timeouts are configurable under ``[mpi]`` (seconds; ``0`` = built-in default):

.. code-block:: toml

   modex_connect_timeout_secs = 5
   modex_fence_timeout_secs = 120
   modex_verify_timeout_secs = 30

Build the plugin from source (fallback)
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

Use this only when the tarball plugin cannot load on your agents (missing
``libpmix.so``, undefined PMIx symbols, or libpmix version skew). **Build on the
same OS/glibc as the agent**, linking against the **agent's** ``libpmix.so``.

With libpmix development files (``pkg-config pmix``):

.. code-block:: bash

   cargo build --release -p spur-mpi-pmix
   sudo install -D target/release/spur_mpi_pmix.so /usr/lib/spur/spur_mpi_pmix.so

If ``pkg-config pmix`` is unavailable, compile on the agent against **that
node's** ``libpmix.so``. Include paths vary by site — common layouts:

- ``/usr/lib/x86_64-linux-gnu/pmix2/include`` (Debian-style)
- ``/usr/mpi/gcc/openmpi-*/include`` (Open MPI bundled PMIx headers, e.g. Crusoe)

Example (adjust ``-I`` and ``libpmix`` paths for your agent):

.. code-block:: bash

   gcc -fPIC -Wall -O2 -shared -o spur_mpi_pmix.so \
     c/pmix_server.c c/modex_exchange.c \
     -Ic -Iinclude \
     -I/usr/lib/x86_64-linux-gnu/pmix2/include \
     /usr/lib/x86_64-linux-gnu/pmix2/lib/libpmix.so.2 \
     -pthread -Wl,-rpath,/usr/lib/x86_64-linux-gnu/pmix2/lib
   sudo install -D spur_mpi_pmix.so /usr/lib/spur/spur_mpi_pmix.so

Copying a plugin built on a mismatched dev environment (wrong glibc or
libpmix) can crash ``spurd`` at ``dlopen`` time.

Runtime requirements
~~~~~~~~~~~~~~~~~~~~

- **OpenPMIx** on the agent (plugin links ``libpmix`` at load time).
- **Open MPI** runtime libraries matching the application build (``mpicc`` /
  ``LD_LIBRARY_PATH`` / ``OPAL_PREFIX``). Spur does **not** invoke ``mpirun``
  for ``--mpi=pmix`` (Slurm direct-launch parity).
- Application binaries built against the **same** Open MPI install you use at
  runtime (consistent ``LD_LIBRARY_PATH`` / ``OPAL_PREFIX``).

``plugin_dir`` must match where ``install.sh`` placed ``spur_mpi_pmix.so`` (see
**Install Spur with the MPI plugin** above). Example for ``INSTALL_DIR=/opt/spur/bin``:

.. code-block:: toml

   [mpi]
   plugin_dir = "/opt/spur/lib/spur"
   pmix_tmpdir = "/tmp/spur-pmix"
   pmix_min_version = "4.1.0"

Submit PMIx jobs
~~~~~~~~~~~~~~~~

.. code-block:: bash

   srun --mpi=pmix -n4 ./hello_mpi
   srun --mpi=pmix -N2 -n4 ./hello_mpi
   sbatch --mpi=pmix -n4 batch.sh

Inside an interactive allocation (``salloc``), enable PMIx per step:

.. code-block:: bash

   srun --mpi=pmix -n4 ./hello_mpi

Minimal ``hello_mpi`` (build on the agent with ``mpicc``):

.. code-block:: c

   #include <mpi.h>
   #include <stdio.h>
   int main(int argc, char **argv) {
       int rank, size;
       MPI_Init(&argc, &argv);
       MPI_Comm_rank(MPI_COMM_WORLD, &rank);
       MPI_Comm_size(MPI_COMM_WORLD, &size);
       printf("rank=%d size=%d\n", rank, size);
       MPI_Finalize();
       return 0;
   }

Expected result for ``-n4``: four lines with ``rank=0`` … ``rank=3`` and
``size=4`` on each.

Application scripts should **avoid**:

- ``OMPI_MCA_ess=env`` — conflicts with Spur's embedded PMIx server.
- Forcing ``OMPI_MCA_pmix=ext3x`` on Open MPI 4.1 (use the default ``pmix3x``
  component, or omit the variable).
- Mixing library paths from different Open MPI installations.

Operational notes
~~~~~~~~~~~~~~~~~

- Set ``SPUR_MPI_DEBUG=1`` in ``spurd`` environment for plugin debug logs.
- Each agent holds at most 64 active PMIx namespaces; additional concurrent
  ``--mpi=pmix`` jobs on the same node fail until a job finishes.
- Single-node and multi-node PMIx jobs can run back-to-back on the same agent
  (for example a single-node smoke test followed by a multi-node job). Single-node
  jobs use local GDS modex; multi-node jobs use TCP modex via ``fence_nb``.
- Multi-node ``--mpi=pmix`` is not supported on K8s virtual agents (the
  ``spur-k8s`` in-cluster agent returns ``Unimplemented`` for ``PreparePmix``).
- Multi-node ``--mpi=pmix`` requires agent addresses in the cluster registry to
  be reachable from every node in the allocation. Hostnames and IPv4 literals
  are resolved via DNS; modex TCP listens on port ``16819 + (job_id % 8000)``.
  Only one active multi-node PMIx job should use a given port slot at a time:
  concurrent jobs whose IDs differ by a multiple of 8000 can collide.
- Modex timeouts travel with ``PreparePmix`` in ``PmixLaunchPlan`` (``0`` =
  agent ``[mpi]`` defaults). Keep ``[mpi]`` modex timeout settings identical
  across all agents when not passing explicit values.
- Multi-rank ``--mpi=pmix`` steps use the same per-rank fork + ``setup_fork``
  path as batch jobs. Spur CPU bind (``--cpu-bind``) and per-rank GPU
  partitioning (``SPUR_JOB_GPUS``) apply through the fork wrapper.

Submitting Jobs
---------------

.. code-block:: bash

   cat > train.sh << 'EOF'
   #!/bin/bash
   #SBATCH --job-name=distributed-training
   #SBATCH -N 2
   #SBATCH --ntasks-per-node=8
   #SBATCH --gres=gpu:mi300x:8
   #SBATCH --time=4:00:00

   torchrun \
       --nnodes=$SPUR_NNODES \
       --node_rank=$SPUR_TASK_OFFSET \
       --master_addr=$(echo $SPUR_PEER_NODES | cut -d: -f1) \
       --master_port=29500 \
       --nproc_per_node=8 \
       train.py
   EOF

   spur submit train.sh

Environment Variables
---------------------

Each node in a multi-node job receives:

.. list-table::
   :header-rows: 1

   * - Variable
     - Example
     - Description
   * - ``SPUR_JOB_ID``
     - ``42``
     - Job ID
   * - ``SPUR_NNODES``
     - ``2``
     - Total nodes in allocation
   * - ``SPUR_TASK_OFFSET``
     - ``0`` or ``8``
     - This node's starting task index
   * - ``SPUR_PEER_NODES``
     - ``10.44.0.2:6818,10.44.0.3:6818``
     - All nodes in the allocation
   * - ``SPUR_CPUS_ON_NODE``
     - ``128``
     - CPUs allocated on this node

GPU Isolation
-------------

Spur automatically restricts GPU visibility per job by exporting the allocated
device ordinals into the standard GPU runtime variables:
``ROCR_VISIBLE_DEVICES``, ``CUDA_VISIBLE_DEVICES``, and ``GPU_DEVICE_ORDINAL``.

See Also
--------

- :doc:`ansible`
- :doc:`upgrading`
- :doc:`uninstalling`
- :doc:`/admin-guide/configuration`
