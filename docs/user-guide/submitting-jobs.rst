Submitting Batch Jobs
=====================

A batch job is a shell script that Spur runs on your behalf when the requested
resources become available. Submit it with ``spur submit`` (Slurm ``sbatch``)
and the controller queues it, allocates nodes, and runs the script on them (see
:ref:`submit-script-placement` for which ones). This page covers writing a job
script, the options that control the allocation, job arrays and dependencies, and
the environment variables your script sees at run time.

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

An empty script file or empty piped input is rejected before a job ID is
assigned.

``#SBATCH`` directives in the script header are parsed the same way Slurm parses
them: lines beginning with ``#SBATCH`` are read until the first line that is
neither a comment nor the shebang. ``#PBS`` directives are also converted on a
best-effort basis (``-N`` to ``--job-name``, ``-l walltime=`` to ``--time``,
``-l nodes=N:ppn=M`` to ``--nodes``/``--ntasks-per-node``, plus ``-o``, ``-e``,
``-A``, and ``-l mem=``).

.. _submit-option-precedence:

Option Precedence
-----------------

When the same option is set in more than one place, Spur resolves it in this
order (highest wins), matching Slurm:

1. A command-line flag.
2. An ``SBATCH_*`` environment variable (e.g. ``SBATCH_PARTITION``).
3. A ``#SBATCH`` directive in the script header.
4. The built-in default, or for ``--script-placement`` the cluster's default.

So ``spur submit --nodes=4 train.sh`` overrides a ``#SBATCH --nodes=2`` directive
inside ``train.sh``.

Two option groups conflict by *source* rather than per flag, matching Slurm:
``-c``/``--cpus-per-task`` vs. ``--cpus-per-gpu``, and ``--mem`` vs.
``--mem-per-cpu`` vs. ``--mem-per-gpu``. A command-line flag or a ``#SBATCH``
directive counts as **explicit**; an ``SPUR_*``/``SBATCH_*`` environment
variable counts as **env**. ``--cpus-per-gpu`` and ``--mem-per-gpu`` are the
exception: for ``sbatch`` they read only
``SBATCH_CPUS_PER_GPU``/``SBATCH_MEM_PER_GPU`` (``salloc`` reads its own
``SALLOC_CPUS_PER_GPU``/``SALLOC_MEM_PER_GPU``), not an ``SPUR_*`` twin —
spurd exports that twin into every running job, and reading it back would
make a nested ``sbatch``/``salloc`` inherit the enclosing job's per-GPU
sizing. Within a group, two explicit values (or two env values) are
rejected; one explicit and one env value is not an error — the env value is
silently dropped and the explicit one is used:

.. code-block:: console

   $ sbatch --cpus-per-gpu=8 -c 2 --gres=gpu:1 train.sh
   error: --cpus-per-gpu cannot be combined with --cpus-per-task (-c)

   $ sbatch --mem=4G --mem-per-cpu=1G train.sh
   error: --mem, --mem-per-cpu, and --mem-per-gpu are mutually exclusive

``--cpus-per-gpu`` and ``--mem-per-gpu`` also require the job to request GPUs
(``--gpus``, ``--gpus-per-node``, ``--gpus-per-task``, or ``--gres=gpu:...``);
without one they are rejected before the job reaches the controller. If
neither ``-c`` nor ``--cpus-per-gpu`` survives this rule, a GPU job may still
get a default CPUs-per-GPU count from the partition's (or ``[scheduler]``'s)
``def_cpu_per_gpu``. ``--cpus-per-gpu=0`` (from the CLI, ``SBATCH_CPUS_PER_GPU``,
or the equivalent ``srun``/``salloc`` env variable) is rejected, matching Slurm.

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

.. _submit-script-placement:

Where Your Batch Script Runs
----------------------------

A multi-node job can run its script once, on the first allocated node (the
*batch host*), or once on every allocated node. ``--script-placement`` chooses:

.. list-table::
   :header-rows: 1
   :widths: 18 82

   * - Value
     - Where the script runs
   * - ``auto``
     - On the batch host when the script itself calls ``srun``, ``mpirun``, or
       ``mpiexec``; on every node otherwise. This is the default unless your
       administrator changed it.
   * - ``batch-host``
     - On the batch host only, as in Slurm. The other nodes stay allocated until
       the script exits, so ``srun``, or an ``mpirun`` that places its daemons
       with ``srun``, can reach any of them at any time.
   * - ``all-nodes``
     - On every allocated node, each with its own ``SLURM_NODEID``.

``auto`` reads only the script's own text. It misses a launcher one level
down, such as one in a script the batch script runs, behind a wrapper, or in a
variable like ``$LAUNCHER``, and then runs the whole script on every node. Set
``batch-host`` for such scripts:

