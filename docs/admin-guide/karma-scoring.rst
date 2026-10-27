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
     - Job dispatched (once per job; deduped across requeues)
     - Reserved for per-GPU resource accuracy (future)
   * - ``spur_karma_overflow_borrows_total``
     - Job dispatched with idle_fill=true (once per job)
     - Citizenship
   * - ``spur_karma_walltime_requested_seconds_total``
     - Job dispatched (once per job; zero when no time limit is set)
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
Prometheus ``increase()`` handles resets transparently.

.. note::

   After a failover the per-job deduplication set is empty, so a job already
   running may have its requested GPUs and walltime counted again on its next
   dispatch, and completions can briefly exceed submissions. The recording rules
   aggregate ``sum by (username)`` across leader terms, so the ratios converge
   as new jobs are submitted. ``walltime_actual_seconds`` accumulates per run
   (including each preempted segment), so a preempted-and-rerun job can show
   actual greater than requested; the ``clamp`` in the walltime accuracy rule
   caps the ratio at 1.

Prometheus recording rules
--------------------------

An example ``prometheus-rules.yaml`` is provided in ``examples/karma/``. It defines
five sub-scores and a weighted composite:

.. code-block:: text

   karma:reliability       = completed / (completed + failed + timeout)    or 1.0
   karma:gpu_utilization   = avg(gpu_gfx_activity{job_user!=""}) / 100     or 0.5
   karma:walltime_accuracy = clamp(actual / requested, 0, 1)               or 1.0
   karma:citizenship       = 1 - (overflow / submitted)                    or 1.0
   karma:cancellation_rate = cancelled / submitted                         or 0.0

   karma:score = 0.30 * reliability
               + 0.30 * gpu_utilization
               + 0.15 * walltime_accuracy
               + 0.15 * citizenship
               + 0.10 * (1 - cancellation_rate)

``spur_karma_gpus_requested_total`` is exported but not consumed by the default
rules. It is reserved for a future per-GPU resource accuracy dimension that
requires a per-GPU→user join key from the exporter.

NaN handling
~~~~~~~~~~~~

A zero denominator (e.g. a user with only cancelled jobs has 0/0 reliability)
produces ``NaN`` in PromQL. ``or`` does not replace an existing ``NaN`` sample.
Each sub-score uses ``(X > -Inf) or (fallback)`` — the ``> -Inf`` comparison
drops ``NaN`` (NaN fails all comparisons), letting the neutral fallback fire.

Label normalization
~~~~~~~~~~~~~~~~~~~

Every counter-based sub-score uses ``sum by (username)`` so that scrape
labels (``instance``, ``job``) and multiple series from leader failovers
are collapsed into a single ``{username}`` series. GPU utilization from
DME carries ``job_user``; the rules relabel it to ``username`` and
aggregate ``avg by (username)`` to drop ``job_user``. All five dimensions
therefore share the same ``{username}`` label set in the composite.

GPU utilization requires the :doc:`dme-integration` prolog hook so that
``amd_gpu_gfx_activity`` carries the ``job_user`` label. Without the hook,
GPU utilization defaults to 0.5 (neutral) in the composite.

Weights can be adjusted by editing the rules file — no Spur code change
required.
