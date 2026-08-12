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

The exporter watches a directory (default ``/var/run/exporter/``) for files
named after a GPU's render ID (``0``, ``1``, ...). Each file is a small JSON
document describing the job currently using that GPU. The exporter's watcher
is scheduler-agnostic in behavior but not in **field names** — it always reads
these five JSON keys, regardless of which scheduler wrote the file:

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
   * - ``SLURM_CLUSTER_NAME``
     - reserved; Spur's hook context does not populate a cluster-name value,
       so this is always empty under Spur
   * - ``CUDA_VISIBLE_DEVICES``
     - which GPU render ID(s) the file's content applies to

A Spur ``prolog`` hook writes this file when a job starts on a GPU; the
matching ``epilog`` hook deletes it when the job ends. Spur's own hook
environment does **not** set ``SLURM_JOB_ID`` or ``CUDA_VISIBLE_DEVICES`` — it
sets ``SPUR_JOB_ID``, ``SPUR_JOB_USER``, ``SPUR_JOB_PARTITION``, and
``SPUR_JOB_GPUS`` instead (see :ref:`hooks-config` below). The integration
scripts below read the ``SPUR_*`` values Spur actually provides, then write
them back out under the ``SLURM_*`` JSON keys the exporter actually parses.

.. important::

   Do not rename the JSON keys to ``SPUR_*`` when adapting these scripts. The
   exporter's parser looks for the literal strings ``SLURM_JOB_ID``,
   ``SLURM_JOB_USER``, ``SLURM_JOB_PARTITION``, ``SLURM_CLUSTER_NAME``, and
   ``CUDA_VISIBLE_DEVICES`` — a file using any other key names is parsed
   successfully but the labels are left empty, with no error logged.

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
the controller-side ``prolog_slurmctld``/``epilog_slurmctld``.

The scripts must be:

- **fully-qualified paths** — Spur does not search ``$PATH`` for hook scripts
- **executable** and owned appropriately for the agent's ``spurd`` process
  (``chmod 0755``, ``root:root`` is sufficient when ``spurd`` runs as root)

.. code-block:: bash

   sudo chmod 0755 /usr/share/exporter/slurm-prolog.sh /usr/share/exporter/slurm-epilog.sh

``spurd`` must be started with an explicit config path (``-f
/path/to/spur.conf``) for hooks to take effect — without ``-f`` it falls back
to the (usually absent) ``/etc/spur/spur.conf`` default and silently runs with
no hooks configured. Check ``journalctl -u spurd`` for a ``loaded spur.conf
path=...`` line to confirm the right file was picked up.

Restart ``spurd`` after editing ``spur.conf`` — hooks are only read at agent
startup:

.. code-block:: bash

   sudo systemctl restart spurd

Example scripts
----------------

These are the exact scripts validated end-to-end against a running Spur
cluster and the exporter's Prometheus output. Adjust ``EXPORT_DIR`` only if
the exporter container mounts a different host path.

``slurm-prolog.sh``
~~~~~~~~~~~~~~~~~~~~