.. code-block:: bash

   #!/bin/bash
   #SBATCH --nodes=8
   #SBATCH --script-placement=batch-host

   # Each test calls `mpirun --mca plm slurm ...`.
   for test in tests/*.sh; do
       bash "$test"
   done

The value resolves like any other option: the command line, then
``SPUR_SCRIPT_PLACEMENT`` or ``SBATCH_SCRIPT_PLACEMENT``, then a ``#SBATCH``
line, then the cluster default, ``[controller] script_placement`` (see
:doc:`/admin-guide/configuration`). Real Slurm rejects an unknown ``#SBATCH``
option, so a script that must also run on Slurm should get the value from the
environment variable instead.

To run a command on every node from a ``batch-host`` script, launch it as a
step, as in Slurm:

.. code-block:: bash

   srun --ntasks-per-node=1 ./prepare-node.sh

``scontrol show job`` reports the outcome. ``ScriptPlacement=`` shows the
requested value while the job is pending and the one it runs with once it is
placed, and ``BatchHost=`` names the node the script runs on.

Nodes managed by the Kubernetes operator run one Pod per node and cannot run
``srun`` steps, so a job placed on them always runs the script on every node.
A multi-node ``--script-placement=batch-host`` job is held with a reason
there.

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
     - CPUs per task. When not given on a GPU job, the partition's (or
       ``[scheduler]``'s) ``def_cpu_per_gpu`` sets CPUs per GPU instead — it
       does not set ``-c`` itself. See :ref:`submit-option-precedence`.
   * - ``--mem``
     -
     - Memory per node. ``512G`` and ``4096M`` use suffixes; a bare ``4096`` means MB.
   * - ``--mem-per-cpu``
     -
     - Memory per allocated CPU instead of per node.
   * - ``--cpus-per-gpu``
     -
     - CPUs per allocated GPU instead of per task. Needs a GPU request;
       conflicts with ``-c`` by source. See :ref:`submit-option-precedence`.
   * - ``--mem-per-gpu``
     -
     - Memory per allocated GPU instead of per node or per CPU. Needs a GPU
       request; conflicts with ``--mem``/``--mem-per-cpu`` by source. See
       :ref:`submit-option-precedence`.
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
     - Required node features, e.g. ``mi300x,nvlink``, and optional GPU
       predicates such as ``same(gpu.fabric_domain_id)``. See
       :ref:`submit-topology`.
   * - ``--nodelist``
     - ``-w``
     - Request a specific list of nodes.
   * - ``--exclude``
     - ``-x``
     - Exclude specific nodes from the allocation.
   * - ``--exclusive``
     -
     - Do not share allocated nodes with other jobs. Default off.
   * - ``--topology``
     -
     - Prefer locality with ``block`` or ``tree``; ``none`` skips the hint.
       An explicit same-fabric constraint still applies.
   * - ``--hold``
     - ``-H``
     - Submit the job held; it will not start until released. Default off.
   * - ``--requeue``
     -
     - Allow the job to be requeued after a node failure or time limit.
       Default off. If the failed node comes back, the job's earlier run there
       is stopped (SIGTERM, then SIGKILL if it does not exit).
   * - ``--no-requeue``
     -
     - Do not requeue the job after a node failure or time limit. This is the
       default. Unlike Slurm, preemption and ``scontrol requeue`` still requeue
       the job, as do reclaim and launch failures. Either flag overrides the
       other, including a ``#SBATCH`` line, and the later flag wins when both
       appear on the command line.
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
   * - ``--script-placement``
     -
     - Run the script on the batch host only (``batch-host``), on every node
       (``all-nodes``), or let Spur decide (``auto``). See
       :ref:`submit-script-placement`.
   * - ``--wrap``
     -
     - Wrap a command instead of using a script file.
   * - ``--parsable``
     -
     - Print only the job ID on success.
   * - ``--wait``
     - ``-W``
     - Block until the job terminates; exit code reflects the job outcome.
       Works with ``--parsable`` (ID printed at submission, then blocks).
       Also accepted as an ``#SBATCH --wait`` directive in scripts.

.. _submit-topology:

Topology Locality and Block Containment
---------------------------------------

When the administrator enables block topology, automatically discovered vPods
or administrator-declared node groups supply the existing topology model.
``--topology block`` and ``--topology tree`` use the configured model to prefer
nearby nodes. These options permit allocations across blocks when needed.

To require all allocated GPUs inside one block, add a same-fabric constraint.
Both jobs below use the same partition and discovered topology:

.. code-block:: bash

   spur submit -p gpu -N 2 -n 2 --gpus 6 --topology block train.sh
   spur submit -p gpu -N 2 -n 2 --gpus 6 --constraint='same(gpu.fabric_domain_id)' train.sh

The first job requests a locality preference and may span blocks. The second
requires containment and stays pending if no single block can fit it.
``same(gpu.vpod_id)`` is an alias. Combine either predicate with required node
features using commas or ``&``. Per-device model, VRAM, NUMA, PCI-root, and
domain filters are also supported; OR combinations are not supported. The constraint also works with ``spur alloc`` and standalone
``spur run``; steps inherit their allocation's binding.

``--topology none`` skips the hint without disabling an explicit constraint.
Omitting ``-p`` uses the default partition and does not alter the constraint.
A malformed GPU expression, such as an unknown field (``gpu.unknown=1``), an
uppercase ``GPU.`` prefix on a comparison, or a property name without a
comparison (``gpu.vram``), is rejected at submit with
``unsupported GPU constraint``. A term such as ``gpu.fast``, which is not a
property name and has no operator, remains an ordinary node feature. Partition names have no built-in topology meaning. See
:doc:`/admin-guide/fabric-observation` for configuration and inspection.

.. _submit-export:

Forwarding the Submission Environment
-------------------------------------

``--export`` (env ``SBATCH_EXPORT``) controls which of your environment
variables the job inherits. Default is ``ALL``.

- ``ALL`` — forward the submitter's full environment.
- ``NONE`` — forward nothing from the submitter.
- ``VAR1,VAR2`` — forward only the named variables.
- ``VAR=value`` — set an explicit value instead of copying it from the
  submitter. Overrides an inherited value.
- ``ALL,VAR=value`` — combine the two: forward the full environment and set (or
  override) the named variables on top.

The ``NONE`` and list forms still forward the caller's ``SLURM_*`` and
``SPUR_*`` variables, matching Slurm, so a job keeps its scheduler context.
The exception is credentials (``SPUR_AUTH_TOKEN``, ``SPUR_REGISTRY_PASSWORD``),
which these forms drop unless you name them explicitly
(``--export=NONE,SPUR_AUTH_TOKEN``). ``ALL`` forwards the whole environment
unfiltered, credentials included.

.. code-block:: bash

   sbatch --export=NONE train.sh
   sbatch --export=DATA_DIR,MODEL_DIR train.sh
   sbatch --export=ALL,MASTER_PORT=29999,WORLD_SIZE=16 train.sh

``ALL`` and ``NONE`` are case-insensitive.

A non-default ``--export`` is recorded in ``SLURM_EXPORT_ENV``, so ``srun``
steps inside the script inherit the same mode. Under ``NONE`` those steps get
no ``PATH``, so commands need an absolute path. To give steps the full batch
environment back, use ``srun --export=ALL`` for one step, or add
``export SLURM_EXPORT_ENV=ALL`` near the top of the script for all of them.
``srun`` accepts the same ``--export`` grammar directly.

As in Slurm, ``srun --export`` does not rewrite ``SLURM_EXPORT_ENV`` in the
step, so an ``srun`` launched from inside such a step still inherits the
original mode, not the override. Set ``SRUN_EXPORT_ENV`` (the ``srun``-only
override) or ``unset SLURM_EXPORT_ENV`` to change it.

.. note::

   As in Slurm, running ``sbatch`` from inside an existing job forwards that
   job's ``SLURM_*``/``SPUR_*`` variables to the new job. The node overwrites
   job-level variables such as the job ID, node list, and task counts, but
   leftovers it does not set survive: for example, ``SLURM_ARRAY_TASK_ID`` from
   a parent array task, or ``SLURM_EXPORT_ENV`` from a parent submitted with
   ``--export=NONE``. Unset them before submitting if the child job should not
   see them.

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
Tasks held back by the ``%10`` cap show ``JobArrayTaskLimit`` as their pending
reason — ``(JobArrayTaskLimit)`` in ``squeue``'s ``NODELIST(REASON)`` column,
``Reason=JobArrayTaskLimit`` in ``scontrol show job`` — until a running task
completes and frees a slot.

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
     - CPUs allocated on the current node.
   * - ``SPUR_LOCAL_GPUS``
     - Set only inside a step that holds its own GPU share: the step's GPUs
       renumbered from ``0`` to match what the GPU runtime sees there.
       ``SPUR_JOB_GPUS`` keeps the node's own ids in every case.
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

For MPI, the agent sets the ``PMI_*``/``PMIX_*`` ranks (the latter with
``--mpi=pmix``) and ``SPUR_PEER_NODES``, so PMI/PMIx launchers run without extra
wiring.

Open MPI (``--mpi=pmix``)
~~~~~~~~~~~~~~~~~~~~~~~~~

Spur launches ranks itself (no ``mpirun``). Administrators must install the
PMIx plugin and ``$HOME/spur/mpi/env.sh`` on every agent first — see
:doc:`/deployment/native-host`.

Build the binary **on each compute node** with that node's ``mpicc`` (the
controller may have no MPI compiler). The path in the script must exist on
every allocated node; home directories are often not shared.

.. code-block:: bash

   #!/bin/bash
   #SBATCH -J hello-mpi
   #SBATCH -N 2
   #SBATCH --ntasks-per-node=4
   #SBATCH -t 01:00:00
   #SBATCH --mpi=pmix

   cd "$SLURM_SUBMIT_DIR"
   srun --mpi=pmix ./hello_mpi

Use ``--mpi=pmix`` on both the batch header and the inner ``srun``. After
``sbatch``, ``spur-<jobid>.out`` may appear on an allocated node rather than
the submit host. A successful 8-rank job prints ``rank=0`` … ``rank=7`` with
``size=8``. Empty output plus ranks stuck at ~100 % CPU means ``MPI_Init`` did
not finish (often hash GDS, memlock, or Open MPI TCP using the wrong NIC) —
ask an admin to check the native-host MPI section rather than retrying the
same script.

Open MPI via mpirun
~~~~~~~~~~~~~~~~~~~

Two launch paths avoid SSH between nodes:

**Path 1: srun --mpi=mpirun** — Spur launches ``mpirun`` on node 0 only;
PRRTE's ``plm:slurm`` places its daemons on remote nodes via ``srun``,
no SSH required.  Other nodes hold the allocation until the driver
completes.

.. code-block:: bash

   srun --mpi=mpirun -N 2 -n 8 --gres=gpu:8 ./hello_mpi

When ``--mpi=mpirun`` is set, Spur additionally sources
``$HOME/spur/mpi/env.sh`` for MPI library paths.

**Path 2: sbatch + inline mpirun** — the batch script calls ``mpirun``
directly without ``--mpi=mpirun``.  PRRTE detects the Slurm allocation
from the environment (``SLURM_JOBID``, ``SLURM_NODELIST``) and uses
``plm:slurm`` to place daemons.

.. code-block:: bash

   #!/bin/bash
   #SBATCH -N 2
   #SBATCH -n 8
   #SBATCH --gres=gpu:8

   mpirun -np 8 --bind-to none ./hello_mpi

Both paths set ``SLURM_TASKS_PER_NODE`` in the ``N(xM)`` format that
Open MPI expects.  The ``--mpi=mpirun`` value appears in
``srun --mpi=list``.

.. note::

   For buffered ``srun`` recovery, including ``mpirun`` daemon placement, see
   :ref:`srun-controller-restart`.

Distributed-training rendezvous variables
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

Like Slurm, Spur does **not** set the PyTorch/torchrun rendezvous variables
(``MASTER_ADDR``, ``MASTER_PORT``, ``WORLD_SIZE``, ``RANK``, ``LOCAL_RANK``,
``LOCAL_WORLD_SIZE``, ``NODE_RANK``, ``NPROC_PER_NODE``). Any value you forward
with ``--export`` is preserved. Derive them from the ``SPUR_*``/``SLURM_*``
variables in your batch script, or let ``torchrun`` compute them:

.. list-table::
   :header-rows: 1
   :widths: 40 60

   * - Torch / ``env://`` variable
     - Spur source
   * - ``WORLD_SIZE``
     - ``SPUR_NTASKS`` / ``SLURM_NTASKS``
   * - ``RANK``
     - ``SPUR_PROCID`` / ``SLURM_PROCID``
   * - ``LOCAL_RANK``
     - ``SPUR_LOCALID`` / ``SLURM_LOCALID``
   * - ``NODE_RANK``
     - ``SPUR_NODEID`` / ``SPUR_NODE_RANK``
   * - ``NPROC_PER_NODE`` / ``LOCAL_WORLD_SIZE``
     - ``SPUR_TASKS_PER_NODE``
   * - ``MASTER_ADDR`` / ``MASTER_PORT``
     - First host of ``SPUR_JOB_NODELIST`` / ``SPUR_PEER_NODES``, plus a port you choose

.. code-block:: bash

   # Respect anything forwarded with --export; otherwise derive from the
   # allocation. The port is derived per job to avoid collisions between
   # concurrent jobs on shared nodes.
   export MASTER_ADDR="${MASTER_ADDR:-$(scontrol show hostnames "$SPUR_JOB_NODELIST" | head -n1)}"
   export MASTER_PORT="${MASTER_PORT:-$((20000 + SPUR_JOB_ID % 20000))}"
   export WORLD_SIZE="${WORLD_SIZE:-$SPUR_NTASKS}"
   export RANK="${RANK:-$SPUR_PROCID}"

See Also
--------

- :doc:`monitoring-jobs`
- :doc:`interactive`
- :doc:`running-containers`
- :doc:`/admin-guide/accounting`
