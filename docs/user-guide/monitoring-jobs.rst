Monitoring & Controlling Jobs
=============================

Once jobs are submitted, you inspect the queue, the nodes, and the accounting
history with a handful of commands, and you steer running jobs with cancel,
hold, and update operations. This page covers viewing the queue (``spur queue``),
nodes and partitions (``spur nodes``), accounting history (``spur history``),
live job stats (``spur stat``), detailed records (``spur show``), and the
controls that change a job's state.

.. note::

   In ``spur queue`` (``squeue``) and ``spur nodes`` (``sinfo``), ``-h`` means
   ``--noheader``, not help. ``spur history`` (``sacct``) uses ``-n`` for the
   same purpose.

View the Queue — ``squeue``
---------------------------

``spur queue`` (Slurm ``squeue``) lists jobs currently in the system. With no
arguments it shows every active job.

.. code-block:: bash

   spur queue

.. code-block:: text

   JOBID PARTITION     NAME     USER ST       TIME  NODES NODELIST(REASON)
       1       gpu train-ll    alice  R       2:14      2 node[01-02]
       2   default    hello      bob PD       0:00      1 (Priority)

The default columns are ``JOBID PARTITION NAME USER ST TIME NODES
NODELIST(REASON)``. ``--long``/``-l`` adds the full STATE and TIME_LIMIT.

Common flags:

.. list-table::
   :header-rows: 1
   :widths: 26 10 64

   * - Long
     - Short
     - Description
   * - ``--user``
     - ``-u``
     - Show only this user's jobs.
   * - ``--partition``
     - ``-p``
     - Filter by partition.
   * - ``--states``
     - ``-t``
     - Filter by state codes or names (comma list). ``-t all`` shows every
       state.
   * - ``--account``
     - ``-A``
     - Filter by account.
   * - ``--name``
     - ``-n``
     - Filter by job name.
   * - ``--format``
     - ``-o``
     - Custom column format (see below).
   * - ``--long``
     - ``-l``
     - Long form; adds STATE and TIME_LIMIT.
   * - ``--noheader``
     - ``-h``
     - Omit the header line.

When ``--states`` is omitted the default filter is **PENDING, RUNNING,
SUSPENDED, COMPLETING**.

``--format``/``-o`` uses ``%[.][-][width]<letter>`` fields. The resolved letters:

.. list-table::
   :header-rows: 1
   :widths: 18 40 18 40

   * - Letter
     - Column
     - Letter
     - Column
   * - ``%i`` / ``%A``
     - JOBID
     - ``%C``
     - CPUS
   * - ``%j`` / ``%n``
     - NAME
     - ``%N``
     - NODELIST
   * - ``%u``
     - USER
     - ``%R``
     - NODELIST(REASON)
   * - ``%a``
     - ACCOUNT
     - ``%r``
     - REASON
   * - ``%P``
     - PARTITION
     - ``%p``
     - PRIORITY
   * - ``%q``
     - QOS
     - ``%Z``
     - WORK_DIR
   * - ``%t``
     - ST (short code)
     - ``%o``
     - COMMAND
   * - ``%T``
     - STATE (full)
     - ``%v``
     - RESERVATION
   * - ``%M``
     - TIME
     - ``%S``
     - START_TIME
   * - ``%l``
     - TIME_LIMIT
     - ``%V``
     - SUBMIT_TIME
   * - ``%D``
     - NODES
     - ``%e``
     - END_TIME

.. code-block:: bash

   spur queue -u alice -t R
   squeue -p gpu -o "%.18i %.9P %.8T %.10M %R"
   squeue --states=PD,R --noheader

**Job state codes.** The ``ST`` column uses these short codes:

.. list-table::
   :header-rows: 1
   :widths: 12 30 12 30

   * - Code
     - State
     - Code
     - State
   * - ``PD``
     - PENDING
     - ``CA``
     - CANCELLED
   * - ``R``
     - RUNNING
     - ``TO``
     - TIMEOUT
   * - ``CG``
     - COMPLETING
     - ``NF``
     - NODE_FAIL
   * - ``CD``
     - COMPLETED
     - ``PR``
     - PREEMPTED
   * - ``F``
     - FAILED
     - ``S``
     - SUSPENDED
   * - ``DL``
     - DEADLINE
     - ``OOM``
     - OUT_OF_MEMORY

For a pending job the ``NODELIST(REASON)`` column shows why it is waiting.
Common reasons include ``Priority`` (waiting its turn), ``Resources`` (waiting
for nodes to free up), ``Dependency`` (waiting on another job), ``Reservation``
(waiting for a reservation window), and various QOS or association limit reasons
(``QOSMax*``, ``AssocMax*``, ``AssocGrp*``).

View Nodes & Partitions — ``sinfo``
-----------------------------------

``spur nodes`` (Slurm ``sinfo``) shows partition and node state. The default is a
partition-oriented view; ``-N`` switches to one line per node.

.. code-block:: bash

   spur nodes

.. code-block:: text

   PARTITION AVAIL TIMELIMIT NODES STATE NODELIST
   default*     up  infinite     4  idle node[01-04]
   gpu          up  1-00:00:00   2   mix node[05-06]

Flags:

.. list-table::
   :header-rows: 1
   :widths: 26 10 64

   * - Long
     - Short
     - Description
   * - ``--partition``
     - ``-p``
     - Filter by partition.
   * - ``--nodes``
     - ``-n``
     - Filter by node.
   * - ``--node-oriented``
     - ``-N``
     - One line per node instead of per partition.
   * - ``--format``
     - ``-o``
     - Custom column format.
   * - ``--long``
     - ``-l``
     - Long form; adds CPUS and MEMORY.
   * - ``--noheader``
     - ``-h``
     - Omit the header line.

Default columns (partition view): ``PARTITION AVAIL TIMELIMIT NODES STATE
NODELIST``; ``-l`` adds CPUS and MEMORY. Node view (``-N``): ``NODELIST NODES
PARTITION STATE CPUS MEMORY GRES``.

.. code-block:: bash

   sinfo -N -o "%N %.6D %.11T %c %m %G"
   sinfo -p gpu -l

Node states are shown as short abbreviations: ``idle`` (free), ``alloc`` (fully
allocated), ``mix`` (partly allocated), ``down``, ``drain`` (offline, not
accepting jobs), ``drng`` (draining), ``err`` (error), ``unk``
(unknown/unreachable), ``susp`` (suspended), and ``resv`` or ``maint`` for a node
held by a reservation.

Accounting History — ``sacct``
-------------------------------

``spur history`` (Slurm ``sacct``) reports finished and running jobs from the
accounting service.

.. code-block:: bash

   spur history -u alice -S now-7days

Flags:

.. list-table::
   :header-rows: 1
   :widths: 26 10 64

   * - Long
     - Short
     - Description
   * - ``--user``
     - ``-u``
     - Filter by user.
   * - ``--account``
     - ``-A``
     - Filter by account.
   * - ``--starttime``
     - ``-S``
     - Earliest submit/start time to include.
   * - ``--endtime``
     - ``-E``
     - Latest time to include.
   * - ``--state``
     - ``-s``
     - Filter by state (comma list).
   * - ``--format``
     - ``-o``
     - Comma-separated field names (see below).
   * - ``--long``
     - ``-l``
     - Long form; adds DerivedExitCode, Start, End, TimeLimit.
   * - ``--brief``
     - ``-b``
     - Brief form: JobID, State, ExitCode.
   * - ``--noheader``
     - ``-n``
     - Omit the header line.
   * - ``--limit``
     -
     - Maximum rows to return. Default ``100``.

