Node Health Checks & Auto-Recovery
===================================

Spur can automatically detect unhealthy nodes and, optionally, recover them
without admin intervention.

Health Checks
-------------

The controller submits each configured check as an exclusive whole-node job,
so it runs only when the node is idle. A non-zero exit (or timeout) drains
the node with the check output as the reason — a sticky drain the admin
clears with ``scontrol update NodeName=<n> State=IDLE``. Checks are visible
in ``squeue`` (name prefix ``_spur-health.``).

Configuration (``spur.conf``):

.. code-block:: toml

   [health]
   max_unavailable = "10%"     # concurrent cap; "5" or "10%"

   [[health.checks]]
   program = "/etc/spur/node-health.sh"
   interval_secs = 300
   timeout_secs = 60
   max_wait_secs = 600         # force-drain if check starves this long (0 = wait)
   user = "nobody"
   uid = 65534
   gid = 65534

No ``[[health.checks]]`` entries = feature disabled.

Auto-Recovery
-------------

When a node enters a system-initiated drain or down state (health-check failure,
epilog failure, heartbeat loss), the controller dispatches an admin-configured
recovery script. The script owns its phase state via ``spur.recovery/*`` labels
and the return-to-service decision.

Configuration (``spur.conf``):

.. code-block:: toml

   [recovery]
   program = "/etc/spur/recover.sh"
   trigger_on = ["drain", "down"]
   max_attempts = 3
   timeout_secs = 600
   user = "root"
   reboot_timeout_secs = 900
   concurrency_cap = "10%"

No ``program`` = feature disabled. Admin drains are never auto-recovered.

Script Contract
~~~~~~~~~~~~~~~

The recovery script receives these environment variables:

.. list-table::
   :header-rows: 1

   * - Variable
     - Description
   * - ``SPUR_NODE_NAME``
     - Node being recovered
   * - ``SPUR_DRAIN_REASON``
     - Original drain reason
   * - ``SPUR_RECOVERY_ATTEMPT``
     - Current dispatch number (1-based)
   * - ``SPUR_RECOVERY_MAX_ATTEMPTS``
     - Configured cap
   * - ``SPUR_RECOVERY_PHASE``
     - Value of ``spur.recovery/phase`` label (empty on first dispatch)

Exit codes:

.. list-table::
   :header-rows: 1

   * - Code
     - Meaning
   * - ``0``
     - Phase complete. Controller re-evaluates; redispatches if ``spur.recovery/*`` labels remain.
   * - ``42``
     - Permanent failure. Controller stops immediately (exhausts).
   * - Any other nonzero
     - Phase failed. Attempt incremented; retries if under cap.

The script uses ``spur node label --self`` to manage its phase state and
``spur node undrain --self`` to return the node to service:

.. code-block:: bash

   #!/bin/bash
   set -euo pipefail
   case "$SPUR_RECOVERY_PHASE" in
     "")
       /opt/amd/install-bkc.sh --version 6.3.2
       spur node label --self "$SPUR_NODE_NAME" spur.recovery/phase=verify
       reboot
       ;;
     verify)
       rocm-smi --showhw > /dev/null 2>&1 || exit 1
       spur node label --self "$SPUR_NODE_NAME" "spur.recovery/phase-"
       spur node undrain --self "$SPUR_NODE_NAME"
       ;;
   esac

Reboot and Reprovision
~~~~~~~~~~~~~~~~~~~~~~

The script can call ``reboot`` or trigger PXE reprovision after setting a
recovery label. The label is WAL-committed before the node disappears, so
the controller holds it across the outage and redispatches when the node
re-registers. If the node does not re-register within ``reboot_timeout_secs``,
the dispatch counts as a failed attempt.

Admin Commands
~~~~~~~~~~~~~~

``spur node recovery trigger <node>``
   Hand a drained node to the recovery script. Reclassifies the drain as
   system-initiated, resets recovery state, and lets the reconciler dispatch.

``spur node recovery abort <node>``
   Stop auto-recovery. Cancels any in-flight recovery job, reclassifies the
   drain as admin-initiated, and clears all ``spur.recovery/*`` labels.

``spur node undrain <node>``
   Clear a drain hold and return the node to idle.

Interaction with Health Checks
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

Health checks probe only schedulable nodes. A recovery-drained node is never
probed while recovery is in flight. After the script returns the node to idle,
health checks resume on the next interval. Recovery jobs skip the epilog.
