Reservations
============

A reservation sets nodes aside for a time window. Only the listed users and
accounts may run jobs on those nodes while it is active. Creating, updating,
and deleting reservations requires operator privilege (see :doc:`rbac`).

Creating a reservation
----------------------

Use the Slurm-style inline syntax:

.. code-block:: bash

   scontrol create ReservationName=maint StartTime=now Duration=1h \
       Nodes=gpu[01-04] Users=alice,bob Accounts=ml PartitionName=gpu

or the equivalent flag form:

.. code-block:: bash

   scontrol create-reservation --name=maint --start-time=now --duration=1h \
       --nodes=gpu[01-04] --users=alice,bob --accounts=ml --partition=gpu

Supported inline keys (case-insensitive, in any order):

.. list-table::
   :header-rows: 1
   :widths: 25 75

   * - Key
     - Meaning
   * - ``ReservationName``
     - Required. The reservation's name.
   * - ``StartTime``
     - ISO 8601 timestamp or ``now`` (the default).
   * - ``Duration``
     - Required. Whole minutes, Slurm time (``MM:SS``, ``H:MM:SS``,
       ``D-HH:MM:SS``), or suffixed (``90m``, ``1h30m``). ``UNLIMITED`` is not
       supported.
   * - ``Nodes``
     - Required. Comma-separated node names or hostlist expressions.
   * - ``Users``
     - Comma-separated users allowed to use the reservation.
   * - ``Accounts``
     - Comma-separated accounts allowed to use the reservation.
   * - ``Flags``
     - Comma-separated ``MAINT``, ``IGNORE_JOBS``, ``NO_HOLD_JOBS``,
       ``OVERLAP``.
   * - ``PartitionName``
     - Partition to bind the reservation to. See `Binding a partition`_.

Showing and deleting
--------------------

.. code-block:: bash

   scontrol show reservation [maint]
   scontrol delete ReservationName=maint     # or: scontrol delete-reservation maint

``scontrol show reservation`` prints ``PartitionName=`` only when the
reservation is bound to a partition.

Binding a partition
-------------------

``PartitionName=`` binds a reservation to a partition. A job that names the
reservation (``--reservation=maint``) but requests no partition runs in the
reservation's partition instead of the default one. An explicit ``-p`` always
wins.

This applies only when the reservation admits the submitter, either by user or
by the job's account (``--account`` or the user's default account). A job from
anyone else keeps the default partition and pends on the reservation like any
job it does not admit.

- The partition must exist when the reservation is created; an unknown name is
  rejected.
- ``Nodes=`` is still required. Spur does not pick nodes from the partition and
  does not check that the listed nodes belong to it.
- ``scontrol delete PartitionName=`` refuses a partition while a reservation is
  bound to it, as Slurm does. Delete the reservation first.
- When ``spur.conf`` drops the partition, the controller deletes the
  reservations bound to it, as Slurm purges reservations with an invalid
  partition. Reservations used by a running job are never deleted:

  - ``scontrol reconfigure`` then keeps the partition too, and retries on the
    next reconfigure.
  - After a restart that no longer loads the partition, such a reservation is
    deleted once its jobs end. Until then, jobs that name it without ``-p`` are
    rejected at submit with the usual unknown-partition error rather than
    falling back to the default partition.

Upgrade note
------------

``PartitionName=`` needs a controller that advertises the
``reservation-partition`` capability. Against an older controller the CLI
refuses the request rather than create a reservation without its partition.
During a rolling upgrade of an HA cluster, finish upgrading every controller
before creating partition-bound reservations: an old controller that applies
the create drops the partition. See :ref:`upgrading-reservation-partition`.

Limitations
-----------

- ``Nodes=ALL`` and ``NodeCnt=`` are not supported; list the nodes explicitly.
- A reservation's partition cannot be changed after creation; delete and
  recreate the reservation.
- Inline ``scontrol update ReservationName=...`` is not supported. Use
  ``scontrol update-reservation --name=<name>`` with ``--duration``,
  ``--add-nodes``/``--remove-nodes``, ``--add-users``/``--remove-users``, or
  ``--add-accounts``/``--remove-accounts``.
