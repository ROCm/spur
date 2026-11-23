Checkpoint Telemetry Metrics
============================

Jobs can optionally report checkpoint events to Spur (see
:doc:`/user-guide/checkpoint-telemetry`). This page documents the metrics that
telemetry feeds, how the controller derives them, and the socket directory
``spurd`` hardens to collect it. There is no configuration specific to this
feature — it needs no ``spur.conf`` field to turn on, and nothing here is
gated by anything except :ref:`metrics.high_cardinality <checkpoint-metrics-high-cardinality>`
for the per-job series.

How the data gets to the controller
------------------------------------

Each step's ``spurstepd`` collects the events its own job sends, folds them
into an in-memory record, and rewrites ``checkpoint.json`` in its session
directory at most every 5 seconds while it changes, plus once more, marked
final, at step end. ``spurd`` polls every live step's file every 30 seconds,
collects changed records, and ships whichever ones changed since the last
acknowledged send to the controller's ``ReportCheckpointTelemetry`` RPC — the
same cadence and batching as the rest of node reporting, kept separate from
the heartbeat. A batch is capped at 1024 records; a controller that does not
yet implement the RPC gets this logged once per node, after which that node
backs off for 10 minutes before retrying (records stay queued the whole time,
so nothing is lost across the backoff — only delayed). A transient failure
(the controller unreachable, busy, or timing out) also leaves records
queued for the next attempt. A permanent rejection (for example, a malformed
request) is logged once per status code and the batch is dropped rather than
resent forever, since sending the same records again would only repeat the same
rejection.

The controller holds this per-run state only on the current Raft leader, in
memory, keyed by job, run attempt, node, and step; it is never written to the
Raft log or snapshot. A run's records are capped at 4096 distinct
(node, step) entries.

Aggregate metrics — ``/metrics/jobs``
--------------------------------------

These are long-lived counters and histograms, appended to the existing
``/metrics/jobs`` endpoint. Unlike the per-user karma counters, they are
**not** reset when a controller gains leadership — only a process restart
resets them. No series is pre-seeded: a ``(reason, checkpoint)`` pair, a
``preempt_grace`` outcome, or a ``reason`` appears only once something has
actually produced it.

A run that never reported a single checkpoint event (no completed or failed
checkpoint, none in progress, no restore, across every record it sent) is
**not recorded** in any of these families at all — not ``spur_job_runs_ended_total``,
either histogram, or ``spur_preempt_grace_checkpoint_total``. A run that did
report at least one event is recorded exactly as before, including a
failed-only run (see the ``checkpoint`` label below).

.. list-table::
   :header-rows: 1
   :widths: 46 12 42

   * - Metric
     - Type
     - Labels
   * - ``spur_job_runs_ended_total``
     - counter
     - ``reason``, ``checkpoint``
   * - ``spur_job_runs_restored_total``
     - counter
     - ``reason``
   * - ``spur_job_run_end_seconds_since_last_checkpoint``
     - histogram
     - ``reason``
   * - ``spur_job_run_checkpoints_completed``
     - histogram
     - ``reason``
   * - ``spur_preempt_grace_checkpoint_total``
     - counter
     - ``outcome``
   * - ``spur_checkpoint_events_dropped_total``
     - counter
     - ``reason``
   * - ``spur_job_run_checkpoints_failed_total``
     - counter
     - ``reason``

.. _checkpoint-metrics-job-runs-ended:

``spur_job_runs_ended_total{reason, checkpoint}``
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

One count per finalized run that reported at least one checkpoint event (a
job's single run attempt reaching a terminal state; a run with no events at
all is skipped entirely, see above).

- ``reason`` is the finalized ``JobState`` in lowercase snake_case:
  ``completed``, ``failed``, ``cancelled``, ``timeout``, ``node_fail``,
  ``preempted``, ``deadline``, ``out_of_memory``, and so on.
- ``checkpoint`` is the run's checkpoint state at the moment it ended:

  - ``none`` — the run had at least one checkpoint event, but none completed
    and none was in progress when the run ended (for example, a run whose
    only checkpoints failed, or one that only reported a restore).
  - ``completed`` — the run's last checkpoint-shaped activity was a completed
    checkpoint (no later in-progress checkpoint past it).
  - ``in_progress`` — a checkpoint was in progress at or past the run's last
    completed step when the run ended.

