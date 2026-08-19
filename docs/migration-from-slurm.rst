Migrating from Slurm
====================

Spur is drop-in compatible with Slurm's command-line interface, REST API, and C
FFI. Most Slurm workloads move over unchanged. This page covers what works as-is,
where the configuration model differs, and the behavioral differences a Slurm user
should know about before switching.

What Works Unchanged
--------------------

The following work without modification:

- **Commands** — ``sbatch``, ``srun``, ``salloc``, ``squeue``, ``sinfo``,
  ``sacct``, ``scancel``, ``scontrol``, and ``sacctmgr`` (via the symlinks
  described in :doc:`/user-guide/slurm-compatibility`).
- **Job script directives** — ``#SBATCH`` directives in batch scripts are parsed
  the same way. ``#PBS`` directives are also converted on a best-effort basis.
- **Environment variables** — Spur sets a ``SLURM_*`` twin for every ``SPUR_*``
  variable it injects into a job, so Slurm-aware software (MPI launchers, training
  frameworks) sees the ``SLURM_*`` names it expects.
- **REST API and C FFI** — the REST surface and the FFI library remain
  Slurm-compatible.

Configuration Differences
-------------------------

Slurm's ``slurm.conf`` and ``slurmdbd.conf`` are replaced by a single TOML file,
``spur.conf`` (default location ``/etc/spur/spur.conf``). Key differences:

- **One config file, one set of daemons.** There is no ``slurmdbd`` and no
  ``slurmrestd`` — accounting and the REST API are served by the controller
  (``spurctld``). Accounting storage is configured under ``[accounting]`` in
  ``spur.conf`` rather than in a separate ``slurmdbd.conf``.
- **Raft-based state.** Controller state is replicated through Raft and persists
  across restarts, so there is no ``StateSaveLocation`` handling to manage
  separately.
- **Built-in high availability.** HA is provided by Raft (openraft) and is always
  on — even a single controller runs a one-member cluster. There is no separate
  backup-controller configuration; add controllers to the Raft cluster instead.

See :doc:`/admin-guide/configuration` for the full ``spur.conf`` reference.

Behavioral Differences and Current Limitations
----------------------------------------------

The commands below accept Slurm syntax, but the following differences apply. Flags
noted as "accepted for compatibility" parse without error but have no effect yet.

.. list-table::
   :header-rows: 1
   :widths: 30 70

   * - Area
     - Difference
   * - Partitions
     - Defined in ``spur.conf``; there is no runtime ``scontrol create/update/delete partition``. Edit the config and reload the controller to change a partition.
   * - ``squeue --sort``
     - Accepted for compatibility; result ordering is not yet applied.
   * - ``sinfo --states``
     - Accepted for compatibility; the state filter is not yet applied (all node states are returned).
   * - ``sacct --jobs``
     - Accepted for compatibility; the job-id filter is not yet applied server-side.
   * - ``sacct`` ``ReqMem``
     - The ``ReqMem`` format field has no value yet and renders empty.
   * - ``squeue`` format
     - The ``%b`` (GRES) and ``%L`` (time-left) format fields are not yet resolved.
   * - ``sstat``
     - Per-process ``Ave*`` and ``Max*`` metrics (for example ``AveRSS``, ``MaxVMSize``) show ``N/A``.
   * - ``sprio``
     - The ``FAIRSHARE`` column is a placeholder and does not yet reflect usage.
   * - ``scancel`` arrays
     - Array-element syntax (``123_4``) is not yet supported client-side; cancel by plain job id.
   * - ``scontrol show``
     - Output is always the multi-line ``Key=Value`` block; there is no ``--oneliner``.

Accounting and QOS Mapping
--------------------------

Accounting entities map directly from Slurm:

- ``sacctmgr add account``, ``sacctmgr add user``, and ``sacctmgr add qos`` work as
  in Slurm, following the same ``cluster → account → user → association → QOS``
  ordering.
- The ``[accounting] require_qos`` setting is the equivalent of Slurm's
  ``AccountingStorageEnforce=qos``.
- The ``default_qos`` setting is the equivalent of Slurm's fallback QOS.

See :doc:`/admin-guide/accounting` for the accounting concept guide.

.. tip::

   Before migrating a production workload, test the golden path on a small cluster
   first: submit a job (``sbatch``), check the queue (``squeue``), cancel a job
   (``scancel``), and review accounting history (``sacct``).

See Also
--------

- :doc:`/user-guide/slurm-compatibility`
- :doc:`/admin-guide/accounting`
- :doc:`/admin-guide/configuration`
