Partition Resource Limits
=========================

When several partitions share the same nodes, a busy partition can take
resources that another partition depends on. For example, CPU-only jobs can
fill every core of a GPU node and leave its GPUs idle. Partition resource limits
stop this. They are set per partition in ``spur.conf`` (or at runtime with
``scontrol``) and come in two kinds:

- **Per-node caps**, set with ``max_tres_per_node`` and its shorthand
  ``max_cpus_per_node``. A cap limits what **all** of a partition's jobs
  **together** may hold on **one** node. It applies even when the node is
  otherwise idle, so the rest of the node stays free for the other partitions.
- **Per-job memory policy**, set with ``max_mem_per_node``, ``max_mem_per_cpu``,
  and ``def_mem_per_node`` / ``def_mem_per_cpu`` / ``def_mem_per_gpu``. These
  limit or fill in the memory request of **one** job. They never add up across
  jobs.

Each limit is optional and the two kinds are independent. A partition with
nothing set behaves exactly as before, so existing ``spur.conf`` files need no
change. All memory values are in MiB.

Per-node caps
-------------

``max_tres_per_node`` is a table that maps a resource key to a limit:

.. list-table::
   :header-rows: 1
   :widths: 25 75

   * - Key
     - Counts
   * - ``cpu``
     - CPUs allocated to the partition's jobs on the node. An ``--exclusive``
       job counts every CPU of the node.
   * - ``mem``
     - Memory (MiB) allocated to the partition's jobs on the node.
   * - ``gres/gpu``
     - GPUs of any type allocated to the partition's jobs on the node.
   * - ``gres/gpu:<type>``
     - GPUs of one type, for example ``gres/gpu:mi300x``. GPU types come from
       the node inventory, so nothing else needs to be declared.

Keys are case-sensitive. Any other key (for example ``cpus`` or ``disk``) is a
config load error, so a typo cannot leave a partition uncapped by mistake. Keys
that contain ``/`` or ``:`` must be quoted in TOML.

A job is placed on a node only if, for every capped key, the resources the
partition already uses on that node plus the job's request stay within the
limit:

.. code-block:: text

   used by the partition on the node + requested by the job <= limit

- A key that is not set is unlimited. An empty table is unlimited.
- A limit of ``0`` means none of that resource; ``"gres/gpu" = 0`` keeps
  GPU jobs out of the partition.
- A cap limits only its own partition. Caps of different partitions do not
  need to add up to the node size (see `Overlapping caps`_).
- For a multi-node job, each node's share is checked against that node. A
  ``--gpus`` total is spread over the nodes so that every node's share fits:
  with ``"gres/gpu" = 2``, ``-N2 --gpus=4`` gets 2 GPUs on each node.
- Lowering a cap does not stop running jobs. New jobs wait until usage drops
  below the new limit.

``max_cpus_per_node`` is shorthand for the ``cpu`` key, matching Slurm's
``MaxCPUsPerNode``. Use only one of the two. Setting both is a load error, even
when the values are the same.

Example: keep a GPU node free for GPU jobs
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

Nodes ``gpu[01-08]`` have 128 CPUs, 1 TiB memory, and 8 GPUs, and belong to two
partitions. The ``cpu`` partition may use at most 64 CPUs and 512 GiB on each
node and no GPUs. The ``gpu`` partition is not capped:

.. code-block:: toml

   [[partitions]]
   name = "cpu"
   nodes = "gpu[01-08]"
   max_cpus_per_node = 64
   def_mem_per_cpu = 8192         # a job naming no memory still charges mem

   [partitions.max_tres_per_node]
   mem = 524288
   "gres/gpu" = 0

   [[partitions]]
   name = "gpu"
   nodes = "gpu[01-08]"

However many ``cpu`` jobs are queued, 64 CPUs, 512 GiB, and all 8 GPUs on every
node stay available to the ``gpu`` partition.

A job that requests no memory charges nothing against a ``mem`` cap. Without
``def_mem_per_cpu`` (or another ``def_mem_*`` knob), ``cpu`` jobs that omit
``--mem`` could use more than 512 GiB of a node between them.

GPU types
~~~~~~~~~

A GPU request counts against these keys:

