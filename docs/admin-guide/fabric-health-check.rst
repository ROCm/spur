RDMA Fabric Health Check
========================

``spur-fabric-check`` is a small program meant to be named as the ``program`` of
a ``[[health.checks]]`` entry (see ``examples/spur.conf``). It inspects the
node's RDMA devices through sysfs and exits non-zero when the fabric looks
degraded, which makes the health framework drain the node with the check's
output as the reason. It is vendor-neutral: it reads the generic
``/sys/class/infiniband`` interface, so it covers ``ionic``, ``bnxt_re``, and
``mlx5`` devices alike.

The program is built with the workspace (``cargo build --release -p
spur-fabric-check``) and must be copied to the same absolute path on every node.

Configuration
-------------

.. code-block:: toml

   [[health.checks]]
   program = "/usr/local/bin/spur-fabric-check"
   nodes = 1
   interval_secs = 900
   timeout_secs = 60

What it checks
--------------

.. list-table::
   :header-rows: 1
   :widths: 20 80

   * - Check
     - Passes when
   * - ``rdma_devices``
     - The number of RDMA devices equals ``--expected-nics``, or, when that is
       not given, the number of GPUs in the KFD topology. If neither is known
       the check only reports the count.
   * - ``port_state``
     - Every port of every device is ``ACTIVE``.
   * - ``link_rate``
     - All ``ACTIVE`` ports report the same link rate.
   * - ``netdev_mtu``
     - The netdevs behind all ``ACTIVE`` ports have the same MTU.

Each check prints one ``PASS``, ``FAIL``, or ``SKIP`` line. ``SKIP`` means the
value could not be read (for example, a provider that does not expose a rate)
and never fails the node. On a host without ``/sys/class/infiniband`` the whole
program reports ``SKIP`` and exits 0, so it is safe to run on mixed fleets.

Exit status is ``0`` when nothing failed and ``1`` otherwise.

Options
-------

``--expected-nics N``
   Require exactly ``N`` RDMA devices instead of matching the GPU count.

``--no-gpu-match``
   Do not compare the device count with the GPU count. Use this on nodes whose
   NIC-to-GPU ratio is not one to one.

``-- COMMAND [ARGS...]``
   Enable the transmit probe (below). Everything after ``--`` is the traffic
   command.

Transmit probe
--------------

A NIC can report ``ACTIVE`` while its control path is dead and it sends nothing.
The probe catches this: for each ``ACTIVE`` port it reads the transmit counters,
runs the command you supply, and reads them again. A port whose counters did not
advance is reported as failed, as is a traffic command that exits non-zero.

An idle node sends no traffic by itself, so the command is what exercises the
port. ``{dev}`` and ``{port}`` in the arguments are replaced with the device
name and port number.

The health framework runs ``program`` without arguments, so to pass flags put
the invocation in a small wrapper script and name that script as the
``program``:

.. code-block:: bash

   #!/bin/sh
   exec /usr/local/bin/spur-fabric-check --expected-nics 8 -- \
       /usr/local/bin/send-test-traffic --device {dev} --port {port}

Counters are read from ``counters/port_xmit_data`` and
``counters/port_xmit_packets``, falling back to the provider's ``hw_counters``
(``tx_bytes``, ``tx_pkts``, ``tx_roce_pkts``). Ports that expose none of these
are skipped.

Limitations
-----------

This is a single-node check. Verifying that nodes can actually reach each other
(pairwise bandwidth, collective tests) needs multi-node health checks, which the
framework does not support yet.
