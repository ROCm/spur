Architecture
============

Spur runs as three binaries: a controller daemon (``spurctld``), a node agent
(``spurd``), and a command-line client (``spur``). This page describes what each
component does, which ports they use, and the core scheduling concepts.

Components
----------

``spurctld`` — Controller
~~~~~~~~~~~~~~~~~~~~~~~~~~~

The controller is the scheduler and the single point of contact for clients. It
serves the gRPC API (the ``SlurmController`` and ``SlurmAccounting`` services) on
port ``6817``. Accounting runs in-process, backed by PostgreSQL, and the REST API
(built on Axum) is served directly by the controller — there are no separate
accounting or REST daemons.

High availability is built in through Raft log replication (openraft) and is
always on: even a single-node deployment runs a one-member Raft cluster. In a
multi-controller cluster, the leader handles all writes and non-leaders forward
requests to it automatically, so clients can talk to any controller.

``spurd`` — Node agent
~~~~~~~~~~~~~~~~~~~~~~~

The node agent runs on every compute node. It registers with the controller,
sends periodic heartbeats, and receives job launch and cancel commands over gRPC
(the ``SlurmAgent`` service) on port ``6818``. Interactive sessions and live job
output stream directly between the client and the agent.

``spur`` — Command-line client
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

``spur`` is a multi-call binary. It talks to the controller on port ``6817`` for
scheduling, admin, and accounting. Invoke it as ``spur <command>`` (for example
``spur submit`` or ``spur queue``), or through Slurm-compatible symlinks such as
``sbatch``, ``squeue``, and ``sinfo``. See :doc:`slurm-compatibility` for the
full command map.

.. note::

   Unlike Slurm, Spur has **no** separate accounting or REST daemons — there is
   no ``slurmdbd`` and no ``slurmrestd``. The controller (``spurctld``) handles
   accounting and the REST API itself. The entire distribution is three binaries:
   ``spurctld``, ``spurd``, and ``spur``.

Ports
-----

.. list-table::
   :header-rows: 1
   :widths: 15 25 60

   * - Port
     - Component
     - Purpose
   * - ``6817``
     - ``spurctld``
     - Controller gRPC API and accounting
   * - ``6818``
     - ``spurd``
     - Node agent gRPC (launch, cancel, I/O streaming)
   * - ``6820``
     - ``spurctld``
     - REST API
   * - ``6821``
     - ``spurctld``
     - Raft replication (controller-to-controller)

These are the defaults; all four are configurable in ``spur.conf`` (``listen_addr``,
``agent_port``, ``rest_addr``, ``raft_listen_addr``). See
:doc:`/admin-guide/configuration`.

Concepts
--------

Partitions
~~~~~~~~~~

A partition is a job queue over a set of nodes. Partitions are defined in the
configuration file (``spur.conf``) and control which nodes a job can run on, along
with access limits such as permitted accounts and default time limits. See
:doc:`/admin-guide/configuration` for partition settings.

Jobs and job steps
~~~~~~~~~~~~~~~~~~~

A *job* is a resource allocation submitted to a partition, typically from a batch
script (``spur submit`` / ``sbatch``) or an interactive allocation
(``spur alloc`` / ``salloc``). A *job step* is a task launched inside an existing
allocation with ``spur run`` / ``srun``; a step shares the parent job's resources
rather than requesting a new allocation.

Associations and QOS
~~~~~~~~~~~~~~~~~~~~~

An association is the ``(cluster, account, user, partition)`` tuple that ties a
user to the resources they may use. A Quality of Service (QOS) applies limits and
priority on top of associations. Both are managed through the accounting service.
See :doc:`/admin-guide/accounting`.

Networking and scheduling
-------------------------

Spur can carry controller and agent traffic over a WireGuard mesh, giving encrypted
transport between nodes without a separate VPN. The mesh is optional — deployments
on a trusted LAN can run traffic directly. See :doc:`/deployment/native-host` for
setup.

Scheduling is GPU-first: GPUs are requested with ``--gres=gpu:...`` (or the
``--gpus`` shorthand), and the agent sets ``ROCR_VISIBLE_DEVICES`` and
``CUDA_VISIBLE_DEVICES`` for the allocated devices at launch.

See Also
--------

- :doc:`/getting-started`
- :doc:`slurm-compatibility`
- :doc:`/admin-guide/configuration`
