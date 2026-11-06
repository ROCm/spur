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

A step is named after the command line that created it. That name is stored
capped at 256 bytes, so a longer command line is truncated on a character
boundary and ends in ``...`` where ``scontrol show step`` prints it. The step
still runs the full command — only the displayed name is shortened.

Common options:

.. list-table::
   :header-rows: 1
   :widths: 26 10 64

   * - Long
     - Short
     - Description
   * - ``--nodes``
     - ``-N``
     - Number of nodes. Standalone default ``1``; a nested step inherits the
       allocation unless ``-N`` is typed.
   * - ``--ntasks``
     - ``-n``
     - Number of tasks. Standalone default ``1``; a nested step inherits
       ``SPUR_NTASKS`` / ``SLURM_NTASKS`` unless ``-n`` is typed.
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
   * - ``--export``
     -
     - Which submission environment variables reach the step, using Slurm's
       grammar (``ALL``, ``NONE``, ``VAR1,VAR2``, ``VAR=value``,
       ``ALL,VAR=value``; ``ALL``/``NONE`` are case-insensitive). Under
       ``NONE``/list forms, ``SLURM_*``/``SPUR_*`` are kept, except credentials
       (``SPUR_AUTH_TOKEN``, ``SPUR_REGISTRY_PASSWORD``) unless named. Default
       ``ALL``; env default ``SPUR_EXPORT_ENV``, then ``SRUN_EXPORT_ENV``, then
       ``SLURM_EXPORT_ENV`` (set by ``sbatch --export``). Under ``NONE`` a step
       has no ``PATH``, so give the command's absolute path. Ignored for a
       ``--pty`` step inside an existing allocation, which runs in the job's
       environment.
   * - ``--partition``
     - ``-p``
     - Partition to run in.
   * - ``--time``
     - ``-t``
     - Wall-clock limit.
   * - ``--pty``
     -
     - Allocate a pseudo-terminal (use with an interactive shell). Inside an
       existing allocation this runs as an interactive step: a single task on
       one node, started in the *job's* environment and working directory
       rather than your shell's. ``srun`` warns about the flags that implies it
       must drop — ``--output``/``--error``/``--input``, the task-sizing flags,
       and ``--cpu-bind``/``--gpu-bind``/``--label``/``--mpi``. ``--chdir`` moves
       the srun prolog/epilog but not the terminal. ``--container-image`` *is*
       honored — the interactive shell runs inside the container (see
       :doc:`running-containers`).
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

When JWT authentication is enabled, ``salloc`` also passes ``$SPUR_AUTH_TOKEN``
(or ``~/.spur/token``) into the allocation shell so step commands can
authenticate to the controller. With ``[auth] plugin = "spur"``, the shell does
not inherit a bearer; each command mints a fresh credential from the local
socket. ``SPUR_JOB_USER`` records the job owner bound at submit time (for
example the JWT subject); ``srun`` inside the shell uses it when step RPCs run
without a token.

Inside that shell, ``srun`` runs as a job step. With no step-level ``-N`` or
``-w``, it uses the allocation's nodes and inherits the allocation's task count
(``SPUR_NTASKS`` / ``SLURM_NTASKS``). For **buffered** steps, ``-N``/``--nodes``
limits the step to that many allocated nodes without dropping that inherited
task count, and ``-w``/``--nodelist`` selects an exact subset of allocated
nodes. A step cannot request more nodes than its allocation.

``--pty`` does not follow those node-selection rules: it still ignores
``--nodes`` (one task on one node) and, with ``-w``, uses only the first listed
name.

The ``-N``/``-n`` defaults of 1 apply when ``srun`` submits a **new** job, not
when it runs as a step inside ``salloc``/``sbatch``. Common options: ``--nodes``/
``-N``, ``--ntasks``/``-n``, ``--cpus-per-task``/``-c``, ``--mem``, ``--time``/
``-t``, ``--gres``, ``--gpus``/``-G``, ``--partition``/``-p``, ``--constraint``/
``-C``, ``--nodelist``/``-w``, ``--exclude``/``-x``, ``--reservation``, and
``--exclusive``.

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

Standalone ``srun --pty`` Lifecycle
-----------------------------------

A standalone ``srun --pty`` shell is the job's workload. When that shell exits,
the node reports it and the controller finishes the job with the shell's exit
code: ``COMPLETED`` on ``0`` and ``FAILED`` otherwise, ``TIMEOUT`` if the
shell was killed at its time limit or by the ``InactiveLimit`` reaper, and
``OUT_OF_MEMORY`` if the kernel OOM killer took it. A signal death reports
``128 + signal``, so a hangup is ``129``.
This does not depend on the ``srun`` client: if the client is lost (closed
terminal, ``kill -9``), the shell's exit still ends the job within seconds
rather than leaving the allocation in ``RUNNING``. A live client reports the
exit too; whichever report arrives first finishes the job and the other is a
no-op. ``srun`` marks its shell's step for the controller when it opens the
session, so a job started by an ``srun`` from an earlier release still ends
only when that client reports the exit.