.. code-block:: bash

   #!/bin/bash
   #
   # Copyright (c) Advanced Micro Devices, Inc. All rights reserved.
   # SPDX-License-Identifier: Apache-2.0
   #

   EXPORT_DIR="/var/run/exporter/"
   mod128_array() {
       local arr_str="$1"
       local arr result

       # convert string to array using comma as delimiter
       IFS=',' read -ra arr <<< "$arr_str"

       # modulo 128 to each element
       for i in "${!arr[@]}"; do
           arr[i]=$(( ${arr[i]} % 128 ))
       done

       # join array back into a comma-separated string
       result=$(IFS=','; echo "${arr[*]}")

       echo "$result"
   }
   AMDGPU_DEVICES=$(mod128_array "${CUDA_VISIBLE_DEVICES}")
   AMD_SPUR_GPUS=$(mod128_array "${SPUR_JOB_GPUS}")
   # CUDA_VISIBLE_DEVICES is empty on AMD hardware; fall back to SPUR_JOB_GPUS so
   # job_id/job_user labels are still attached to the GPU metrics.
   [ -z "${AMDGPU_DEVICES}" ] && AMDGPU_DEVICES="${AMD_SPUR_GPUS}"
   # The device-metrics-exporter's Slurm watcher (pkg/exporter/scheduler/slurm.go)
   # hardcodes these JSON key names (SLURM_*, CUDA_VISIBLE_DEVICES) regardless of
   # scheduler — values are sourced from Spur's SPUR_* env vars, but the keys must
   # stay SLURM_* or the exporter silently leaves job_id/job_user/job_partition empty.
   MSG=$(
       cat <<EOF
       {
       "SLURM_JOB_ID": "${SPUR_JOB_ID}",
       "SLURM_JOB_USER": "${SPUR_JOB_USER}",
       "SLURM_JOB_PARTITION": "${SPUR_JOB_PARTITION}",
       "SLURM_CLUSTER_NAME": "${SPUR_CLUSTER_NAME}",
       "SLURM_JOB_GPUS": "${AMD_SPUR_GPUS}",
       "CUDA_VISIBLE_DEVICES": "${AMDGPU_DEVICES}",
       "SLURM_SCRIPT_CONTEXT": "${SPUR_SCRIPT_CONTEXT}"
      }
   EOF
   )
   [ -d ${EXPORT_DIR} ] || exit 0
   GPUS=$(echo ${AMDGPU_DEVICES} | tr "," "\n")
   for GPUID in ${GPUS}; do
       echo ${MSG} >${EXPORT_DIR}/${GPUID}
   done

``slurm-epilog.sh``
~~~~~~~~~~~~~~~~~~~~

Identical to the prolog except for the final loop, which removes the file
instead of writing it:

.. code-block:: bash

   #!/bin/bash
   #
   # Copyright (c) Advanced Micro Devices, Inc. All rights reserved.
   # SPDX-License-Identifier: Apache-2.0
   #

   EXPORT_DIR="/var/run/exporter/"
   mod128_array() {
       local arr_str="$1"
       local arr result
       IFS=',' read -ra arr <<< "$arr_str"
       for i in "${!arr[@]}"; do
           arr[i]=$(( ${arr[i]} % 128 ))
       done
       result=$(IFS=','; echo "${arr[*]}")
       echo "$result"
   }
   AMDGPU_DEVICES=$(mod128_array "${CUDA_VISIBLE_DEVICES}")
   AMD_SPUR_GPUS=$(mod128_array "${SPUR_JOB_GPUS}")
   # CUDA_VISIBLE_DEVICES is empty on AMD hardware; fall back to SPUR_JOB_GPUS so
   # the per-GPU job tracking files are cleaned up correctly on job exit.
   [ -z "${AMDGPU_DEVICES}" ] && AMDGPU_DEVICES="${AMD_SPUR_GPUS}"
   MSG=$(
       cat <<EOF
       {
       "SLURM_JOB_ID": "${SPUR_JOB_ID}",
       "SLURM_JOB_USER": "${SPUR_JOB_USER}",
       "SLURM_JOB_PARTITION": "${SPUR_JOB_PARTITION}",
       "SLURM_CLUSTER_NAME": "${SPUR_CLUSTER_NAME}",
       "SLURM_JOB_GPUS": "${AMD_SPUR_GPUS}",
       "CUDA_VISIBLE_DEVICES": "${AMDGPU_DEVICES}",
       "SLURM_SCRIPT_CONTEXT": "${SPUR_SCRIPT_CONTEXT}"
      }
   EOF
   )
   [ -d ${EXPORT_DIR} ] || exit 0
   GPUS=$(echo ${AMDGPU_DEVICES} | tr "," "\n")
   for GPUID in ${GPUS}; do
       rm -f ${EXPORT_DIR}/${GPUID}
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

1. Confirm the hook scripts are wired up and executable:

   .. code-block:: bash

      sudo journalctl -u spurd -b --no-pager | grep 'loaded spur.conf'
      stat -c '%U:%G %a %n' /usr/share/exporter/slurm-prolog.sh /usr/share/exporter/slurm-epilog.sh

