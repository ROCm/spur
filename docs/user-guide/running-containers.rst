Running Jobs in Containers
==========================

Spur can run a job inside an OCI container image, backed by a squashfs snapshot
of the image. You import an image once with ``spur image``, then pass container
flags to ``spur submit`` (Slurm ``sbatch``) or ``spur run`` (Slurm ``srun``) to
run your job inside it. This page covers importing images, running container
jobs, and executing extra commands inside a running one.

Importing Images
----------------

``spur image import <ref>`` fetches an image and packs it into a zstd squashfs
``.sqsh`` file (this needs ``squashfs-tools`` installed). The reference can be a
registry image or a local container source:

.. code-block:: bash

   spur image import ubuntu:22.04                 # registry
   spur image import registry.example.com/pytorch:latest # registry with host
   spur image import docker://myapp:latest        # registry via docker transport
   spur image import dockerd://myapp:latest        # local Docker daemon
   spur image import podman://myapp:latest         # local Podman storage

List, export, and remove imported images:

.. code-block:: bash

   spur image list
   spur image export mycontainer -o /tmp/mycontainer.sqsh
   spur image remove mycontainer

Images are stored in the first of these directories that is usable:
``$SPUR_IMAGE_DIR``, then ``/var/spool/spur/images``, then ``~/.spur/images``.
At submit time Spur resolves a bare image name (e.g. ``busybox``) to an absolute
``.sqsh`` path in these directories when it finds one — which works when the
login and compute nodes share a filesystem. Otherwise the name is passed through
for the node agent to resolve.

Running a Container Job
-----------------------

Add container flags to ``sbatch`` or ``srun``. The job's script (or command)
runs inside the container.

.. list-table::
   :header-rows: 1
   :widths: 34 66

   * - Flag
     - Description
   * - ``--container-image <ref|/path.sqsh>``
     - Image to run in. A registry reference, a stored name, or a ``.sqsh`` path.
   * - ``--container-mounts <src>:<dst>[:ro]``
     - Bind-mount a host path into the container. Repeatable; append ``:ro`` for
       read-only.
   * - ``--container-workdir <dir>``
     - Working directory inside the container.
   * - ``--container-name <name>``
     - Persist the container across jobs. Required for ``spur image export``.
   * - ``--container-readonly``
     - Mount the container root read-only.
   * - ``--container-mount-home``
     - Mount your home directory into the container.
   * - ``--container-env KEY=VAL``
     - Set an environment variable inside the container. Repeatable.
   * - ``--container-entrypoint``
     - Use the image's entrypoint (``sbatch``).
   * - ``--container-remap-root``
     - Map the job user to root inside the container.

A GPU training job with a read-only data mount:

.. code-block:: bash

   sbatch --container-image registry.example.com/pytorch:latest \
          --container-mounts /data:/data:ro \
          --gres=gpu:8 train.sh

An interactive shell in a container with your home directory mounted:

.. code-block:: bash

   srun --container-image ubuntu:22.04 --container-mount-home bash

Environment Inside the Container
--------------------------------

A container job starts from the image's own environment — the ``config.Env`` that
``docker run`` would apply — and layers the job's environment on top of it. Spur
records the image config at ``spur image import`` time, so images imported by an
older Spur have nothing recorded and keep starting from the job environment
alone; re-import them to pick up their environment.

Later entries win:

1. the image's ``config.Env``
2. the job environment (``--export``, which defaults to ``ALL``)
3. ``--container-env KEY=VAL``
4. admin ``environ.d`` files and hook variables

``PATH`` and ``LD_LIBRARY_PATH`` are the exception: instead of being replaced,
the image's entries and the job's are joined, image first, so that the programs
and libraries shipped in the image stay reachable while additions from the
submitting host still apply. An explicit ``--container-env PATH=...`` replaces
the result outright.

This is what lets an image's own tooling run without spelling out its paths:

.. code-block:: bash

   # torchrun lives on the image's PATH (/opt/venv/bin), not the host's
   sbatch --container-image registry.example.com/pytorch:latest train.sh

Exec Into a Running Container Job
---------------------------------

``spur exec <job_id> <command...>`` runs a one-shot command inside a job's
running container. Output is buffered — this is not an interactive terminal.

.. code-block:: bash

   spur exec 1024 rocm-smi
   spur exec 1024 -- ls -la /workspace

.. tip::

   For an interactive session inside a running job, use ``srun --jobid <id>
   --overlap`` or :doc:`spur attach <interactive>` instead — ``spur exec`` has no
   TTY mode.

See Also
--------

- :doc:`submitting-jobs`
- :doc:`interactive`