.. list-table::
   :header-rows: 1
   :widths: 40 60

   * - Request
     - Counts against
   * - ``--gres=gpu:a100:2`` (typed)
     - ``gres/gpu:a100`` **and** ``gres/gpu``
   * - ``--gres=gpu:2`` (generic)
     - ``gres/gpu`` only

.. important::

   A generic GPU request is **not** checked against typed caps. With only
   ``"gres/gpu:a100" = 2`` set, a job that asks for ``--gres=gpu:4`` can still
   be given four A100 GPUs, because a generic request charges ``gres/gpu``
   only. To put a firm limit on GPUs, also set ``"gres/gpu"``, which every GPU
   request counts against.

Per-job memory policy
---------------------

These knobs use Slurm's names and meaning (``MaxMemPerNode``,
``MaxMemPerCPU``, ``DefMemPerNode``, ``DefMemPerCPU``, ``DefMemPerGPU``).

``max_mem_per_node``
   Largest memory one job may use on one node, checked on the node that gets
   the most: with ``-N2 -n3 --mem-per-cpu=4G`` that is the node running two
   tasks, 8 GiB. This is a per-job limit, not a partition budget. With only ``max_mem_per_node = 65536`` set, four 64 GiB
   jobs can still fill a 256 GiB node. For a budget shared by all of the
   partition's jobs, use the ``mem`` key of ``max_tres_per_node``.

``max_mem_per_cpu``
   Largest memory per allocated CPU. A job that asks for more is **not**
   rejected. It is given more CPUs instead (see below).

``def_mem_per_node``, ``def_mem_per_cpu``, ``def_mem_per_gpu``
   Memory a job gets in this partition when it requests none. ``def_mem_per_gpu``
   applies only to GPU jobs and is multiplied by the GPUs the job gets on each
   node. A ``--gpus`` total can land unevenly (``-N2 --gpus=4`` may get 3 GPUs
   on one node and 1 on the other), so that job gets 3 × and 1 × the default.
   ``scontrol show job`` then reports ``MinMemoryNode=0``, the per-GPU amount
   as ``MemPerTres=gpu:<MiB>`` (the ``mem_per_tres`` field of ``JobInfo`` in
   the gRPC API), and the job's total in ``ReqTRES`` ``mem=``, as Slurm does
   for memory per GPU. The ``prolog_slurmctld`` and ``epilog_slurmctld`` hooks
   see the most memory any one node of the job was given in
   ``SPUR_JOB_MEMORY_MB``.

.. note::

   In Slurm, ``--mem=0`` asks for all of a node's memory and is checked
   against ``MaxMemPerNode`` as such. Spur has no whole-node memory request:
   ``--mem=0`` is the same as giving no ``--mem``, so the ``def_mem_*``
   default applies and that amount is checked against ``max_mem_per_node``.

``max_mem_per_node`` and ``max_mem_per_cpu`` are mutually exclusive. At most
one of the three ``def_mem_*`` knobs may be set. Both rules are checked when
the config loads and when a partition is created or updated.

Per-job and cumulative memory together
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

Node ``n1`` has 256 GiB and belongs to partitions ``cpu`` and ``gpu``. Partition
``cpu`` sets:

.. code-block:: toml

   [[partitions]]
   name = "cpu"
   nodes = "n1"
   max_mem_per_node = 65536       # one job: at most 64 GiB on a node

   [partitions.max_tres_per_node]
   mem = 131072                   # all cpu jobs on a node: at most 128 GiB

- One ``cpu`` job may use at most 64 GiB on ``n1``.
- All ``cpu`` jobs together may use at most 128 GiB on ``n1``. Two 64 GiB jobs
  fit; a third waits.
- The other 128 GiB on ``n1`` stays available to ``gpu`` no matter how busy
  ``cpu`` is.

How ``max_mem_per_cpu`` adds CPUs
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

The adjustment depends on how the job asked for memory:

- ``--mem-per-cpu`` above the limit: CPUs per task are **multiplied** by
  ``ceil(mem_per_cpu / limit)``, and memory per CPU is divided by the same
  factor, rounded up to a whole MiB. Total memory stays the same or grows by
  a few MiB from that rounding (for example ``-c 2 --mem-per-cpu=5G`` with a
  2048 MiB limit becomes 6 CPUs × 1707 MiB = 10242 MiB); it never shrinks.
