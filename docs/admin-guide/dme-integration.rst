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
named after the render ID of a GPU: the ``N`` of its ``/dev/dri/renderD<N>``.
Each file is a small JSON document describing the job currently using that
GPU:

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
     - the ``cluster_name`` metric label
   * - ``CUDA_VISIBLE_DEVICES``
     - which GPUs get those labels

A Spur ``prolog`` hook writes one file per GPU when a job starts; the
matching ``epilog`` hook deletes them when the job ends, or when its launch
fails after the ``prolog`` ran. Deleting a file
clears the labels only of the GPU the file is named after, so each file must
list only its own render ID in ``CUDA_VISIBLE_DEVICES``. A file that lists
other GPUs leaves their labels behind after the job ends, or takes them from
another job.

Spur's hook environment already twins ``SPUR_JOB_ID``/``SPUR_JOB_PARTITION``
into ``SLURM_JOB_ID``/``SLURM_JOB_PARTITION``, so the scripts below use those
directly. ``SLURM_JOB_USER`` comes from ``SPUR_JOB_USER``. Spur does not pass
the cluster name to node hooks, so ``SLURM_CLUSTER_NAME`` is written from
``SPUR_CLUSTER_NAME`` only when ``spurd``'s own environment sets it, and is
empty otherwise. The render IDs come
from ``SPUR_JOB_GPUS``, which holds Spur's internal GPU ids rather than
render IDs or GPU indices: the scripts map each id to its render ID through
the kernel's GPU topology under ``/sys/class/kfd``.

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
for hook scripts), **executable**, and owned by root. From a checkout of the
Spur repository:

.. code-block:: bash

   sudo install -D -o root -g root -m 0755 -t /usr/share/exporter \
     examples/hooks/dme/slurm-prolog.sh examples/hooks/dme/slurm-epilog.sh
   sudo install -o root -g root -m 0644 -t /usr/share/exporter \
     examples/hooks/dme/common.sh

Both scripts load ``common.sh`` from ``/usr/share/exporter``: Spur runs a hook
through a file descriptor, so a script cannot find files next to itself. To
install the three files elsewhere, set ``SPUR_DME_HOOKS`` to their directory in
``spurd``'s environment; hooks inherit it.

Restart ``spurd`` after editing ``spur.conf`` — hooks are only read at agent
startup:

.. code-block:: bash

   sudo systemctl restart spurd

Example scripts
----------------

The scripts live in ``examples/hooks/dme/`` in the Spur repository, and the
end-to-end tests run these same files. Adjust ``EXPORT_DIR`` only if the
exporter container mounts a different host path.

``slurm-prolog.sh``
~~~~~~~~~~~~~~~~~~~~

.. literalinclude:: ../../examples/hooks/dme/slurm-prolog.sh
   :language: bash

``slurm-epilog.sh``
~~~~~~~~~~~~~~~~~~~~

Same GPU lookup as the prolog; removes the files instead of writing them:

.. literalinclude:: ../../examples/hooks/dme/slurm-epilog.sh
   :language: bash

``common.sh``
~~~~~~~~~~~~~~

Maps the GPU ids in ``SPUR_JOB_GPUS`` to render IDs. A GPU it cannot map gets
no label, and a line on the script's stderr, which ``spurd`` logs:

.. literalinclude:: ../../examples/hooks/dme/common.sh
   :language: bash

Replacing older scripts
~~~~~~~~~~~~~~~~~~~~~~~

Earlier versions of these scripts computed ``SPUR_JOB_GPUS`` modulo 128.
Spur's GPU ids are not GPU indices, so that put every job's labels on GPU
``0``, left labels behind after jobs ended, and could show one job's labels on
another job's GPU. After installing the scripts above, remove the files the
old scripts left, once:

.. code-block:: bash

   sudo find /var/run/exporter -maxdepth 1 -type f -regextype posix-extended \
     -regex '.*/([0-9]|[1-9][0-9]|1[01][0-9]|12[0-7])' -delete

The old scripts named their files 0 to 127. Render IDs start at 128, so this
leaves the new scripts' files alone and is safe to run while jobs are running.

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
   ls /var/run/exporter/
   cat /var/run/exporter/128
   docker exec dme curl -s localhost:5000/metrics | grep gfx_activity

Expect one file per GPU of the job, named by its render ID. Which render ID a
GPU has depends on the node; ``128`` here is an example. Expected file
content (values will match your job):

.. code-block:: text

   {"SLURM_JOB_ID":"8","SLURM_JOB_USER":"user","SLURM_JOB_PARTITION":"gpu","SLURM_CLUSTER_NAME":"","CUDA_VISIBLE_DEVICES":"128"}

Expected metric line, with the job's real ID/user/partition populated on that
GPU's series only:

.. code-block:: text

   gpu_gfx_activity{...,job_id="8",job_partition="gpu",job_user="user",...} 0

After the job completes, confirm cleanup — the tracking file should be gone
and the labels should reset to empty:

.. code-block:: bash

   ls /var/run/exporter/
   docker exec dme curl -s localhost:5000/metrics | grep gfx_activity
   # gpu_gfx_activity{...,job_id="",job_partition="",job_user="",...} 0
