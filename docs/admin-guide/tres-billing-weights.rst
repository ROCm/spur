TRES Billing Weights
====================

By default, fairshare bills CPU-seconds and GPU-seconds equally — a user
holding 8 GPUs for an hour and a user running 128 CPUs for an hour accumulate
nearly the same usage. On GPU clusters, this undercharges GPU consumers and
undermines fair scheduling.

``tres_billing_weights`` lets you set the relative cost of each resource type.
A weight of ``GRES/gpu=16.0`` means 1 GPU-second costs 16× a CPU-second in
the fairshare calculation.

Configuration
-------------

.. code-block:: toml

   [scheduler]
   tres_billing_weights = "CPU=1.0,GRES/gpu=16.0"

``tres_billing_weights``
   Slurm-compatible, comma-separated ``KEY=VALUE`` pairs. Case-insensitive.
   Negative values are clamped to 0. Unknown keys are silently ignored.

   When empty or omitted, defaults to ``CPU=1.0,GRES/gpu=1.0`` — identical to
   the behavior before this feature existed.

The billing formula is:

.. code-block:: text

   billable = cpu_seconds × weight("CPU")
            + gpu_seconds × weight("GRES/gpu")

This scalar feeds ``usage_raw``, which drives fairshare. The per-TRES raw
counters (``CPURawUsage``, ``GPURawUsage`` in ``sshare -l``) are tracked
separately and unaffected by weights.

Billing is computed at read time, not stored. Changing the weights
retroactively adjusts fairshare for all historical usage — no migration or
backfill required.

.. note::

   **Memory billing** (``Mem=`` weight) is not yet tracked. The ``Mem=`` key
   is accepted in the config string but has no effect until memory billing is
   enabled in a future release. When it lands, jobs that do not specify
   ``--mem`` will use the partition's ``DefMemPerCPU`` / ``DefMemPerGPU``
   defaults.

Example
~~~~~~~

With ``tres_billing_weights = "CPU=1.0,GRES/gpu=16.0"``:

.. list-table::
   :header-rows: 1

   * - Job
     - CPUs
     - GPUs
     - Wall time
     - Billable (default 1:1)
     - Billable (weighted)
   * - A
     - 32
     - 0
     - 1 hour
     - 115,200
     - 115,200
   * - B
     - 32
     - 8
     - 1 hour
     - 144,000
     - 576,000

Without weights, Job B bills at 1.25× Job A. With ``GRES/gpu=16.0``, Job B
bills at 5× — reflecting GPU cost.

Usage reset
-----------

Two mechanisms for resetting accumulated usage:

Periodic reset
~~~~~~~~~~~~~~

.. code-block:: toml

   [scheduler]
   usage_reset_period = "monthly"

Resets all users and accounts cluster-wide at calendar boundaries (UTC
midnight). Valid values: ``none`` (default), ``daily``, ``weekly``,
``monthly``, ``quarterly``, ``yearly``.

The next scheduled reset is logged at startup and visible via
``scontrol show config``.

Manual reset
~~~~~~~~~~~~

.. code-block:: bash

   # Reset a user's usage to zero
   sacctmgr modify user name=alice account=research set rawusage=0

   # Set to an arbitrary value
   sacctmgr modify user name=alice account=research set rawusage=50000

   # Subtract usage (billing correction, e.g. refund lost node-hours)
   sacctmgr modify user name=alice account=research set rawusage=-10000

   # Reset an entire account and its sub-accounts
   sacctmgr modify account name=research set rawusage=0

Negative values subtract from current usage, clamped at zero. All
modifications are audit-logged with actor, timestamp, and previous value.

Verifying
---------

``scontrol show config`` displays the active weights and reset period:

.. code-block:: text

   TRESBillingWeights=CPU=1.0,GRES/gpu=16.0
   UsageResetPeriod=monthly

``sshare`` shows weighted usage by default. Use ``-l`` for the per-TRES
breakdown:

.. code-block:: bash

   sshare -l -A research

Divergences from Slurm
----------------------

This implementation is compatible with Slurm's ``TRESBillingWeights`` for the
most common use case (cluster-wide CPU+GPU weighting) but differs in several
ways:

- **Scope:** Spur's weights are cluster-wide. Slurm supports per-partition
  ``TRESBillingWeights`` and additional TRES types (``Mem``, ``Node``, plus
  ``MAX(TRES)`` billing).
- **Manual usage adjustment:** ``set rawusage=N`` accepts arbitrary positive
  values and negative values (subtract, clamped at 0). Slurm only permits
  ``rawusage=0`` (reset to zero) on a decayed value.
- **Reset period:** ``usage_reset_period`` supports the same calendar
  boundaries as Slurm's ``PriorityUsageResetPeriod`` but does not support
  Slurm's ``NOW`` value (immediate reset).
- **Memory billing:** not yet tracked. The ``Mem=`` key is accepted in the
  config but inert until memory billing is enabled in a future release.
