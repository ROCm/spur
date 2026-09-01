Interactive & Parallel Jobs
===========================

Not every job is a batch script. Use ``spur run`` (Slurm ``srun``) to run a
command directly as a job or job step, ``spur alloc`` (Slurm ``salloc``) to get
an interactive shell on allocated resources, and ``spur attach`` (Slurm
``sattach``) to connect to a running job's input and output. This page covers
all three and how to request GPUs for them.

Run a Command — ``srun``
------------------------

``spur run`` (Slurm ``srun``) runs a command as a job and streams its output
live. It operates in two modes:

- **Standalone** — when run outside an allocation, it submits a new job, waits
  for the allocation, streams output from the node agent, and exits with the
  job's exit code. Pressing Ctrl-C cancels the job. After the allocation is
  granted, standalone ``srun`` (including ``--pty``) resolves the job owner from
  the controller for step, keepalive, and cancel RPCs, the same way
  ``srun`` does
  inside an ``salloc`` shell via ``SPUR_JOB_USER``.
- **Step mode** — when run inside an existing allocation (``SPUR_JOB_ID`` is set,
  as under ``salloc`` or in a batch script), it creates a *job step* against the
  parent allocation instead of submitting a new job.

Inside an allocation, a bare ``srun`` inherits the allocation's size
(``--ntasks``, ``--cpus-per-task``, nodes, partition, account, QOS), so it runs
at the allocation's scale unless you override on the command line.

Common options:

.. list-table::
   :header-rows: 1
   :widths: 26 10 64

   * - Long
     - Short
     - Description
   * - ``--nodes``
     - ``-N``
     - Number of nodes. Default ``1``.
   * - ``--ntasks``
     - ``-n``
     - Number of tasks. Default ``1``.
   * - ``--cpus-per-task``
     - ``-c``
     - CPUs per task. Default ``1``.
   * - ``--gres``
     -
     - Generic resources, e.g. ``gpu:8``.
   * - ``--gpus``
     - ``-G``
     - GPU shorthand, folded into ``gpu:<val>``.
   * - ``--gpu-bind``
     -
     - Per-task GPU binding: ``closest``, ``map_gpu:...``, ``mask_gpu:...``, or
       ``none``.
   * - ``--partition``
     - ``-p``
     - Partition to run in.
   * - ``--time``
     - ``-t``
     - Wall-clock limit.
   * - ``--pty``
     -
     - Allocate a pseudo-terminal (use with an interactive shell).
   * - ``--label``
     - ``-l``
     - Prefix each output line with its task rank.
   * - ``--output``
     - ``-o``
     - File for stdout. ``%j`` expands to the job ID; if ``-o`` is set but
       ``-e`` is not, stderr follows stdout.
   * - ``--error``
     - ``-e``
     - File for stderr.
   * - ``--jobid``
     -
     - Target a running job (use with ``--overlap``).
   * - ``--overlap``
     -
     - Share resources with the targeted job's existing steps.

Examples:

.. code-block:: bash

   srun -N2 -n16 --gres=gpu:8 hostname     # parallel command across 2 nodes
   spur run --pty bash                      # interactive shell on an allocated node
   srun -n4 python train.py
   srun --jobid 1024 --overlap rocm-smi     # run a command inside a running job

.. note::

   ``--jobid`` requires ``--overlap``; ``--jobid`` alone is an error.
   ``--input``/``-i`` is ignored in step mode.

Interactive Allocation — ``salloc``
------------------------------------

``spur alloc`` (Slurm ``salloc``) requests an interactive allocation, waits for
it to start (up to 300 seconds), then spawns your ``$SHELL`` with the allocation
environment exported (``SPUR_JOB_ID``, ``SPUR_JOB_USER``, ``SPUR_NODELIST``,
``SPUR_NNODES``, ``SPUR_NTASKS``, ``SPUR_CPUS_PER_TASK``, the
partition/account/QOS variables, and their ``SLURM_*`` twins). When you exit the
shell, the allocation is released. Ctrl-C cancels it.

When authentication is enabled, ``salloc`` also passes ``$SPUR_AUTH_TOKEN`` (or
``~/.spur/token``) into the allocation shell so step commands can authenticate
to the controller. ``SPUR_JOB_USER`` records the job owner bound at submit time
(for example the JWT subject); ``srun`` inside the shell uses it when step RPCs
run without a token.

Inside that shell, ``srun`` runs as a job step sized to the allocation.

Common options: ``--nodes``/``-N`` (default ``1``), ``--ntasks``/``-n`` (default
``1``), ``--cpus-per-task``/``-c`` (default ``1``), ``--mem``, ``--time``/``-t``
(default ``1:00:00``), ``--gres``, ``--gpus``/``-G``, ``--partition``/``-p``,
``--constraint``/``-C``, ``--nodelist``/``-w``, ``--exclude``/``-x``,
``--reservation``, and ``--exclusive``.

Examples:

.. code-block:: bash

   salloc -N1 --gres=gpu:2 -t 2:00:00
   spur alloc --partition gpu --exclusive

Attach to a Running Job — ``sattach``
-------------------------------------

``spur attach`` (Slurm ``sattach``) connects to a running job's I/O. The
positional argument is ``JOB_ID`` or ``JOB_ID.STEP_ID``; the step-id component is
accepted, but only the job id is used to route the attach. By default it opens a
full interactive raw-mode terminal attached through the node agent. Attach RPCs
use the job owner from ``SPUR_JOB_USER`` (when set), the controller job record, or
the local login name, matching step-mode ``srun`` under JWT authentication.

.. list-table::
   :header-rows: 1
   :widths: 30 70

   * - Option
     - Description
   * - ``--output <stdout|stderr>``
     - Which stream to attach to. Default ``stdout``.
   * - ``--output-only``
     - Stream output one-way instead of an interactive terminal.

.. code-block:: bash

   sattach 1024
   sattach 1024 --output-only --output stderr

Requesting GPUs
---------------

``srun``, ``salloc``, and batch jobs all request GPUs the same way:

.. code-block:: bash

   srun --gres=gpu:2 python infer.py         # 2 GPUs of any type
   srun --gres=gpu:mi300x:8 python train.py  # 8 GPUs of a specific type
   srun -G 4 python infer.py                 # -G shorthand
   salloc --gres=gpu:2 -t 1:00:00

When GPUs are allocated, the job sees them through several variables, each set to
the allocated device ordinals so GPU runtimes find the right devices:

.. list-table::
   :header-rows: 1
   :widths: 34 66

   * - Variable
     - Meaning
   * - ``ROCR_VISIBLE_DEVICES``
     - Allocated GPU ordinals.
   * - ``CUDA_VISIBLE_DEVICES``
     - Allocated GPU ordinals.
   * - ``GPU_DEVICE_ORDINAL``
     - Allocated GPU ordinals.
   * - ``SPUR_JOB_GPUS``
     - Allocated GPU ordinals (Spur-native).

Under ``srun``, ``--gpu-bind`` adjusts the visible set per task —
``--gpu-bind=closest`` binds each task to its nearest GPU, and
``map_gpu:...``/``mask_gpu:...`` set an explicit per-task mapping or mask.

See Also
--------

- :doc:`submitting-jobs`
- :doc:`monitoring-jobs`