Unlike ``squeue``, ``--format`` here takes **comma-separated field names**, not
``%`` letters. Available fields include ``JobID``, ``JobName``, ``User``,
``Account``, ``Partition``, ``State``, ``Elapsed``, ``NNodes``, ``ExitCode``,
``DerivedExitCode``, ``Start``, ``End``, ``Submit``, ``TimeLimit``, ``NodeList``,
``NCPUS``, and ``QOS``. Set a per-field width with ``Field%N``, e.g.
``JobName%20``.

The default columns are ``JobID JobName User Account Partition State Elapsed
NNodes ExitCode``.

Time arguments accept an absolute date (``YYYY-MM-DD`` or
``YYYY-MM-DDTHH:MM:SS``) or a relative offset (``now-7days``, ``now-6hours``).

.. code-block:: bash

   sacct -S 2026-07-01 -E 2026-07-25 -s FAILED --limit 500
   sacct --format=JobID,JobName,State,Elapsed,ExitCode

Running-Job Stats — ``sstat``
-----------------------------

``spur stat`` (Slurm ``sstat``) reports live resource usage for running jobs.
``--jobs``/``-j`` is required (comma list).

.. code-block:: bash

   spur stat -j 12345
   sstat -j 12345,12346 -o JobID,NTasks,GPUAlloc,Elapsed -p

Default fields: ``JobID NTasks NCPUS MemAlloc GPUAlloc Elapsed Nodelist``.
``--format``/``-o`` takes field names; ``--parsable``/``-p`` produces
``|``-delimited output.

.. note::

   The per-process ``Ave*`` and ``Max*`` fields (``AveCPU``, ``AveRSS``,
   ``MaxRSS``, and similar) always report ``N/A`` — Spur does not collect
   per-process metrics.

Detailed Records — ``scontrol show``
------------------------------------

``spur show`` (Slurm ``scontrol show``) prints the full ``Key=Value`` record for
an entity: ``job``, ``node``, ``partition``, ``reservation``, or ``step``.

.. code-block:: bash

   scontrol show job 1024
   spur show node node01
   scontrol show partition gpu

Cluster Metrics — ``/metrics``
------------------------------

Beyond the per-job CLI commands above, ``spurctld`` exports cluster-wide metrics
over HTTP in Prometheus/OpenMetrics text format. This is the surface a
monitoring stack (Prometheus, Grafana, an OpenMetrics scraper) consumes to chart
queue depth, node utilization, and resource allocation over time.

The server is controlled by the ``[metrics]`` section of ``spur.conf`` (see
:doc:`/admin-guide/configuration`). It listens on port ``6822`` and by default
binds to loopback only; set ``bind = "all"`` to expose it to a scraper on
another host. Only the Raft **leader** serves data — followers return
``503 Service Unavailable`` — so point your scraper at all controllers and let it
follow the leader.

.. code-block:: bash

   curl http://127.0.0.1:6822/metrics/jobs
   curl http://127.0.0.1:6822/metrics/nodes

Endpoints
~~~~~~~~~

.. list-table::
   :header-rows: 1
   :widths: 34 16 50

   * - Path
     - Status
     - Contents
   * - ``/metrics``
     - Live
     - Alias for ``/metrics/jobs``.
   * - ``/metrics/jobs``
     - Live
     - Job counts by state and aggregate allocated resources.
   * - ``/metrics/nodes``
     - Live
     - Node counts by state, cluster resource totals, and per-node gauges.
   * - ``/metrics/partitions``
     - Planned
     - Route exists but currently returns an empty body.
   * - ``/metrics/scheduler``
     - Planned
     - Route exists but currently returns an empty body.
   * - ``/metrics/k8s``
     - Live
     - Spur-managed k0s cluster lifecycle and per-node health (``spur_k8s_*``).
   * - ``/metrics/jobs-users-accts``
     - Planned
     - Per-user/per-account breakdown. Returns ``404`` until implemented, even
       with ``high_cardinality = true``.

Job metrics — ``/metrics/jobs``
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