- ``--mem`` (per node) above ``CPUs × limit``: CPUs are raised to
  ``ceil(mem / limit)``.
- Memory from ``def_mem_per_gpu``: handled like ``--mem`` for a node holding
  the most GPUs when they are spread as evenly as possible. A node is then
  never given more GPUs than its CPUs allow under the limit.

With ``max_mem_per_cpu = 2048`` (2 GiB):

.. list-table::
   :header-rows: 1
   :widths: 35 65

   * - Request
     - Result
   * - ``-c 2 --mem-per-cpu=2G``
     - Within the limit: 2 CPUs × 2 GiB.
   * - ``-c 2 --mem-per-cpu=3G``
     - Factor ``ceil(3/2) = 2``: 4 CPUs × 1.5 GiB = 6 GiB.
   * - ``-c 2 --mem-per-cpu=5G``
     - Factor ``ceil(5/2) = 3``: 6 CPUs × about 1.67 GiB = 10 GiB.
   * - ``-c 2 --mem=8G``
     - ``ceil(8/2) = 4``: 4 CPUs, 8 GiB.

The ``--mem-per-cpu=3G`` job gets 4 CPUs, not the 3 that would be just enough,
because the CPU count is multiplied, as in Slurm. The extra CPUs count against
the partition's ``cpu`` cap and against association and QOS limits like any
other CPUs.

A job that lists several partitions is checked against association and QOS
limits once per partition, with the request that partition would give it (see
`How caps combine with other limits`_).

The adjustment is made separately for each partition a job may run in. When the
job starts, the adjusted CPU and memory request of the partition it runs in is
recorded on the job and shown by ``scontrol show job``. If the job is requeued,
its original request is restored.

Jobs that request several partitions
------------------------------------

When a job lists several partitions (``-p cpu,wide``) and at least one of them
has a cap or a memory policy, the scheduler picks a partition for the job on
each scheduling pass:

1. Partitions that cannot run the job are skipped: one that is not ``UP``, one
   whose ``MaxTime``, ``MinNodes``, ``MaxNodes``, or ``max_mem_per_node`` the
   job exceeds, one whose cap the job would exceed even on an idle node, and
   one whose adjusted request the job's association or QOS limits refuse.
   A node that only skipped partitions hold is not used. A partition that is
   only full right now is not skipped. The same rule applies to every
   multi-partition job, capped or not.
2. The rest are tried in ``priority_tier`` order, highest first. The job starts
   in the first one where it can start now, so a lower-tier partition with
   room runs it while a higher-tier one is busy. This matches Slurm's
   behaviour for multi-partition jobs.
3. If the job cannot start now in any of them, it waits in the partition where
   it could start earliest, taking the caps into account. Ties go to the higher
   ``priority_tier``.

The order of names in ``--partition`` is not used. One exception: a job whose
QOS or account has a ``GrpTRES`` node cap and that can
:ref:`pack onto nodes its group already occupies <grptres-node-packing>` runs
on those nodes, whatever their partition's tier.

Nothing is fixed while the job waits. If another candidate partition frees up
first, the job starts there. The partition is recorded only when the job starts,
and the job's usage counts against that partition until it ends. A suspended
job keeps its allocation, so it keeps counting. A job preempted in ``cancel``
mode with a :ref:`grace time <qos-preempt-grace>` keeps counting until it exits
or is killed at the end of its grace, so its preemptor starts only once that
share is freed. A job preempted in ``requeue`` mode returns to ``PENDING`` and
stops counting at once, before its processes have exited, so while they shut
down (up to ``kill_wait_secs``) the node can briefly hold more than the cap.
A multi-node job stops counting on each node as soon as it finishes there.

``scontrol show job`` keeps the requested list in ``Partition=`` and adds
``EffectivePartition=`` with the chosen one once the job has started; Slurm
has no separate field for it. The same value is the ``effective_partition``
field of the REST job object (``null`` while pending) and of ``JobInfo`` in the
gRPC API (empty while pending).

A job blocked by a cap is never dropped. It stays ``PENDING`` and is checked
again on every pass. Backfill reserves a future start for it within the cap, so
smaller jobs in the same partition can run before it only if they finish before
that reserved start.

Jobs that can never run as configured
-------------------------------------

