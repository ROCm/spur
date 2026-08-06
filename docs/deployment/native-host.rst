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
     - Config file (largely ignored; see the note below).
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

   ``spurd`` is configured entirely by CLI flags. It logs ``failed to load spur.conf``
   at startup — this is harmless; the agent never truly reads a config file.

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

Spur supports **single-node** Open MPI jobs via ``--mpi=pmix``. The controller and
CLI do not link libpmix; each compute node loads ``spur_mpi_pmix.so`` from
``[mpi].plugin_dir`` when a PMIx job starts.

Architecture
~~~~~~~~~~~~

1. **``spurd``** loads ``spur_mpi_pmix.so`` and calls ``PMIx_server_init`` when a
   job with ``mpi = pmix`` is launched.
2. The plugin registers a namespace (``spur.<job_id>``), job size, and local
   client ranks, then serves PMIx to application processes.
3. For ``-n > 1`` on one node, ``spurd`` wraps the user command in a bash script
   that runs **``mpirun -np N``** once (not ``N`` independent forks). Open MPI
   4.x otherwise creates a singleton ``MPI_COMM_WORLD`` (``size=1`` per rank).
4. The wrapper exports ``PMIX_SERVER_URI4`` from ``PMIX_SERVER_URI``, unsets
   ``SLURM_*`` twins (so Open MPI does not assume Slurm PMI), and resolves
   ``mpirun`` from ``PATH`` or ``OPAL_PREFIX``.

The embedded PMIx server must **not** override ``fence_nb`` / ``fence`` with a
no-op: modex exchange is handled internally by OpenPMIx GDS on single-node jobs.

Build and install the plugin
~~~~~~~~~~~~~~~~~~~~~~~~~~~~

On each **agent**, with libpmix development files (``pkg-config pmix`` or vendor
headers + ``libpmix.so``):

.. code-block:: bash

   cargo build --release -p spur-mpi-pmix
   sudo install -D target/release/spur_mpi_pmix.so /usr/lib/spur/spur_mpi_pmix.so

.. important::

   Build on the **same OS/glibc** as the agent, or compile ``pmix_server.c``
   directly on the node. Copying a plugin from a mismatched dev environment can
   crash ``spurd`` at ``dlopen`` time.

If ``pkg-config pmix`` is unavailable on the agent but system headers exist (e.g.
``/usr/lib/x86_64-linux-gnu/pmix2/include``), compile the C plugin manually on the
node:

.. code-block:: bash

   gcc -fPIC -Wall -O2 -shared -o spur_mpi_pmix.so pmix_server.c \
     -Iinclude \
     -I/usr/lib/x86_64-linux-gnu/pmix2/include \
     -L/usr/lib/x86_64-linux-gnu/pmix2/lib \
     -Wl,-rpath,/usr/lib/x86_64-linux-gnu/pmix2/lib \
     -lpmix -pthread
   sudo install -D spur_mpi_pmix.so /usr/lib/spur/spur_mpi_pmix.so

Runtime requirements
~~~~~~~~~~~~~~~~~~~~

- **OpenPMIx** on the agent (plugin links ``libpmix``).
- **Open MPI** with ``mpirun`` on the agent ``PATH`` (or set ``OPAL_PREFIX`` so
  the wrapper finds ``$OPAL_PREFIX/bin/mpirun``).
- Application binaries built against the **same** Open MPI install you use at
  runtime (consistent ``LD_LIBRARY_PATH`` / ``OPAL_PREFIX``).

``spur.conf`` on agents (match ``plugin_dir`` to the install path):

.. code-block:: toml

   [mpi]
   plugin_dir = "/usr/lib/spur"
   pmix_tmpdir = "/tmp/spur-pmix"
   pmix_min_version = "4.1.0"

Submit PMIx jobs
~~~~~~~~~~~~~~~~

.. code-block:: bash

   srun --mpi=pmix -n4 ./hello_mpi
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

- ``OMPI_MCA_ess=env`` when ``spurd`` already uses ``mpirun``.
- Forcing ``OMPI_MCA_pmix=ext3x`` on Open MPI 4.1 (use the default ``pmix3x``
  component, or omit the variable).
- Mixing library paths from different Open MPI installations.

Operational notes
~~~~~~~~~~~~~~~~~

- Set ``SPUR_MPI_DEBUG=1`` in ``spurd`` environment for plugin debug logs.
- Each agent holds at most 64 active PMIx namespaces; additional concurrent
  ``--mpi=pmix`` jobs on the same node fail until a job finishes.
- Multi-node PMIx coordination is not yet supported.
- Multi-rank ``--mpi=pmix`` steps launch via a single ``mpirun -np N`` wrapper.
  That path does not apply Spur's per-task CPU bind (``--cpu-bind``) or per-rank
  GPU partitioning (``SPUR_JOB_GPUS``) the way the non-MPI fork wrapper does.
  Use Open MPI binding options or set rank-local GPU env in the application script
  until Spur adds MPI-aware bind support.

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
