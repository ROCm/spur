AMD Device Metrics Exporter Integration
========================================

The `AMD Device Metrics Exporter
<https://github.com/ROCm/device-metrics-exporter>`_ (``dme``) exports GPU
telemetry — utilization, memory, temperature, power — as Prometheus metrics.
When it runs alongside Spur on a compute node, it can also attach per-job
labels (``job_id``, ``job_user``, ``job_partition``) to every GPU metric, so a
``gpu_gfx_activity`` spike in Prometheus can be traced back to the Spur job
that caused it.

This integration needs no changes to the exporter's own configuration —
``job_id``/``job_user``/``job_partition`` are exporter-side **mandatory
labels**, enabled out of the box. All of the work is on the Spur side: prolog
and epilog hooks that tell the exporter which job owns which GPU, for as long
as the job runs.

How it works
------------

The exporter watches a directory (default ``/var/run/exporter``) for files
named after the render ID of a GPU (``0``, ``1``, ...). Each file is a small
JSON document describing the job currently using that GPU:

.. list-table::
   :header-rows: 1
   :widths: 30 70

   * - JSON key
     - Used for
   * - ``SLURM_JOB_ID``
     - the ``job_id`` metric label
   * - ``SLURM_JOB_USER``
     - the ``job_user`` metric label
   * - ``SLURM_JOB_PARTITION``
     - the ``job_partition`` metric label
   * - ``CUDA_VISIBLE_DEVICES``
     - which GPU render ID(s) the file's content applies to

A Spur ``prolog`` hook writes this file when a job starts on a GPU; the
matching ``epilog`` hook deletes it when the job ends. Spur's hook
environment already twins ``SPUR_JOB_ID``/``SPUR_JOB_PARTITION`` into
``SLURM_JOB_ID``/``SLURM_JOB_PARTITION`` automatically, so the scripts below
can use those directly. ``SLURM_JOB_USER`` and ``CUDA_VISIBLE_DEVICES`` have
no such twin — they're built from ``SPUR_JOB_USER`` and ``SPUR_JOB_GPUS``
(see :ref:`hooks-config` below).

.. important::

   Do not rename the JSON keys to ``SPUR_*`` when adapting these scripts. The
   exporter's parser looks for the literal strings ``SLURM_JOB_ID``,
   ``SLURM_JOB_USER``, ``SLURM_JOB_PARTITION``, and ``CUDA_VISIBLE_DEVICES`` —
   a file using any other key names is parsed successfully but the labels are
   left empty, with no error logged.

.. _hooks-config:

Prolog/epilog configuration
----------------------------

Add a ``[hooks]`` block to every agent's ``spur.conf`` pointing at the two
scripts:

.. code-block:: toml

   [hooks]
   prolog = "/usr/share/exporter/slurm-prolog.sh"
   epilog = "/usr/share/exporter/slurm-epilog.sh"

See :doc:`configuration` for the full ``[hooks]`` reference — these are the
node-level ``prolog``/``epilog`` fields (Slurm's ``Prolog``/``Epilog``), not
the controller-side ``prolog_slurmctld``/``epilog_slurmctld``. ``spur.conf``
must exist at the path ``spurd`` loads (``/etc/spur/spur.conf`` by default,
or wherever ``-f`` points) and contain this ``[hooks]`` block.

The scripts must be **fully-qualified paths** (Spur does not search ``$PATH``
for hook scripts) and **executable**:

.. code-block:: bash

   sudo chmod 0755 /usr/share/exporter/slurm-prolog.sh /usr/share/exporter/slurm-epilog.sh

Restart ``spurd`` after editing ``spur.conf`` — hooks are only read at agent
startup:

.. code-block:: bash

   sudo systemctl restart spurd

Example scripts
----------------

Adjust ``EXPORT_DIR`` only if the exporter container mounts a different host
path.

``slurm-prolog.sh``
~~~~~~~~~~~~~~~~~~~~

.. code-block:: bash

   #!/bin/bash
   #
   # Copyright (c) Advanced Micro Devices, Inc. All rights reserved.
   # SPDX-License-Identifier: Apache-2.0
   #

   EXPORT_DIR="/var/run/exporter"

   # AMD GPU render-node minor numbers start at 128, while SPUR_JOB_GPUS
   # reports the 0-based logical device index Spur schedules against.
   IFS=',' read -ra GPU_IDS <<< "${SPUR_JOB_GPUS}"
   RENDER_IDS=()
   for gpu in "${GPU_IDS[@]}"; do
       RENDER_IDS+=("$((gpu % 128))")
   done
   CUDA_VISIBLE_DEVICES=$(IFS=','; echo "${RENDER_IDS[*]}")

   [ -d "${EXPORT_DIR}" ] || exit 0
   for id in "${RENDER_IDS[@]}"; do
       printf '{"SLURM_JOB_ID":"%s","SLURM_JOB_USER":"%s","SLURM_JOB_PARTITION":"%s","CUDA_VISIBLE_DEVICES":"%s"}' \
           "${SLURM_JOB_ID}" "${SPUR_JOB_USER}" "${SLURM_JOB_PARTITION}" "${CUDA_VISIBLE_DEVICES}" \
           > "${EXPORT_DIR}/${id}"
   done

``slurm-epilog.sh``
~~~~~~~~~~~~~~~~~~~~

Identical GPU ID derivation as the prolog; removes the tracking file instead
of writing it:

.. code-block:: bash

   #!/bin/bash
   #
   # Copyright (c) Advanced Micro Devices, Inc. All rights reserved.
   # SPDX-License-Identifier: Apache-2.0
   #

   EXPORT_DIR="/var/run/exporter"

   IFS=',' read -ra GPU_IDS <<< "${SPUR_JOB_GPUS}"
   [ -d "${EXPORT_DIR}" ] || exit 0
   for gpu in "${GPU_IDS[@]}"; do
       rm -f "${EXPORT_DIR}/$((gpu % 128))"
   done

Running the exporter container
--------------------------------

Bind-mount the same directory the prolog/epilog scripts write to into the
``dme`` container at the identical path, plus the GPU device nodes:

.. code-block:: bash

   sudo docker run -d --name dme \
     --device=/dev/kfd --device=/dev/dri \
     -v /var/run/exporter:/var/run/exporter \
     -p 5000:5000 \
     rocm/device-metrics-exporter:v1.5.1

No ``config.json`` or extra environment variables are required for the
``job_id``/``job_user``/``job_partition`` labels specifically — they are part
of the exporter's default mandatory label set.

Verifying the integration
---------------------------

Submit a GPU job and, while it is running, check both the tracking file and
the live metric:

.. code-block:: bash

   sbatch --partition=gpu --gpus=1 --wrap="sleep 30"

   # while the job is RUNNING:
   cat /var/run/exporter/0
   docker exec dme curl -s localhost:5000/metrics | grep gfx_activity

Expected file content (values will match your job):

.. code-block:: text

   {"SLURM_JOB_ID":"8","SLURM_JOB_USER":"user","SLURM_JOB_PARTITION":"gpu","CUDA_VISIBLE_DEVICES":"0"}

Expected metric line, with the job's real ID/user/partition populated:

.. code-block:: text

   gpu_gfx_activity{...,job_id="8",job_partition="gpu",job_user="user",...} 0

After the job completes, confirm cleanup — the tracking file should be gone
and the labels should reset to empty:

.. code-block:: bash

   ls /var/run/exporter/
   docker exec dme curl -s localhost:5000/metrics | grep gfx_activity
   # gpu_gfx_activity{...,job_id="",job_partition="",job_user="",...} 0
