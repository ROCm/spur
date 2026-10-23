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
   * - ``spur_karma_jobs_submitted_total``
     - Job submitted
     - Reliability / cancellation denominators
   * - ``spur_karma_jobs_completed_total``
     - Job reaches Completed
     - Reliability
   * - ``spur_karma_jobs_failed_total``
     - Job reaches Failed, OutOfMemory, or Deadline
     - Reliability
   * - ``spur_karma_jobs_timeout_total``
     - Job reaches Timeout
     - Reliability
   * - ``spur_karma_jobs_node_fail_total``
     - Job reaches NodeFail
     - Excluded from reliability (not the user's fault)
   * - ``spur_karma_jobs_cancelled_total``
     - Job reaches Cancelled
     - Cancellation rate
   * - ``spur_karma_gpus_requested_total``
     - Job dispatched
     - Resource accuracy
   * - ``spur_karma_overflow_borrows_total``
     - Job dispatched with idle_fill=true
     - Citizenship
   * - ``spur_karma_walltime_requested_seconds_total``
     - Job dispatched
     - Wall time accuracy
   * - ``spur_karma_walltime_actual_seconds_total``
     - Accumulates actual run time across all runs, including restarts after preemption
     - Wall time accuracy

Configuration
-------------

.. code-block:: toml

   [metrics]
   high_cardinality = true   # default false; enables /metrics/jobs-users-accts

No other configuration is required. The counters are accumulated in memory on
the Raft leader. On leader failover or restart, counters reset to zero;
Prometheus ``rate()`` and ``increase()`` handle resets transparently.

.. note::

   A job that started before a failover but finishes after the new leader takes
   over will credit ``walltime_actual_seconds`` with no matching
   ``walltime_requested_seconds`` baseline in the new epoch. This is transient
   and self-correcting: ``rate()`` absorbs the reset, and subsequent jobs
   restore the ratio.

Prometheus recording rules
--------------------------

An example ``prometheus-rules.yaml`` is provided in ``examples/karma/``. It defines
six sub-scores and a weighted composite:

.. code-block:: text

   karma:reliability       = completed / (completed + failed + timeout)    or 1.0
   karma:gpu_utilization   = avg(amd_gpu_gfx_activity{job_user!=""}) / 100 or 0.5
   karma:resource_accuracy = clamp(gpu_utilization, 0, 1)                  or 0.5
   karma:walltime_accuracy = clamp(actual / requested, 0, 1)               or 1.0
   karma:citizenship       = 1 - (overflow / submitted)                    or 1.0
   karma:cancellation_rate = cancelled / submitted                         or 0.0

   karma:score = 0.25 * reliability
               + 0.25 * gpu_utilization
               + 0.20 * resource_accuracy
               + 0.15 * citizenship
               + 0.10 * walltime_accuracy
               + 0.05 * (1 - cancellation_rate)

Each sub-score uses ``or (spur_karma_jobs_submitted_total * 0 + <neutral>)``
to fall back to a neutral default when the denominator is zero (no eligible
observations for that user).

GPU utilization requires the :doc:`dme-integration` prolog hook so that
``amd_gpu_gfx_activity`` carries the ``job_user`` label. Without the hook,
the GPU sub-scores default to 0.5 (neutral) in the composite.

Weights can be adjusted by editing the rules file — no Spur code change
required.
