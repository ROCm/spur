Slurm Compatibility
===================

Spur is drop-in compatible with Slurm's command-line interface, so existing job
scripts, submission wrappers, and monitoring tools work without modification. This
page explains how the client dispatches Slurm commands and lists the native Spur
verb for each one.

How Dispatch Works
------------------

``spur`` is a single multi-call binary. It decides which subcommand to run from
``argv[0]`` — the name it was invoked as. Run it as ``spur <command>`` and it
rewrites the arguments so the subcommand parser sees its canonical name; invoke it
through a symlink named ``sbatch`` and it runs the ``sbatch`` command directly.

The installer creates the Slurm-named symlinks. To set them up by hand, link each
Slurm command name to ``spur``:

.. code-block:: bash

   cd $(dirname $(which spur))
   for cmd in sbatch srun squeue scancel sinfo sacct scontrol; do
       ln -sf spur $cmd
   done

Once the symlinks exist, ``sbatch job.sh`` and ``spur submit job.sh`` are
equivalent. The native ``spur`` verbs are convenience aliases; the Slurm names
work directly.

Command Map
-----------

.. list-table::
   :header-rows: 1
   :widths: 22 18 60

   * - Native Spur verb
     - Slurm command
     - Purpose
   * - ``spur submit``
     - ``sbatch``
     - Submit a batch job script
   * - ``spur run``
     - ``srun``
     - Run a command or launch a job step
   * - ``spur alloc``
     - ``salloc``
     - Allocate an interactive session
   * - ``spur queue``
     - ``squeue``
     - View the job queue
   * - ``spur cancel``
     - ``scancel``
     - Cancel jobs
   * - ``spur nodes``
     - ``sinfo``
     - Show cluster and node information
   * - ``spur history``
     - ``sacct``
     - Show accounting history
   * - ``spur accounts``
     - ``sacctmgr``
     - Manage accounts, users, and QOS
   * - ``spur show``
     - ``scontrol``
     - Show and update jobs, nodes, and partitions
   * - ``spur priority``
     - ``sprio``
     - Show the priority breakdown for pending jobs
   * - ``spur share``
     - ``sshare``
     - Show fair-share information
   * - ``spur stat``
     - ``sstat``
     - Show statistics for running jobs
   * - ``spur diag``
     - ``sdiag``
     - Show scheduler diagnostics
   * - ``spur report``
     - ``sreport``
     - Generate usage reports
   * - ``spur attach``
     - ``sattach``
     - Attach to a running job's I/O
   * - ``spur crontab``
     - ``scrontab``
     - Manage recurring cron-style jobs
   * - ``spur health``
     - ``smd``
     - Monitor node health

.. note::

   Slurm command names work directly once the symlinks are in place. The native
   ``spur`` verbs are aliases for the same commands and are interchangeable with
   them.

Behavioral Differences
----------------------

The commands accept Slurm syntax, but a few flags and behaviors differ or are not
yet functional. See :doc:`/migration-from-slurm` for the full list of behavioral
differences and current limitations.

See Also
--------

- :doc:`/migration-from-slurm`
- :doc:`submitting-jobs`