``spur_job_runs_restored_total{reason}``
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

One count per finalized run that reported at least one restore (any node
record with a restore step set), labeled by the same ``reason`` as
``spur_job_runs_ended_total``. A run that reports only a restore and nothing
else still counts here (and on ``spur_job_runs_ended_total``, with
``checkpoint="none"``) — the restore itself is an event. See
:ref:`preemption-grace-user` and the :doc:`/user-guide/checkpoint-telemetry`
page for what a restore means and when a job should report one.

``spur_job_run_end_seconds_since_last_checkpoint{reason}``
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

Histogram of seconds between a finalized run's last *completed* checkpoint and
the run ending — the lost-work signal: how long a run computed since the
checkpoint it will actually resume from. Only observed for a run that
completed at least one checkpoint (no observation at all means ``none`` on the
``checkpoint`` label above, not a zero sample here).

Bucket boundaries (seconds): ``60, 120, 240, 480, 960, 1920, 3840, 7680,
15360, 30720, 61440, 122880, +Inf``.

``spur_job_run_checkpoints_completed{reason}``
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

Histogram of how many checkpoints a finalized run completed over its
lifetime, one observation per run (always observed, including ``0`` for a run
that never completed one).

Bucket boundaries (count): ``1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024,
2048, +Inf``.

``spur_preempt_grace_checkpoint_total{outcome}``
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

One count per run that entered a preemption grace window (see
:ref:`preemption-grace-user` in the user guide) and then ended, labeled by
what happened to its checkpoint during that window:

- ``completed`` — the run completed a checkpoint at or after the grace
  started.
- ``failed`` — the run recorded a failed checkpoint, and did not complete one
  at or after the grace started.
- ``none`` — neither: the grace window closed with no checkpoint activity at
  all.

Absent (no sample under any outcome) for a run that never entered a grace
window.

.. important::

   **``failed`` is an approximation.** The wire record carries no timestamp
   for a *failed* checkpoint (only for a completed one), so ``failed`` here
   means "this run has a failed checkpoint somewhere in its lifetime, and did
   not go on to complete one during or after the grace" — not strictly "a
   checkpoint failed during the grace window itself". A checkpoint that
   failed long before the grace even started, with no completed checkpoint
   after it, still reads as ``failed``. A completed checkpoint at or after the
   grace start always wins and reports ``completed``, regardless of earlier
   failures.

Every event a step's checkpoint listener received is either applied, dropped,
or rejected as invalid, and every one of the three is counted. Together, the
events a run's records show as applied plus these two metrics are the
complete count of everything a listener saw — the only events neither metric
can see are ones a client could not deliver at all (the listener gone or
hung, so the client's own bounded timeout fired without ever connecting).

``spur_checkpoint_events_dropped_total{reason}``
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

A well-formed event that could not be delivered or attributed to the step it
claimed, by reason.

.. list-table::
   :header-rows: 1
   :widths: 24 76

   * - ``reason``
     - Meaning
   * - ``not_job_process``
     - The sender's cgroup does not match the step this listener serves — a
       process outside the job tried to send an event on this socket.
   * - ``uid_mismatch``
     - The connecting process's uid is not the job's uid.
   * - ``read_timeout``
     - The sender connected but did not finish sending a line within the
       listener's read deadline (100 ms).
   * - ``rate_limited``
     - The step's event rate exceeded its token-bucket limit (burst of 64,
       refilling at 16 events/second) and the event was dropped rather than
       queued.
   * - ``busy``
     - The listener already had its maximum of 128 connections in flight; the
       new connection was closed unread without a reply.

.. _checkpoint-metrics-job-run-checkpoints-failed:

``spur_job_run_checkpoints_failed_total{reason}``
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

Checkpoint events rejected as invalid, by cause — the event's own ``job_id``
mismatched the step it reached, or the line itself was not valid v1 JSON for
this schema. **Not the same thing as a checkpoint whose save itself reported**
``status="failed"``: that is a well-formed, correctly-attributed event, and
it counts as applied — it shows up in the ``checkpoint`` label on
:ref:`spur_job_runs_ended_total <checkpoint-metrics-job-runs-ended>`, never
here.

