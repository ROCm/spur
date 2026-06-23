Metrics
=======

This page inventories how spurctld exposes cluster observability: where each
number comes from, which HTTP or RPC paths surface it, and what is not
implemented yet.

Design principle
----------------

Spur separates **authoritative state** from **export paths**:

- **State-derived gauges** — counts and allocations computed by scanning the
  in-memory job or node maps at request time. There is no separate counter
  store that can drift from cluster state.
- **Event accumulators** — not implemented yet. Counters such as “jobs
  submitted since reset” or per-RPC handler microseconds would live in one
  spurctld module and be read by every exporter (HTTP, gRPC, CLI).
- **Export paths** — HTTP ``/metrics/*``, gRPC metrics RPCs, and ``sdiag`` must
  read the same snapshot collectors. They must not each maintain their own
  incrementing counters or re-count raw records client-side.

The ``prometheus_client`` gauges built during an HTTP scrape are ephemeral:
``encode_registered`` creates a fresh registry per request, sets gauge values
from a snapshot, encodes text, and discards the registry.

Unified job and node metrics
----------------------------

Job and node statistics use a single collector per family on the leader:

1. ``ClusterManager::job_metrics()`` / ``node_metrics()`` scan the WAL-backed
   maps and build ``JobMetricsSnapshot`` / ``NodeMetricsSnapshot``.
2. Exporters read that snapshot:
   - HTTP ``/metrics/jobs`` and ``/metrics/nodes`` encode gauges via
     ``spur-metrics/src/export/*``.
   - gRPC ``GetJobMetrics`` and ``GetNodeMetrics`` return proto messages built
     by ``spurctld/src/metrics_proto.rs`` (same field values as the HTTP
     gauges).
   - ``sdiag`` calls the gRPC metrics RPCs and formats the response for the
     terminal.

Raft followers return **503** for metrics HTTP and forward gRPC metrics
requests to the leader (same as other leader-gated reads).

Conversion tests in ``spurctld/src/metrics_proto.rs`` assert that proto
fields match HTTP gauge values for a given snapshot.

Listeners and access
--------------------

+------------------+------------------+------------------------------------------+
| Service          | Default port     | Notes                                    |
+==================+==================+==========================================+
| gRPC controller  | 6817             | ``GetJobMetrics``, ``GetNodeMetrics``,   |
|                  |                  | ``GetJobs``, ``Ping``, mutations         |
+------------------+------------------+------------------------------------------+
| REST API         | 6820             | ``/api/v1/*``, ``/slurm/v0.0.42/*``      |
+------------------+------------------+------------------------------------------+
| Metrics HTTP     | 6822             | ``/metrics/*`` (configurable)            |
+------------------+------------------+------------------------------------------+

Metrics HTTP is controlled by ``[metrics]`` in ``spur.conf``:

- ``enabled`` — when false, spurctld does not start the metrics listener.
- ``listen_addr`` — bind address (default ``[::]:6822``).
- ``bind`` — ``loopback`` (127.0.0.1) or ``all`` (use ``listen_addr`` as-is).
- ``exposition_format`` — ``slurm_0_0_4`` (Prometheus text 0.0.4) or
  ``openmetrics_1_0`` (OpenMetrics 1.0 with ``# EOF`` trailer).
- ``high_cardinality`` — gates ``/metrics/jobs-users-accts`` (not implemented).

Metric families
---------------

Jobs
~~~~

**Source.** ``JobMetricsSnapshot::collect`` in ``crates/spur-metrics/src/job.rs``,
invoked via ``cluster.job_metrics()``.

**Collector logic.**

- Per-state counts for every ``JobState`` variant (including ``out_of_memory``).
- ``held_pending`` — pending jobs with hold reason (included in ``sdiag`` when
  non-zero; not a separate HTTP gauge series).
- ``running_cpus``, ``running_memory_bytes``, ``running_gpus`` — sums over jobs
  in Running or Completing with an allocation.

**HTTP** (``GET /metrics``, ``GET /metrics/jobs``) — gauge names:

- ``spur_jobs``
- ``spur_jobs_<state>`` for each state (``pending``, ``running``, ``completing``,
  ``completed``, ``failed``, ``cancelled``, ``timeout``, ``node_fail``,
  ``preempted``, ``suspended``, ``deadline``, ``out_of_memory``)
- ``spur_jobs_cpus_alloc``
- ``spur_jobs_memory_alloc_bytes``
- ``spur_jobs_gpus_alloc``

**gRPC.** ``GetJobMetrics`` (``JobMetrics`` in ``proto/slurm.proto``).

**CLI ``sdiag`.** ``GetJobMetrics``; per-state lines match HTTP state gauges.

**REST.** ``GET /api/v1/jobs`` returns per-job JSON only (not the metrics
snapshot). Clients must not use this for aggregated counts if they need parity
with ``/metrics/jobs``.

**Accumulators.** None (no “jobs submitted since reset” counter).

Nodes
~~~~~

**Source.** ``NodeMetricsSnapshot::collect`` in ``crates/spur-metrics/src/node.rs``,
invoked via ``cluster.node_metrics()``.

**Collector logic.**

- Cluster-wide state counts and CPU/memory/GPU totals and allocations.
- Per-node labeled series from agent-reported telemetry (``cpu_load``,
  ``free_memory_mb``) and catalog fields (``total_resources``,
  ``alloc_resources``).

**HTTP** (``GET /metrics/nodes``) — gauge names:

- ``spur_nodes``, ``spur_nodes_<state>`` for each ``NodeState``
- ``spur_nodes_cpus``, ``spur_nodes_cpus_alloc``
- ``spur_nodes_memory_bytes``, ``spur_nodes_memory_alloc_bytes``
- ``spur_nodes_gpus``, ``spur_nodes_gpus_alloc``
- ``spur_node_cpus{node}``, ``spur_node_cpus_alloc{node}``
- ``spur_node_memory_bytes{node}``, ``spur_node_memory_alloc_bytes{node}``
- ``spur_node_gpus{node}``, ``spur_node_gpus_alloc{node}``
- ``spur_node_cpu_load{node}``, ``spur_node_free_memory_bytes{node}``

**gRPC.** ``GetNodeMetrics`` (``NodeMetrics`` in ``proto/slurm.proto``),
including ``per_node`` entries for labeled HTTP series.

**CLI ``sdiag`.** ``GetNodeMetrics``; prints cluster-wide totals and per-state
counts. Per-node labeled values are available via HTTP or gRPC but are not
listed line-by-line in ``sdiag`` text output.

**REST.** ``GET /api/v1/nodes`` returns per-node JSON only (not the metrics
snapshot).

Partitions
~~~~~~~~~~

**Source.** Not implemented. ``register_partitions`` in
``crates/spur-metrics/src/export/partitions.rs`` is a stub.

**HTTP** (``GET /metrics/partitions``) — returns an empty body (OpenMetrics EOF
only).

**gRPC / REST.** ``GetPartitions`` and ``GET /api/v1/partitions`` return
partition config; no partition metrics snapshot.

Scheduler
~~~~~~~~~

**Source.** Not implemented. ``register_scheduler`` in
``crates/spur-metrics/src/export/scheduler.rs`` is a stub.

**HTTP** (``GET /metrics/scheduler``) — empty export.

**Runtime.** The backfill scheduler runs in ``scheduler_loop.rs`` but does not
record cycle times, queue depth, or backfill counts for export.

**CLI ``sdiag`.** Prints fixed scheduler description strings (algorithm,
weights, half-life); values are not read from spurctld configuration.

RPC handler timing
~~~~~~~~~~~~~~~~~~

**Source.** Not implemented. No per-method invocation count or handler duration
in spurctld.

**HTTP / gRPC / CLI.** No export. This is distinct from job-state gauges: it
would measure controller request handling cost (for example ``SubmitJob``
including Raft commit wait on the leader).

Per-user and per-account job metrics
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

**HTTP** (``GET /metrics/jobs-users-accts``) — returns 404 unless
``metrics.high_cardinality = true``, and still returns 404 with “deferred”
message. Not implemented.

Cross-path consistency
----------------------

Job and node metrics (unified)
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

+---------------------------+-----------------------------------------------+
| Export path               | How it obtains job/node counts                |
+===========================+===============================================+
| ``GET /metrics/jobs``     | ``job_metrics()`` → HTTP encode               |
| ``GET /metrics/nodes``    | ``node_metrics()`` → HTTP encode              |
| ``GetJobMetrics`` gRPC    | ``job_metrics()`` → ``metrics_proto``         |
| ``GetNodeMetrics`` gRPC   | ``node_metrics()`` → ``metrics_proto``        |
| ``sdiag``                 | ``GetJobMetrics`` + ``GetNodeMetrics``        |
+===========================+===============================================+

All of the above perform a fresh map scan per request. That is repeated work,
not duplicate stored counters. Values should match across paths for the same
leader at the same instant.

APIs that do **not** use the metrics snapshot (callers may recompute counts
themselves; not used by ``sdiag`` or ``/metrics/*``):

- ``GetJobs`` / ``GetJob`` gRPC
- ``GET /api/v1/jobs`` REST

Remaining gaps
~~~~~~~~~~~~~~

+---------------------------+---------------------------+---------------------------+
| Quantity                  | Canonical source today    | Status                    |
+===========================+===========================+===========================+
| Partition metrics         | —                         | HTTP stub                 |
+---------------------------+---------------------------+---------------------------+
| Scheduler diagnostics     | —                         | HTTP stub; ``sdiag`` text |
+---------------------------+---------------------------+---------------------------+
| RPC handler stats         | —                         | Not implemented           |
+---------------------------+---------------------------+---------------------------+
| Job event counters        | —                         | Not implemented           |
| (submitted since reset)   |                           |                           |
+---------------------------+---------------------------+---------------------------+

Event accumulators and RPC stats, when added, should follow the same pattern:
one collector in spurctld, multiple exporters.

Code map
--------

+-------------------------------+-----------------------------------------------+
| Component                     | Role                                          |
+===============================+===============================================+
| ``spur-metrics/src/job.rs``   | ``JobMetricsSnapshot::collect``               |
| ``spur-metrics/src/node.rs``  | ``NodeMetricsSnapshot::collect``              |
| ``spur-metrics/src/export/*`` | Snapshot → Prometheus text encoding           |
| ``spurctld/cluster.rs``       | ``job_metrics()``, ``node_metrics()``         |
| ``spurctld/metrics_proto.rs`` | Snapshot ↔ ``JobMetrics`` / ``NodeMetrics``   |
| ``spurctld/metrics_server``   | HTTP routes, leader gate, config format       |
| ``spurctld/server.rs``        | ``GetJobMetrics``, ``GetNodeMetrics`` handlers|
| ``spur-cli/src/sdiag.rs``     | Formats gRPC metrics for terminal output      |
| ``spurctld/rest/handlers``    | REST JSON; ``/ping`` exposes leader/replica   |
| ``proto/slurm.proto``         | ``JobMetrics``, ``NodeMetrics`` messages      |
+===============================+===============================================+

Tests:

- ``crates/spur-metrics/tests/*_golden.rs`` — HTTP encoding for jobs and nodes.
- ``spurctld/src/metrics_proto.rs`` — proto field parity with HTTP gauges.
