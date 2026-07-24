Quickstart
==========

The fastest way to try Spur. Everything runs on one machine, no config file needed.

Get the Binaries
----------------

.. code-block:: bash

   curl -fsSL https://raw.githubusercontent.com/ROCm/spur/main/install.sh | bash
   export PATH="$HOME/.local/bin:$PATH"

Or build from source — see :doc:`/developer/building`.

Start the Controller
--------------------

.. code-block:: bash

   mkdir -p /tmp/spur-state
   spurctld -D --state-dir /tmp/spur-state --log-level info

The controller listens on port 6817 and creates a single "default" partition.

Start the Agent
---------------

In a new terminal:

.. code-block:: bash

   spurd -D --controller http://localhost:6817

The agent auto-discovers local CPUs, memory, and GPUs, then registers with the controller:

.. code-block:: text

   INFO spurd: resources discovered cpus=16 memory_mb=32000 gpus=0
   INFO spurd::reporter: registered with controller

Submit a Job
------------

.. code-block:: bash

   cat > /tmp/hello.sh << 'EOF'
   #!/bin/bash
   #SBATCH --job-name=hello
   #SBATCH --time=00:01:00

   echo "Hello from Spur!"
   echo "Running on $(hostname) with $SPUR_CPUS_ON_NODE CPUs"
   sleep 2
   echo "Done."
   EOF

   spur submit /tmp/hello.sh

Output: ``Submitted batch job 1``

Check Status
------------

.. code-block:: bash

   spur queue              # View the job queue
   spur nodes              # View nodes
   spur show job 1         # Detailed job info
   cat /tmp/spur-1.out     # Job output (default location)

Run an Interactive Command
--------------------------

.. code-block:: bash

   spur run hostname
   spur run -- bash -c "echo I have \$SPUR_CPUS_ON_NODE CPUs"

Slurm Compatibility
-------------------

Use Slurm commands as ``spur`` subcommands:

.. code-block:: bash

   spur sbatch /tmp/hello.sh
   spur squeue
   spur sinfo -N
   spur scancel 2

Or create symlinks for full drop-in compatibility:

.. code-block:: bash

   cd $(dirname $(which spur))
   for cmd in sbatch srun squeue scancel sinfo sacct scontrol; do
       ln -sf spur $cmd
   done

Requesting GPUs
---------------

Three flags control GPU allocation, matching Slurm semantics:

``--gpus=N`` (or ``-G N``)
   Request *N* GPUs total across the job. Spur distributes them greedily
   across the assigned nodes (most-available first), guaranteeing at least
   one GPU per node. The request is rejected if *N* is less than the number
   of nodes.

``--gpus-per-node=K`` (or ``--gres=gpu:K``)
   Request exactly *K* GPUs on every node.

``--gpus-per-task=K``
   Request *K* GPUs per task. The per-node GPU count follows the task
   layout (nodes running more tasks get more GPUs).

Only one of the three forms may be specified per job. An optional GPU type
can be prefixed (e.g., ``--gpus=mi300x:4``).

.. code-block:: bash

   # 8 GPUs total, distributed across 2 nodes
   spur sbatch -N2 --gpus=8 train.sh

   # 4 GPUs on every node
   spur sbatch -N4 --gpus-per-node=4 train.sh

   # 2 GPUs per task (4 tasks across 2 nodes)
   spur sbatch -N2 -n4 --gpus-per-task=2 train.sh