.. list-table::
   :header-rows: 1
   :widths: 24 76

   * - ``reason``
     - Meaning
   * - ``job_mismatch``
     - The event's own ``job_id`` does not match the step the socket belongs
       to, or it sent a ``step_id`` (optional — most senders omit it) that
       does not match this step.
   * - ``unknown_kind``
     - ``kind`` is not ``checkpoint-begin``, ``checkpoint-end``, or
       ``restore``.
   * - ``unsupported_version``
     - ``v`` is not ``1``.
   * - ``invalid_status``
     - ``status`` is present on a ``checkpoint-begin``/``restore`` event,
       missing on a ``checkpoint-end`` event, or is neither ``ok`` nor
       ``failed``.
   * - ``unknown_field``
     - The line carries a field outside the fixed v1 vocabulary.
   * - ``invalid_field``
     - A field has the wrong JSON type, a required field is missing, or
       ``job_id``/``step_id`` is not numeric.
   * - ``malformed_json``
     - The line is not a JSON object at all, or fails to parse as JSON
       (truncated, trailing data, syntax error).
   * - ``oversize``
     - The line is longer than 512 bytes (excluding the trailing newline).

Per-job identity labels
------------------------

.. _checkpoint-metrics-per-job-labels:

Every per-job checkpoint series — the gauges below and the invalid-event counter further down —
shares the same identity label set: ``job_id`` (the job itself), ``user`` and ``account`` (who it
ran as), and ``job_name`` (the submitter-chosen name, for finding a job on a dashboard without
knowing its numeric ID). Nothing else is a label: an account's Kubernetes quota namespace is
redundant with ``account`` (it is derived from it), and ``partition``/``qos`` describe where a job
ran, not what it is. ``job_name`` is sanitized before use as a label: any control character
becomes ``_``, and the value is truncated to 64 characters (on a character boundary, never
mid-character). The OpenMetrics encoder separately escapes a quote or backslash that survives
sanitization.

Per-job gauges — ``/metrics/jobs-users-accts``
------------------------------------------------

.. _checkpoint-metrics-high-cardinality:

Built fresh on every scrape from jobs with a live (not yet finalized) run,
gated by ``metrics.high_cardinality = true`` like the rest of that endpoint
(see :doc:`karma-scoring`). Labels: the
:ref:`per-job identity set <checkpoint-metrics-per-job-labels>` above.

.. list-table::
   :header-rows: 1
   :widths: 44 56

   * - Metric
     - Description
   * - ``spur_job_checkpoints_completed``
     - Checkpoints completed so far by the job's live run.
   * - ``spur_job_checkpoint_last_step``
     - Training step of the job's most recently completed checkpoint. Sample
       omitted until the job completes its first checkpoint.
   * - ``spur_job_checkpoint_seconds_since_last``
     - Seconds since the job's most recently completed checkpoint. Sample
       omitted until the job completes its first checkpoint.
   * - ``spur_job_checkpoint_in_progress``
     - ``1`` if a checkpoint is in progress past the last completed step,
       else ``0``. Always present.
   * - ``spur_job_checkpoint_events_dropped``
     - Checkpoint events dropped so far for the job's live run, summed across
       every dropped-family reason. Always present (``0`` if nothing has been
       dropped).

Per-job invalid-event counter — ``/metrics/jobs-users-accts``
-----------------------------------------------------------------

``spur_job_checkpoints_failed_total{job_id, user, account, job_name, reason}`` is gated the same
way — ``metrics.high_cardinality = true`` — and keeps one series per reason instead of folding
every reason into one number. Labels are the same
:ref:`per-job identity set <checkpoint-metrics-per-job-labels>` as the gauges above, plus
``reason``: one of the invalid causes listed under
:ref:`spur_job_run_checkpoints_failed_total <checkpoint-metrics-job-run-checkpoints-failed>`
above. Only ``(job, reason)`` pairs with a nonzero count ever appear — no zero-filled series.

Values are the cumulative invalid-event count, by reason, **across every run attempt the job has
had** — monotonic per job identity, so a requeue adds to the count instead of resetting it:

- A job with a live (not yet finalized) run reports the cumulative count, live, including
  whatever earlier, already-finalized run attempts of the same job contributed.
- A job with no live run reports the same cumulative count until 1 hour after its last run
  ended — long enough for a dashboard's refresh interval or an alert's evaluation window to still
  see it after the job itself is gone. The controller retains this in a small bounded map (capped
  at 10,000 jobs, oldest evicted first).