Some requests break a partition's limits even on an idle node. Examples: an
``--exclusive`` job on nodes with more CPUs than the ``cpu`` cap, a job that
asks for more of a resource per node than the cap allows, or a job that asks for
more memory per node than ``max_mem_per_node``. A ``--gpus`` total counts as
spread as evenly as possible over the job's nodes: ``-N2 --gpus=4`` puts at
least 2 GPUs on one node however it is split, so a ``gres/gpu`` cap of 1 can
never run it.

``scheduler.enforce_part_limits`` decides what happens to these jobs at submit:

.. list-table::
   :header-rows: 1
   :widths: 40 20 20 20

   * - Violation
     - ``NO`` (default)
     - ``ALL``
     - ``ANY``
   * - Over ``max_mem_per_node``, or over a cap even on an idle node
     - accept, pend
     - reject if any requested partition is violated
     - reject if every requested partition is violated
   * - Over a cap only because of current load
     - pend
     - pend
     - pend

Under ``ALL`` and ``ANY`` the submit is refused with one of these messages:

.. code-block:: text

   Memory specification can not be satisfied: 131072M per node exceeds max_mem_per_node 65536M of partition 'cpu'
   Partition cpu can never run this job as configured: exclusive job needs 128 CPUs, max_tres_per_node.cpu is 64 on every eligible node

Under ``NO`` the job is accepted and pends. A job that no requested partition's
cap could ever hold pends with reason ``PartitionTresLimit`` and a description
of the form ``Partition <name> can never run this job as configured: ...``.
It is held back before association and QOS limits are checked, so it holds no
group quota that other jobs could use. A job over ``max_mem_per_node`` pends with reason
``PartitionConfig``.

Freeing resources does not help these jobs. They start, without being
resubmitted, once something changes: an administrator raises the cap, a
smaller node joins the partition (for an ``--exclusive`` job), or the job is
updated to ask for less or to use another partition.

Pending reason
--------------

A job held back by a cap pends with the reason code ``PartitionTresLimit``.
``squeue`` and ``scontrol show job`` show a description in place of the bare
code, naming the partition, node, resource, and numbers:

.. code-block:: text

   Partition cpu limit reached on n03: cpu 48 in use + 24 requested = 72 > max 64
   Partition gpu limit reached on n07: gres/gpu:a100 2 in use + 1 requested = 3 > max 2
   Partition cpu memory limit reached on n03: mem 120G in use + 16G requested = 136G > max 128G (7 other nodes also over limit)
   Partition cpu can never run this job as configured: exclusive job needs 128 CPUs, max_tres_per_node.cpu is 64 on every eligible node

When several nodes block the job, the description names the node closest to
fitting and counts the others. When several resources are over on that node,
the first in the order ``cpu``, ``mem``, ``gres`` is named.

Setting limits at runtime
-------------------------

``scontrol show partition`` prints the limits on two lines, in Slurm's
spelling, with ``UNLIMITED`` for anything unset. ``MaxCPUsPerNode`` shows the
``cpu`` cap however it was set. The second line names the ``DefMemPer*`` and
``MaxMemPer*`` knobs that are set, or reads
``DefMemPerNode=UNLIMITED MaxMemPerNode=UNLIMITED`` when none is:

.. code-block:: text

   MaxCPUsPerNode=64 MaxTRESPerNode=cpu=64,gres/gpu=0,mem=524288
   DefMemPerCPU=1024 MaxMemPerNode=65536

``scontrol create partition`` and ``scontrol update`` accept the same names
(memory in MiB):

.. code-block:: bash

   scontrol update PartitionName=cpu MaxCPUsPerNode=64
   scontrol update PartitionName=cpu MaxTRESPerNode=cpu=64,mem=131072,gres/gpu:a100=2
   scontrol update PartitionName=cpu MaxMemPerCPU=2048 DefMemPerGPU=8192

- ``MaxTRESPerNode=`` replaces the whole cap table. ``MaxTRESPerNode=`` with an
  empty value removes every cap.
