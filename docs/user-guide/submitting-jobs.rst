Submitting Batch Jobs
=====================

A batch job is a shell script that Spur runs on your behalf when the requested
resources become available. Submit it with ``spur submit`` (Slurm ``sbatch``)
and the controller queues it, allocates nodes, and runs the script on the first
allocated node. This page covers writing a job script, the options that control
the allocation, job arrays and dependencies, and the environment variables your
script sees at run time.

Script Body Sources
-------------------

Spur takes the job's script body from one of three sources:

- **A script file** — the common case. Pass the script as a positional
  argument. Trailing arguments after the script are passed to it as ``$1``,
  ``$2``, and so on.

  .. code-block:: bash

     spur submit train.sh arg1 arg2

- **--wrap** — wrap a single command in a minimal ``#!/bin/sh`` script. This is
  mutually exclusive with a script file; passing both is rejected.

  .. code-block:: bash

     spur submit --wrap "python quick.py"

- **stdin** — pipe a script in. This errors if stdin is a terminal and no script
  file is given.

  .. code-block:: bash

     spur submit < train.sh

``#SBATCH`` directives in the script header are parsed the same way Slurm parses
them: lines beginning with ``#SBATCH`` are read until the first line that is
neither a comment nor the shebang. ``#PBS`` directives are also converted on a
best-effort basis (``-N`` to ``--job-name``, ``-l walltime=`` to ``--time``,
``-l nodes=N:ppn=M`` to ``--nodes``/``--ntasks-per-node``, plus ``-o``, ``-e``,
``-A``, and ``-l mem=``).

Option Precedence
-----------------

When the same option is set in more than one place, Spur resolves it in this
order (highest wins), matching Slurm:

1. A command-line flag.
2. An ``SBATCH_*`` environment variable (e.g. ``SBATCH_PARTITION``).
3. A ``#SBATCH`` directive in the script header.
4. The built-in default.

So ``spur submit --nodes=4 train.sh`` overrides a ``#SBATCH --nodes=2`` directive
inside ``train.sh``.

A Sample Batch Script
---------------------

.. code-block:: bash

   #!/bin/bash
   #SBATCH --job-name=train-llm
   #SBATCH --partition=gpu
   #SBATCH --nodes=2
   #SBATCH --ntasks-per-node=8
   #SBATCH --cpus-per-task=6
   #SBATCH --gres=gpu:mi300x:8
   #SBATCH --mem=512G
   #SBATCH --time=12:00:00
   #SBATCH --account=research
   #SBATCH --qos=high
   #SBATCH --output=train-%j.out
   #SBATCH --error=train-%j.err

   echo "Job $SLURM_JOB_ID on $SLURM_JOB_NODELIST"
   echo "GPUs: $ROCR_VISIBLE_DEVICES"
   srun python train.py --epochs 100

Submit it. ``spur submit`` and ``sbatch`` are equivalent:

.. code-block:: bash

   sbatch train.sh              # Slurm-compatible name
   spur submit train.sh         # native verb equivalent
   sbatch --nodes=4 train.sh    # the CLI flag overrides #SBATCH --nodes=2

On success Spur prints the assigned job ID:

.. code-block:: text

   Submitted batch job 1

For scripting, ``--parsable`` prints only the job ID so you can capture it:

.. code-block:: bash

   jobid=$(sbatch --parsable train.sh)

Common Options
--------------

The options below are the ones you will use most often. In ``%j``/``%J``
patterns for ``--output`` and ``--error``, the token expands to the job ID.