Two cases behave differently, by design:

- ``salloc``: exiting an attached ``srun --pty`` closes only that step. The
  allocation stays until you exit the ``salloc`` shell, or until its client has
  been gone past ``InactiveLimit`` (see :ref:`inactive-limit`).
- ``srun --jobid <id> --pty``, an extra session in the job, even one attached
  while the job was still pending: when its shell exits, the job keeps
  running.

If the client is gone *and* the shell keeps running (for example a shell that
ignores the hangup), the ``InactiveLimit`` reaper reclaims the allocation once
the client has been silent past the limit (see :ref:`inactive-limit`). With
``inactive_limit_secs = 0`` the job runs until its time limit.

.. _srun-controller-restart:

Surviving a Controller Restart
------------------------------

Buffered ``srun`` steps, including steps inside ``sbatch`` scripts and
multi-node ``mpirun`` daemon placement, survive a controller restart while
their allocation remains active. ``srun`` makes up to 30 attempts to reach the
controller with a two-second backoff (roughly a minute, plus connection and RPC
time). Agents join an existing step rather than launching it again.

If completion was already recorded, the controller returns its recorded exit
code and attempts to retrieve the original output from the agents, allowing
up to ten seconds per agent in parallel. Settled agent completion records are
retained for up to ten minutes, subject to capacity limits, and output replay
also requires the agent's spool files. An agent restart can recover completion
from its retained on-disk record.

If an output fetch fails, the recorded exit code and any output returned by
other agents are preserved. ``srun`` reports each unavailable node on standard error
as ``srun: output from <node> is no longer available``. Authorization failures
still reject the request. Once the client exhausts its reconnect attempts, it
exits with an error; the payload can remain running until it finishes or is
cancelled with ``scancel``.

These diagnostics are displayed even when workload output was streamed live
or redirected with ``--output`` or ``--error``. They require an updated client
and controller; older clients still receive the recorded exit code and available
workload output.

Surviving an Agent Restart
--------------------------

Work is supervised by a process that outlives the node agent, so an
administrator restarting or upgrading ``spurd`` does not kill it. An allocation
stays held, a running ``srun`` step keeps running and still reports its exit
status. A terminal is the exception: the allocation and its supervisors survive,
but a terminal already open inside it ends, and you reconnect by running
``srun --pty`` again. A step that *finishes* while the agent is down is covered
too: its supervisor records the exit durably, that record is kept for ten
minutes after the step settles, and the restarted agent answers the waiting
caller with the exit and the step's output instead of reporting the step as
lost. Any number of restarts inside that window are covered; a caller that only
comes back after it is still told the step is unknown. A session that nothing
can ever settle — one whose supervisor was killed before it recorded an exit —
is kept for a day and then swept.

If an interactive terminal's command exits before the agent takes its
pseudo-terminal master, the supervisor keeps serving for up to ten seconds
so the agent can finish attaching. Taking the master or requesting teardown
ends this grace early; the supervisor records the exit after the grace ends.
This is separate from the ten-minute retention of a finished step's recorded
result.

One case is not covered: a step given its own ``--container-image``. It ends
when the agent stops, and leaves its unpacked rootfs behind. A standalone
``srun --pty`` that allocates its own job loses the job as well: ``srun``
cancels the job it created as soon as its connection drops. Take an allocation
first and run ``srun --pty`` inside it, so a restart costs you the terminal
rather than the work:

.. code-block:: bash

   salloc -N1
   srun --pty bash     # inside the allocation shell

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
     - Allocated GPU ordinals. Under a root ``spurd`` the job's ``/dev/dri`` holds
       only its own GPUs, which ROCr numbers from zero, so there this holds their
       ranks (``0`` to *n*-1) instead.
   * - ``CUDA_VISIBLE_DEVICES``
     - Allocated GPU ordinals.
   * - ``GPU_DEVICE_ORDINAL``
     - Allocated GPU ordinals.
   * - ``HIP_VISIBLE_DEVICES``
     - ``0``, ``0,1``, and so on. ROCr has already narrowed and renumbered the
       devices by the time HIP reads this, so it counts from zero rather than
       repeating the ordinals above.
   * - ``SPUR_JOB_GPUS``
     - Allocated GPU ordinals (Spur-native).

Under ``srun``, ``--gpu-bind`` adjusts the visible set per task —
``--gpu-bind=closest`` binds each task to its nearest GPU, and
``map_gpu:...``/``mask_gpu:...`` set an explicit per-task mapping or mask.

See Also
--------

- :doc:`submitting-jobs`
- :doc:`monitoring-jobs`
