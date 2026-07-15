Native-Host Deployment
=====================

Deploy Spur across physical or virtual machines.

Install
-------

Get Spur binaries onto all nodes.

.. code-block:: bash

   curl -fsSL https://raw.githubusercontent.com/ROCm/spur/main/install.sh | bash
   export PATH="$HOME/.local/bin:$PATH"

To build from source instead, see :doc:`/developer/building`.

Setting Up the Controller
-------------------------

Initialize the network for encrypted node-to-node communication:

.. code-block:: bash

   sudo spur net init --cidr 10.44.0.0/16 --port 51820

This sets up a WireGuard mesh, prints the server public key, and outputs a join command template for workers.

Create ``/etc/spur/spur.conf``. The repository includes ``examples/spur.conf``. A minimal example:

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

Start the controller:

.. code-block:: bash

   sudo mkdir -p /var/spool/spur
   spurctld -D -f /etc/spur/spur.conf

.. tip::

   For production, run as a systemd service, e.g.:

   .. code-block:: ini

      # /etc/systemd/system/spurctld.service
      [Unit]
      Description=Spur Controller
      After=network.target

      [Service]
      ExecStart=/usr/local/bin/spurctld -f /etc/spur/spur.conf
      StateDirectory=spur
      Restart=on-failure

   Adjust ``ExecStart`` to match your install path. Then ``systemctl enable --now spurctld``.

High Availability
^^^^^^^^^^^^^^^^^

For HA, run ``spurctld`` on 3 (or 5) nodes with Raft consensus. Add all controller addresses to the ``peers`` list in the config:

.. code-block:: toml

   [controller]
   peers = [
     "10.44.0.1:6821",
     "10.44.0.4:6821",
     "10.44.0.5:6821",
   ]

Raft automatically elects a leader. Workers connect to any controller and are redirected to the current leader.

Joining Worker Nodes
--------------------

On each worker, join the WireGuard mesh:

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
       --listen [::]:6818

The agent auto-detects CPUs, memory, and GPUs, then registers with the controller over the mesh.

For an HA quorum, pass every controller as a comma-separated list so the agent
and CLI fail over to a surviving node if one is unreachable. The same format
works for the ``SPUR_CONTROLLER_ADDR`` environment variable:

.. code-block:: bash

   --controller http://10.44.0.1:6817,http://10.44.0.2:6817,http://10.44.0.3:6817

Repeat for each worker, incrementing the WireGuard address.

Verify:

.. code-block:: bash

   spur net status    # WireGuard peers and handshake times
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

Spur automatically restricts GPU visibility per job:

- **AMD (ROCm):** Sets ``ROCR_VISIBLE_DEVICES``
- **NVIDIA (CUDA):** Sets ``CUDA_VISIBLE_DEVICES``