All gauges. ``<state>`` expands to one metric per job state: ``pending``,
``running``, ``completing``, ``completed``, ``failed``, ``cancelled``,
``timeout``, ``node_fail``, ``preempted``, ``suspended``, ``deadline``,
``out_of_memory``.

.. list-table::
   :header-rows: 1
   :widths: 44 56

   * - Metric
     - Description
   * - ``spur_jobs``
     - Total number of jobs.
   * - ``spur_jobs_<state>``
     - Number of jobs in each state.
   * - ``spur_jobs_cpus_alloc``
     - CPUs allocated to running/completing jobs.
   * - ``spur_jobs_memory_alloc_bytes``
     - Memory (bytes) allocated to running/completing jobs.
   * - ``spur_jobs_gpus_alloc``
     - GPUs allocated to running/completing jobs.

Node metrics — ``/metrics/nodes``
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

All gauges. Cluster-wide totals carry no labels; per-node gauges carry a
``node=<name>`` label. ``<state>`` expands to one metric per node state:
``idle``, ``alloc``, ``mixed``, ``down``, ``drain``, ``draining``, ``error``,
``unknown``, ``suspended``.

.. list-table::
   :header-rows: 1
   :widths: 44 56

   * - Metric
     - Description
   * - ``spur_nodes``
     - Total number of nodes.
   * - ``spur_nodes_<state>``
     - Number of nodes in each state.
   * - ``spur_nodes_cpus`` / ``spur_nodes_cpus_alloc``
     - Total and allocated CPUs across all nodes.
   * - ``spur_nodes_memory_bytes`` / ``spur_nodes_memory_alloc_bytes``
     - Total and allocated memory (bytes) across all nodes.
   * - ``spur_nodes_gpus`` / ``spur_nodes_gpus_alloc``
     - Total and allocated GPUs across all nodes.
   * - ``spur_node_cpus`` / ``spur_node_cpus_alloc``
     - Total and allocated CPUs on the labeled node.
   * - ``spur_node_memory_bytes`` / ``spur_node_memory_alloc_bytes``
     - Total and allocated memory (bytes) on the labeled node.
   * - ``spur_node_gpus`` / ``spur_node_gpus_alloc``
     - Total and allocated GPUs on the labeled node.
   * - ``spur_node_cpu_load``
     - CPU load reported by the node agent.
   * - ``spur_node_free_memory_bytes``
     - Free memory (bytes) reported by the node agent.

k0s metrics from spurctld — ``/metrics/k8s``
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

When Spur manages a k0s cluster (``spur k8s up``, see
:doc:`/deployment/managed-kubernetes`), ``spurctld`` exports its own view of the
cluster lifecycle and per-node health at ``/metrics/k8s``. Every series carries
``distribution="k0s"`` and ``cluster="<cluster-name>"``.

.. list-table::
   :header-rows: 1
   :widths: 46 54

   * - Metric
     - Description
   * - ``spur_k8s_cluster_phase{phase}``
     - Current cluster phase as a one-hot set (``down``, ``provisioning``,
       ``ready``, ``degraded``); value 1 on the active phase.
   * - ``spur_k8s_cluster_up``
     - 1 when the cluster phase is Ready (primary alerting signal).
   * - ``spur_k8s_control_plane_replicas``
     - Configured control-plane replica count.
   * - ``spur_k8s_nodes_total`` / ``spur_k8s_nodes_by_role{role}``
     - Total nodes with a k0s role, and the per-role count.
   * - ``spur_k8s_provision_attempts_total`` / ``spur_k8s_provision_failures_total``
     - Provisioning attempts and attempts that gave up before Ready.
   * - ``spur_k8s_phase_transitions_total{from,to}``
     - Phase transitions labeled by source and destination.
   * - ``spur_k8s_reconcile_duration_seconds`` / ``spur_k8s_reconcile_errors_total``
     - Reconcile-loop iteration wall time (histogram) and error count.
   * - ``spur_k8s_node_up{node}``
     - Whether a node's k0s systemd unit reports active.
   * - ``spur_k8s_node_restart_total{node}`` / ``spur_k8s_install_duration_seconds{node}``
     - Per-node k0s unit restarts and install time (histogram).