- A job is never reported twice: a live run supersedes a retained entry for the same job, even
  one with no invalid events of its own right now — the cumulative total still carries forward
  what an earlier run attempt added.

Finalization and its limits
----------------------------

A run's records accumulate as its nodes report them. The run's final metrics
(``spur_job_runs_ended_total``, the two histograms, and
``spur_preempt_grace_checkpoint_total``) are recorded once, always 60 seconds
after the run ended — whatever records have arrived by then are recorded as
final, even if some nodes never reported, and a late record that arrives
within that window still counts. A run is never finalized before the 60
seconds pass, even if every node already sent a final record — unless the
run never reported a single checkpoint event, in which case it is dropped
without recording anything into these families at all (see
:ref:`spur_job_runs_ended_total <checkpoint-metrics-job-runs-ended>` above).

**Leader-change undercount.** This per-run state lives only on the current
Raft leader's process memory — it is not replicated and not rebuilt when
leadership changes. A run's final outcome is only recorded by whichever
controller is leader at the moment it finalizes; if leadership changes at
that same moment, or the run finalizes while every controller that ever held
its records has since lost leadership, that run's outcome can be missed
entirely and never appear in ``spur_job_runs_ended_total``. This is an
undercount, never a double-count: a run is recorded at most once. A periodic
sweep bounds how much a leadership change can leave behind by dropping any
not-yet-finalized run absent from the current leader's own live-job set, so
stale entries do not accumulate forever.

Cardinality
-----------

``/metrics/jobs`` — always on
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

Every family here is labeled only by a small, fixed enum (``reason``,
``checkpoint``, ``outcome``) — never by job, user, or account — so its
worst-case series count is fixed, regardless of how many jobs the cluster
has ever run. There are 9 possible ``reason`` values (the finalized
``JobState`` values that can end a run: ``completed``, ``failed``,
``cancelled``, ``timeout``, ``node_fail``, ``deadline``, ``out_of_memory``,
``preempted``, ``requeued``), 3 ``checkpoint`` values, 3 preempt-grace
``outcome`` values, 5 dropped-family reasons, and 8 invalid-family reasons.

.. list-table::
   :header-rows: 1
   :widths: 54 26 20

   * - Metric
     - Label combinations
     - Worst case
   * - ``spur_job_runs_ended_total``
     - 9 reasons × 3 checkpoint states
     - 27
   * - ``spur_job_runs_restored_total``
     - 9 reasons
     - 9
   * - ``spur_job_run_end_seconds_since_last_checkpoint``
     - 9 reasons × (12 buckets + ``+Inf`` + ``_sum`` + ``_count``)
     - 135
   * - ``spur_job_run_checkpoints_completed``
     - 9 reasons × (12 buckets + ``+Inf`` + ``_sum`` + ``_count``)
     - 135
   * - ``spur_preempt_grace_checkpoint_total``
     - 3 outcomes
     - 3
   * - ``spur_checkpoint_events_dropped_total``
     - 5 dropped-family reasons
     - 5
   * - ``spur_job_run_checkpoints_failed_total``
     - 8 invalid-family reasons
     - 8

322 series at the absolute most, total, no matter how long the cluster runs
or how many jobs it has scheduled.

``/metrics/jobs-users-accts`` — opt-in, high cardinality
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

Gated by ``metrics.high_cardinality = true``. Unlike the families above,
this endpoint's series count scales with job activity, because every series
here also carries the per-job identity labels below.

- The 5 per-job gauges (``spur_job_checkpoints_completed``,
  ``spur_job_checkpoint_last_step``, ``spur_job_checkpoint_seconds_since_last``,
  ``spur_job_checkpoint_in_progress``, ``spur_job_checkpoint_events_dropped``)
  appear only for a job whose current run has not yet ended. The moment the
  controller finalizes that run — even before the 60-second finalize grace
  elapses — its gauges disappear from this endpoint; they do not linger
  through the grace window.
- ``spur_job_checkpoints_failed_total`` adds up to 8 series — one per
  invalid reason a job has actually sent — per job that has ever sent an
  invalid event. Unlike the gauges, these keep reporting for 1 hour after
  the job's last run finalizes, with no live run needed, so a dashboard or
  alert still sees them for a while after the job itself is gone. The
  controller retains this bookkeeping for at most 10,000 jobs at a time,
  evicting the one with the oldest finalize time first once a new job would
  push it over that cap.