.. list-table::
   :header-rows: 1
   :widths: 26 10 64

   * - Long
     - Short
     - Description
   * - ``--job-name``
     - ``-J``
     - Name for the job. Defaults to the script file name (or ``wrap``/``sbatch``).
   * - ``--partition``
     - ``-p``
     - Partition to run in. Defaults to the cluster's default partition.
   * - ``--account``
     - ``-A``
     - Account to charge the job to.
   * - ``--qos``
     - ``-q``
     - Quality-of-service level.
   * - ``--nodes``
     - ``-N``
     - Number of nodes to allocate. Default ``1``.
   * - ``--ntasks``
     - ``-n``
     - Total number of tasks. Default ``1``.
   * - ``--ntasks-per-node``
     -
     - Tasks to launch per node.
   * - ``--cpus-per-task``
     - ``-c``
     - CPUs per task. Default ``1``.
   * - ``--mem``
     -
     - Memory per node. ``512G`` and ``4096M`` use suffixes; a bare ``4096`` means MB.
   * - ``--mem-per-cpu``
     -
     - Memory per allocated CPU instead of per node.
   * - ``--gres``
     -
     - Generic resources, e.g. ``gpu:4`` or ``gpu:mi300x:8``. See
       :ref:`submit-gpus` below.
   * - ``--gpus``
     - ``-G``
     - Shorthand for GPUs; ``4`` or ``mi300x:4``, folded into ``gpu:<val>``.
   * - ``--gpus-per-node``
     -
     - GPUs per node, folded into ``gpu:<val>``.
   * - ``--time``
     - ``-t``
     - Wall-clock limit, e.g. ``12:00:00`` or ``1-00:00:00`` (day-hours). The
       job is killed when reached.
   * - ``--chdir``
     - ``-D``
     - Working directory for the job. Defaults to the submission directory.
   * - ``--output``
     - ``-o``
     - File for stdout. ``%j`` expands to the job ID.
   * - ``--error``
     - ``-e``
     - File for stderr. If unset, stderr follows ``--output``.
   * - ``--dependency``
     - ``-d``
     - Defer the job until other jobs reach a state. See
       :ref:`submit-dependencies`.
   * - ``--array``
     - ``-a``
     - Submit a job array, e.g. ``0-99%10``. See :ref:`submit-arrays`.
   * - ``--constraint``
     - ``-C``
     - Required node features, e.g. ``mi300x,nvlink``.
   * - ``--nodelist``
     - ``-w``
     - Request a specific list of nodes.
   * - ``--exclude``
     - ``-x``
     - Exclude specific nodes from the allocation.
   * - ``--exclusive``
     -
     - Do not share allocated nodes with other jobs. Default off.
   * - ``--hold``
     - ``-H``
     - Submit the job held; it will not start until released. Default off.
   * - ``--requeue``
     -
     - Allow the job to be requeued after a node failure. Default off.
   * - ``--begin``
     -
     - Defer start until a time, e.g. ``now+2hours`` or an ISO 8601 timestamp.
   * - ``--deadline``
     -
     - Cancel the job if it is still pending past this time.
   * - ``--mail-type``
     -
     - Email events: ``BEGIN``, ``END``, ``FAIL``, ``ALL`` (comma-separated).
   * - ``--mail-user``
     -
     - Address for job email.
   * - ``--export``
     -
     - Which submission environment to forward. See :ref:`submit-export`.
   * - ``--licenses``
     - ``-L``
     - Request licenses. Repeatable; values accumulate.
   * - ``--wrap``
     -
     - Wrap a command instead of using a script file.
   * - ``--parsable``
     -
     - Print only the job ID on success.

.. _submit-export:

Forwarding the Submission Environment
-------------------------------------

``--export`` (env ``SBATCH_EXPORT``) controls which of your environment
variables the job inherits. Default is ``ALL``.

- ``ALL`` — forward the submitter's full environment.
- ``NONE`` — forward nothing from the submitter.
- ``VAR1,VAR2`` — forward only the named variables.

.. code-block:: bash

   sbatch --export=NONE train.sh
   sbatch --export=DATA_DIR,MODEL_DIR train.sh

.. _submit-gpus:

Requesting GPUs
---------------

There are several equivalent ways to request GPUs; all resolve to a ``gres``
entry on the job:

.. code-block:: bash

   sbatch --gres=gpu:8 train.sh              # 8 GPUs of any type
   sbatch --gres=gpu:mi300x:8 train.sh       # 8 GPUs of a specific type
   sbatch --gpus=4 train.sh                  # -G shorthand
   sbatch --gpus-per-node=8 train.sh

``--gres`` accepts a comma list (``gpu:2,fpga:1``) which is cumulative; a
repeated ``--gres`` flag replaces the previous value (last wins). See
:doc:`interactive` for what a GPU job sees at run time.

.. _submit-arrays:

Job Arrays
----------

A job array submits many near-identical tasks from one script. Use ``--array``
(``-a``); the ``%`` suffix caps how many run concurrently.

.. code-block:: bash

   sbatch --array=0-99%10 train.sh

This submits 100 tasks (indices ``0``–``99``) with at most 10 running at a time.
Tasks held back by the ``%10`` cap show ``Reason=JobArrayTaskLimit`` in
``squeue``/``scontrol show job`` until a running task completes and frees a
slot.

Each task sees its array identity through these variables (each with a
``SLURM_*`` twin):

.. list-table::
   :header-rows: 1
   :widths: 34 66

   * - Variable
     - Value
   * - ``SPUR_ARRAY_JOB_ID``
     - The array's base job ID, shared by every task.
   * - ``SPUR_ARRAY_TASK_ID``
     - This task's array index.

.. _submit-dependencies:

Dependencies
------------

``--dependency`` (``-d``) defers a job until other jobs reach a given state. The
value is a comma-separated list.

.. code-block:: bash

   sbatch --dependency=afterok:123,afterany:456 next.sh

``afterok:123`` waits for job 123 to complete successfully; ``afterany:456``
waits for job 456 to finish in any state.

Environment Variables Inside a Job
----------------------------------

The node agent injects a set of variables when your job launches. Every
``SPUR_*`` variable gets an automatic ``SLURM_*`` twin, so Slurm-aware software
works unchanged. The most useful are below.

.. list-table::
   :header-rows: 1
   :widths: 34 66

   * - Variable
     - Meaning
   * - ``SPUR_JOB_ID``
     - The job's ID (twin ``SLURM_JOB_ID``).
   * - ``SPUR_JOB_NAME``
     - The job name.
   * - ``SPUR_JOB_PARTITION``
     - The partition the job runs in.
   * - ``SPUR_NNODES``
     - Number of nodes in the allocation.
   * - ``SPUR_NTASKS``
     - Total number of tasks.
   * - ``SPUR_CPUS_PER_TASK``
     - CPUs allocated per task.
   * - ``SPUR_NODELIST``
     - The allocated node list.
   * - ``SPUR_CPUS_ON_NODE``
     - CPUs available on the current node.
   * - ``SPUR_PROCID``
     - The task's global rank (twin ``SLURM_PROCID``).
   * - ``SPUR_LOCALID``
     - The task's rank within its node (twin ``SLURM_LOCALID``).
   * - ``SPUR_SUBMIT_DIR``
     - The directory the job was submitted from.
   * - ``SPUR_ARRAY_JOB_ID``
     - Array base job ID (array jobs only).
   * - ``SPUR_ARRAY_TASK_ID``
     - Array task index (array jobs only).

For MPI and distributed-training frameworks, the agent also sets the variables
those runtimes expect — ``LOCAL_RANK``, ``LOCAL_WORLD_SIZE``, ``NODE_RANK``, the
``PMI_*``/``PMIX_*`` ranks (the latter with ``--mpi=pmix``), the
``OMPI_COMM_WORLD_*`` ranks, and ``SPUR_PEER_NODES`` — so ``torchrun``, MPI, and
PMI/PMIx launchers run without extra wiring.

See Also
--------

- :doc:`monitoring-jobs`
- :doc:`interactive`
- :doc:`running-containers`
- :doc:`/admin-guide/accounting`