- ``UNLIMITED`` or ``0`` leaves ``MaxCPUsPerNode``, ``MaxMemPerNode``,
  ``MaxMemPerCPU``, ``DefMemPerNode``, ``DefMemPerCPU``, or ``DefMemPerGPU``
  unset on create and clears it on update. To forbid a resource, use
  ``MaxTRESPerNode`` with a ``0`` limit, for example
  ``MaxTRESPerNode=gres/gpu=0``. ``MaxTRESPerNode`` entries take numbers only;
  leave a key out to make it unlimited.
- The same rules as in ``spur.conf`` apply: unknown keys, ``MaxCPUsPerNode``
  and a ``cpu`` key in the same command, and conflicting memory knobs are
  rejected.
- ``scontrol reconfigure`` applies the values in ``spur.conf`` again,
  replacing limits changed with ``scontrol``.

The REST ``GET /api/v1/partitions`` (and ``/slurm/v0.0.42/partitions``) response
includes ``max_cpus_per_node``, ``max_tres_per_node``, ``max_mem_per_node``,
``max_mem_per_cpu``, ``def_mem_per_node``, ``def_mem_per_cpu``, and
``def_mem_per_gpu`` for each partition. Unset values are ``null``;
``max_tres_per_node`` is an object, empty when there are no caps.

How caps combine with other limits
----------------------------------

Spur applies the job's QOS, its user and account association, the partition
it runs in, and physical node capacity together. A job runs only if it passes
all of them. A partition cap never loosens an association or QOS limit, and the
other way round. This is not Slurm's hierarchy, where the first layer that
defines a limit wins. Inheriting limits from parent accounts or the root, and
partition QOS, are not supported.

Partition caps (``max_tres_per_node``) are additional constraints on top of the
association and QOS limits. A partition's memory policy can change a job's CPU
and memory request (see `How max_mem_per_cpu adds CPUs`_), and each partition
a job may run in is judged with its own adjusted request, the same one at every
step:

- **Submit.** A ``DenyOnLimit`` QOS, or no QOS, rejects the job only if the
  adjusted request of every requested partition breaks a QOS or association
  limit on its own. The job is accepted if one of them fits.
- **Quota check while pending.** Association and QOS limits are checked with
  each partition's adjusted request. The job may then start only in the
  partitions whose request passes. If none passes, it pends with the reason of
  the first requested partition's failing limit.
- **Placement and start.** The scheduler picks among the remaining partitions
  and the job starts with the request of the partition it picked. That request
  is what it is charged for against association, QOS, and partition limits
  until it ends.

Example: partition ``a`` sets ``max_mem_per_cpu = 2048`` and partition ``b``
sets ``max_mem_per_cpu = 1024``. ``-p a,b -c 2 --mem-per-cpu=3G`` needs 4 CPUs
in ``a`` and 6 CPUs in ``b``. With an association ``GrpTRES=cpu=4`` the job
can only run in ``a``, so it runs there; ``b`` is not considered for it. With
``GrpTRES=cpu=3`` neither fits and the job pends with ``AssocGrpCpuLimit``.

Within one scheduling pass, a job that could run in several partitions holds
the largest of their requests against shared association and QOS limits until
it starts, so jobs queued behind it in the same pass are never admitted past a
limit. Once it starts, only the request of the partition it runs in is charged.

Jobs started by :doc:`idle-fill-scheduling` count against partition caps like
any other running job, and an idle-fill start must fit within the cap.

Overlapping caps
----------------

Caps are independent per partition. The caps of the partitions that share a
node may add up to more than the node has; those partitions then compete for
the overlap. This is allowed, but when two or more partitions on a node cap
the same key and their caps add up to more than the node's capacity,
``spurctld`` logs this warning, with fields ``node``, ``resource``, ``total``,
``capacity``, and ``partitions``:

.. code-block:: text

   partition caps on this node add up to more than it has; those partitions can contend for it

The check runs when the scheduler starts, when a node registers or reports
new resources, after ``scontrol reconfigure`` (for every node), and after
``scontrol create partition`` or ``scontrol update partition`` (for the nodes
in that partition). It is not repeated on scheduling passes, so the warning
for a node whose caps exceed it is logged again only at the next of these
events.

Upgrading
---------

Upgrade every controller replica before you configure any cap or memory
knob, and remove them before you downgrade. See
:ref:`upgrading-partition-resource-limits` for the order and the rollback
steps. Existing configurations are not affected: with no limit set,
scheduling and ``enforce_part_limits`` behave as before.
