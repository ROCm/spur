Karma Scoring
=============

Spur exports per-user job lifecycle counters that enable a *karma score* — a
composite metric that measures how well each user is using the shared cluster.
The counters are served from ``/metrics/jobs-users-accts`` (requires
``metrics.high_cardinality = true``) alongside the existing per-user gauges.

All score computation happens in Prometheus via recording rules. Spur only
exports raw counters; it never computes sub-scores or the composite itself.

Counters
--------

All counters are labeled ``{username="..."}`` and use the OpenMetrics
``counter`` type (Prometheus appends ``_total`` to sample names).

.. list-table::
   :header-rows: 1
   :widths: 40 30 30

   * - Metric
     - Incremented when
     - Karma dimension
   * - ``spur_user_jobs_submitted_total``
     - Job submitted
     - Reliability / cancellation denominators
   * - ``spur_user_jobs_completed_total``
     - Job reaches Completed
     - Reliability
   * - ``spur_user_jobs_failed_total``
     - Job reaches Failed, OutOfMemory, or Deadline
     - Reliability
   * - ``spur_user_jobs_timeout_total``
     - Job reaches Timeout
     - Reliability
   * - ``spur_user_jobs_node_fail_total``
     - Job reaches NodeFail
     - Excluded from reliability (not the user's fault)
   * - ``spur_user_jobs_cancelled_total``
     - Job reaches Cancelled
     - Cancellation rate
   * - ``spur_user_gpus_requested_total``
     - Job dispatched
     - Resource accuracy
   * - ``spur_user_overflow_borrows_total``
     - Job dispatched with idle_fill=true
     - Citizenship
   * - ``spur_user_walltime_requested_seconds_total``
     - Job dispatched
     - Wall time accuracy
   * - ``spur_user_walltime_actual_seconds_total``
     - Job reaches terminal state
     - Wall time accuracy

Configuration
-------------

.. code-block:: toml

   [metrics]
   high_cardinality = true   # default false; enables /metrics/jobs-users-accts

No other configuration is required. The counters are accumulated in memory on
the Raft leader. On leader failover or restart, counters reset to zero;
Prometheus ``rate()`` and ``increase()`` handle resets transparently.

Prometheus recording rules
--------------------------

An example ``prometheus-rules.yaml`` is provided in ``examples/karma/``. It defines
six sub-scores and a weighted composite:

.. code-block:: yaml

   karma:reliability       = completed / (completed + failed + timeout)
   karma:gpu_utilization   = avg(amd_gpu_gfx_activity{job_user!=""}) / 100
   karma:resource_accuracy = clamp_max(gpu_utilization, 1)
   karma:walltime_accuracy = clamp(actual / requested, 0, 1)
   karma:citizenship       = 1 - (overflow / submitted)
   karma:cancellation_rate = cancelled / submitted

   karma:score = 0.25 * reliability
               + 0.25 * gpu_utilization
               + 0.20 * resource_accuracy
               + 0.15 * citizenship
               + 0.10 * walltime_accuracy
               + 0.05 * (1 - cancellation_rate)

GPU utilization requires the :doc:`dme-integration` prolog hook so that
``amd_gpu_gfx_activity`` carries the ``job_user`` label. Without the hook,
the GPU sub-scores default to 0.5 (neutral) in the composite.

Weights can be adjusted by editing the rules file — no Spur code change
required.