k0s component metrics *(incoming)*
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

.. note::

   The endpoints in this section are **incoming / in progress** and are separate
   from the ``spurctld`` ``/metrics/k8s`` surface above. The bundled k0s
   components each expose their own upstream Kubernetes ``/metrics`` endpoint;
   Spur does not yet aggregate or proxy these — the ports and paths below are the
   standard Kubernetes surfaces documented here for planning a scrape
   configuration.

These endpoints are served by the k0s-managed components, not by ``spurctld``.
Control-plane endpoints live on the control-plane node; the kubelet endpoints
live on every node. Most require TLS and authentication.

.. list-table::
   :header-rows: 1
   :widths: 26 20 20 34

   * - Component
     - Port / Path
     - Status
     - Contents
   * - kube-apiserver
     - ``6443`` ``/metrics``
     - Incoming
     - API request latency/counts, etcd cache, admission timings.
   * - kubelet
     - ``10250`` ``/metrics``
     - Incoming
     - Node agent, pod lifecycle, and volume stats.
   * - kubelet (cAdvisor)
     - ``10250`` ``/metrics/cadvisor``
     - Incoming
     - Per-container CPU/memory/network/disk usage.
   * - kubelet (resource)
     - ``10250`` ``/metrics/resource``
     - Incoming
     - Node/pod CPU and memory for the metrics pipeline.
   * - kube-scheduler
     - ``10259`` ``/metrics``
     - Incoming
     - Scheduling attempts, latency, and queue depth.
   * - kube-controller-manager
     - ``10257`` ``/metrics``
     - Incoming
     - Controller work queues and reconcile timings.
   * - etcd
     - ``2379`` ``/metrics``
     - Incoming
     - Datastore health, latency, and DB size.
   * - CoreDNS
     - ``9153`` ``/metrics``
     - Incoming
     - Cluster DNS query counts, latency, and cache stats.

Controlling Jobs
----------------

**Cancel jobs** with ``spur cancel`` (Slurm ``scancel``), by job ID or by filter:

.. code-block:: bash

   scancel 1024 1025
   spur cancel -u alice -p gpu --state PENDING
   scancel --signal SIGTERM 2048

Filter flags include ``--user``/``-u`` (defaults to the current user in filter
mode), ``--partition``/``-p``, ``--state``/``-t`` (only ``PD`` or ``R``),
``--name``/``-n``, ``--account``/``-A``, and ``--signal``/``-s`` (``KILL``/9,
``TERM``/15, ``INT``/2, and others). You must supply at least job IDs,
``--user``, or ``--name``. In filter mode, jobs already in a terminal state are
silently skipped.

**Change job state** with ``spur control`` (Slurm ``scontrol``):

.. code-block:: bash

   scontrol hold 1024       # prevent from starting
   scontrol release 1024    # allow a held job to start
   scontrol requeue 1024    # return a job to the queue
   scontrol suspend 1024    # SIGSTOP, keep the allocation
   scontrol resume 1024     # SIGCONT

**Update a job or node** with ``scontrol update`` and ``Key=Value`` pairs. Job
updates need ``JobId=`` and accept ``Priority=``, ``TimeLimit=``, ``Partition=``,
``Account=``, ``Comment=``, and ``QOS=``. Node updates need ``NodeName=`` and
accept ``State=`` and ``Reason=``.

.. code-block:: bash

   scontrol update JobId=1024 TimeLimit=2:00:00 Priority=100
   scontrol update NodeName=node01 State=drain Reason="maintenance"

See Also
--------

- :doc:`submitting-jobs`
- :doc:`/admin-guide/accounting`