Labels
~~~~~~

Every series on this endpoint carries the
:ref:`per-job identity label set <checkpoint-metrics-per-job-labels>` described above — see that
section for what each label is and why it exists. One of those, ``job_name``, is free text the
job submitter chooses: it is sanitized and capped at 64 characters before use as a label, but
every distinct name (after sanitizing) is still its own series — two jobs with different names
are never folded into one.

Guidance
~~~~~~~~

Leave ``metrics.high_cardinality`` off unless something actually needs a per-job view — a
dashboard or alert scoped to one job, user, or account. The aggregate ``/metrics/jobs`` families
above already cover fleet-wide checkpoint health (completion rate, lost work, drop and
invalid-event rates) at fixed cost. When you do turn it on, size scrape storage for the cluster's
peak concurrent job count, not its lifetime job count: the gauges track only currently-live runs,
and even the longer-retained invalid-event counter is capped at 10,000 jobs.

Example PromQL
--------------

Fraction of preempted runs that still had a checkpoint in progress when they
ended (a signal that ``GraceTime`` is too short for this workload):

.. code-block:: promql

   sum(rate(spur_job_runs_ended_total{reason="preempted", checkpoint="in_progress"}[1h]))
   /
   sum(rate(spur_job_runs_ended_total{reason="preempted"}[1h]))

p90 lost work (seconds of compute since the last completed checkpoint) across
preempted runs:

.. code-block:: promql

   histogram_quantile(0.9,
     sum(rate(spur_job_run_end_seconds_since_last_checkpoint_bucket{reason="preempted"}[1h])) by (le)
   )

Dropped-event rate by reason, to catch a job whose checkpoint cadence is
tripping the per-step rate limit:

.. code-block:: promql

   sum by (reason) (rate(spur_checkpoint_events_dropped_total[5m]))

Invalid-event rate by reason, to catch a client sending the wrong event
shape (for example, an integration bug that regressed the wire format):

.. code-block:: promql

   sum by (reason) (rate(spur_job_run_checkpoints_failed_total[5m]))

Restore rate relative to run completions, as a rough sense of how often runs
resume from an existing checkpoint rather than starting fresh:

.. code-block:: promql

   sum(rate(spur_job_runs_restored_total[1h]))
   /
   sum(rate(spur_job_runs_ended_total[1h]))

Top 10 jobs by invalid checkpoint events in the last hour, to find a specific
job or user sending malformed events:

.. code-block:: promql

   topk(10, sum by (job_name, user, account) (increase(spur_job_checkpoints_failed_total[1h])))

.. _checkpoint-telemetry-socket-dir:

The checkpoint socket directory
--------------------------------

No ``spur.conf`` field controls this feature — there is nothing to configure.
``spurd`` hardens its socket directory at startup, scoped by the cluster's
``cluster_name``:

.. code-block:: text

   /run/spur/<cluster_name>/checkpoint/

- The ``checkpoint`` directory itself is owned by the user ``spurd`` runs as
  (root in a standard install), mode ``0711`` (traversal only — its contents
  cannot be listed, since socket names leak job and step IDs). ``spurd``
  creates it if missing and resets its mode if it already exists but is
  loose.
- Each step's socket inside it, ``<job_id>.<run_attempt>.<step_id>.sock``, is
  owned by the job's own uid, mode ``0600``, created by that step's
  ``spurstepd`` just before the step launches and removed when the step ends.
- If any directory segment on the path turns out to be a symlink, is not
  owned by ``spurd``'s own user, or — for ``/run/spur`` and the
  ``<cluster_name>`` directory above ``checkpoint`` — is group- or
  world-writable (the sticky bit does not make this acceptable), ``spurd``
  logs a warning and disables checkpoint telemetry for that node only — it
  never refuses to start the daemon over this.
- A step's session directory (which holds its ``checkpoint.json`` record) is
  kept past the step's own completion until the controller has acknowledged
  the step's final checkpoint record, bounded by 24 hours — after that, an
  abandoned session is pruned regardless, and its unacknowledged record is
  lost.

See :doc:`/user-guide/checkpoint-telemetry` for the job-facing side of this
same socket.