2. Submit a GPU job and, while it is running, check both the tracking file
   and the live metric:

   .. code-block:: bash

      sbatch --partition=gpu --gpus=1 --wrap="sleep 30"

      # while the job is RUNNING:
      cat /var/run/exporter/0
      docker exec dme curl -s localhost:5000/metrics | grep gfx_activity

   Expected file content (values will match your job):

   .. code-block:: text

      { "SLURM_JOB_ID": "8", "SLURM_JOB_USER": "user", "SLURM_JOB_PARTITION": "gpu", "SLURM_CLUSTER_NAME": "", "SLURM_JOB_GPUS": "0", "CUDA_VISIBLE_DEVICES": "0", "SLURM_SCRIPT_CONTEXT": "prolog_slurmd" }

   Expected metric line, with the job's real ID/user/partition populated:

   .. code-block:: text

      gpu_gfx_activity{...,job_id="8",job_partition="gpu",job_user="user",...} 0

   ``gfx_activity`` reads ``0`` here because ``sleep`` does no GPU compute —
   the labels are what this step is checking. To see the value itself spike,
   run an actual compute-bound kernel (see below).

3. After the job completes, confirm cleanup — the tracking file should be
   gone and the labels should reset to empty:

   .. code-block:: bash

      ls /var/run/exporter/
      docker exec dme curl -s localhost:5000/metrics | grep gfx_activity
      # gpu_gfx_activity{...,job_id="",job_partition="",job_user="",...} 0

4. To confirm ``gfx_activity`` itself tracks real GPU load (not just static
   labels), submit a job that runs an actual compute kernel inside a Spur
   container job and sample the metric while it runs:

   .. code-block:: bash

      spur image import docker://docker.io/rocm/dev-ubuntu-22.04:latest

      cat > hip_stress.sh <<'EOF'
      #!/bin/bash
      #SBATCH --partition=gpu
      #SBATCH --gpus=1
      #SBATCH --container-image=docker.io/rocm/dev-ubuntu-22.04:latest
      #SBATCH --container-mounts=/tmp:/workspace
      export LD_LIBRARY_PATH=/opt/rocm/lib:$LD_LIBRARY_PATH
      /opt/rocm/bin/hipcc /workspace/hip_stress.cpp -o /workspace/hip_stress
      /workspace/hip_stress
      EOF
      sbatch hip_stress.sh

      # while RUNNING:
      docker exec dme curl -s localhost:5000/metrics | grep gfx_activity
      # gpu_gfx_activity{...,job_id="10",job_partition="gpu",job_user="root",...} 100

   ``gfx_activity`` jumps to a nonzero value (up to ``100``) for the duration
   of the kernel and drops back to ``0`` — with correct job labels throughout
   — once the job finishes and the epilog removes the tracking file.

.. note::

   ``spur image import`` requires the full ``docker://docker.io/<repo>:<tag>``
   form for Docker Hub images. A bare ``<namespace>/<repo>:<tag>`` (e.g.
   ``rocm/dev-ubuntu-22.04:latest``) is misparsed as a private registry host
   named ``rocm``, and the import fails with ``Error: failed to fetch
   manifest``.

Troubleshooting
-----------------

.. list-table::
   :header-rows: 1
   :widths: 40 60

   * - Symptom
     - Cause / fix
   * - Labels always empty, tracking file has the right values
     - JSON keys in the file don't match ``SLURM_*`` — check for a
       find-and-replace that renamed them to ``SPUR_*``. Keys must stay
       ``SLURM_*``; only the values should come from ``$SPUR_*`` variables.
   * - Tracking file never appears
     - ``spurd`` is running without ``-f <config>`` and loaded no
       ``[hooks]`` block (check ``journalctl -u spurd`` for a ``failed to
       load spur.conf`` warning), or the hook scripts aren't executable.
   * - Tracking file appears but exporter shows nothing in its logs
     - The exporter watches ``/var/run/exporter/`` as configured by
       ``SlurmDir`` in its own build — confirm the container's bind mount
       target matches the path the prolog/epilog scripts write to exactly.
   * - Tracking file left behind after a job ends
     - The epilog hook didn't run — check ``journalctl -u spurd`` for a
       ``epilog_slurmd`` hook failure, or for a job that was killed in a way
       that bypassed normal termination (see :doc:`/user-guide/monitoring-jobs`
       for job state semantics).
