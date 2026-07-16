Partitioning
============

Partitions (Slurm's queues) group nodes so jobs can target a subset of the
cluster with ``-p/--partition``. A node joins a partition if it matches
**either** the partition's hostlist **or** its label selector. The two
mechanisms are independent and combine as a union, so you can mix them.

Label-Based Membership (Recommended)
-------------------------------------

A partition's ``selector`` is a set of ``key = value`` label pairs. A node
joins the partition when it carries **all** of those labels. Nodes declare
their own labels when the agent starts:

.. code-block:: bash

   spurd -D --controller http://10.44.0.1:6817 \
       --label pool=gpu --label rack=a

Repeat ``--label`` once per label to set several. The ``SPUR_NODE_LABELS``
environment variable sets a **single** label only (it is not comma-split), so
setting multiple labels requires repeated ``--label`` flags.

Define the partition by selector:

.. code-block:: toml

   [[partitions]]
   name = "gpu"
   selector = { pool = "gpu" }

Every node started with ``pool=gpu`` joins the ``gpu`` partition, regardless of
its hostname.

**Why this is the recommended approach:** partition membership is decoupled
from hostnames. Adding, replacing, renaming, or scaling nodes needs no change
to the controller config, the node simply advertises its labels on
registration. This suits dynamic or elastic clusters and heterogeneous
hardware, where grouping by role (``pool=gpu``, ``tier=spot``) is clearer and
more durable than enumerating hostnames.

Hostlist-Based Membership
-------------------------

A partition's ``nodes`` field is a Slurm-style hostlist pattern, matched by
exact hostname after expansion. It is **not** a regular expression.

.. code-block:: toml

   [[partitions]]
   name = "cpu"
   nodes = "cpu-node-[1-64]"

Supported forms:

- Bracket ranges and lists: ``node[001-003]``, ``node[1,3,5-7]``
- Comma-joined patterns: ``gpu[01-04],cpu[01-02]``

The special value ``ALL`` (case-insensitive) is not expanded; it short-circuits
to match every node.

Hostlists are convenient for small, static clusters with stable naming.

Combining Both
--------------

Set both fields on one partition and membership is the union: a node joins if
it matches the hostlist **or** the selector.

.. code-block:: toml

   [[partitions]]
   name = "mixed"
   nodes = "gpu-node-4"
   selector = { rack = "a" }

Here ``gpu-node-4`` joins via the hostlist, and every node labeled ``rack=a``
joins via the selector.

The same dual-path matching applies to ``[[nodes]]`` config blocks (which set
features and weight; see :doc:`native-host`), keyed by ``names`` (hostlist) or
``selector`` (labels).

Applying Config Changes
-----------------------

After editing ``spur.conf``, apply the changes to a running controller without
a restart:

.. code-block:: bash

   scontrol reconfigure

``reconfigure`` re-reads ``spur.conf`` and makes the file authoritative:
runtime-only changes not reflected in the file are overwritten.

**Applied live** (no restart): ``[[partitions]]`` (created, updated, or deleted
to match the file), ``[[nodes]]`` features and weight, ``licenses``,
``burst_buffer``, ``[hooks]``, ``[notifications]``, ``[federation]``,
``[power]`` suspend/resume commands, ``[admission]`` mode, and the
``[scheduler]`` tunables ``complete_wait`` and ``resv_overrun``.

**Restart-only**: settings baked in when the daemon starts — listen addresses
and ports (``[controller]``, ``[metrics]``, ``[rest_api]``), the accounting
database (``[accounting]``), Raft identity and peers, ``first_job_id``, and the
scheduler loop cadence (``interval_secs``, ``max_jobs_per_cycle``, topology).
``reconfigure`` reads these but does not apply them; a full controller restart
is required. This mirrors Slurm, where a documented subset of parameters
(ports, ``StateSaveLocation``, plugin set) also require a daemon restart.

Verifying Membership
--------------------

.. code-block:: bash

   # Nodes grouped by partition
   sinfo -o "%P %N"

   # Partitions each node belongs to
   sinfo -N -o "%N %P"
