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
   * - ``--container-mounts <mount>[,<mount>...]``
     - Mount host paths or a tmpfs into the container. Repeatable, or give several
       mounts separated by commas. See `Container Mounts`_.
   * - ``--container-workdir <dir>``
     - Working directory inside the container.
   * - ``--container-name <name>``
     - Persist the container across jobs. Required for ``spur image export``.
   * - ``--container-readonly``
     - Not yet implemented — rejected at submission. (A read-only container
       root is not enforced yet; the flag is refused rather than silently
       ignored.)
   * - ``--container-mount-home``
     - Mount your home directory into the container.
   * - ``--container-env KEY=VAL``
     - Set an environment variable inside the container. Repeatable.
   * - ``--container-entrypoint <cmd>``
     - Shell command run inside the container immediately before the job script
       (``<cmd> && <script>``). It does not replace the image's ENTRYPOINT.
   * - ``--container-remap-root``
     - Map the job user to root inside the container.
   * - ``--container-auth-socket``
     - On ``srun``, ``sbatch`` and ``salloc``. Mount the node's credential mint
       socket into the container so that a nested ``srun`` inside it can
       authenticate. Off by default: without it nothing in the container can
       mint a credential, and a nested ``srun`` fails with "credential mint not
       available in this container". With it, anything in the container can
       act on the cluster as you. On ``salloc`` it applies to every
       ``srun --container-image`` step of the allocation. A
       ``--container-mounts`` entry that would bind the socket in without the
       flag, either the socket itself or a directory that contains it such as
       ``/run/spur``, is rejected. It has no effect on a step that enters its
       job's running container. Only meaningful under ``[auth] plugin =
       "spur"``, and it does not hide a token file in a mounted home directory.

.. warning::

   Breaking change: a container no longer gets the credential mint socket by
   default. Workflows that run ``srun`` (or any other Spur command) from inside
   a container must now pass ``--container-auth-socket`` on the job or step
   that starts the container.

A GPU training job with a read-only data mount:

.. code-block:: bash

   sbatch --container-image registry.example.com/pytorch:latest \
          --container-mounts /data:/data:ro \
          --gres=gpu:8 train.sh

An interactive shell in a container with your home directory mounted. ``--pty``
connects the shell to your terminal; without it, ``bash`` gets no input and exits
at once:

.. code-block:: bash

   srun --pty --container-image ubuntu:22.04 --container-mount-home bash

How the Image Is Treated
~~~~~~~~~~~~~~~~~~~~~~~~

The node agent prepares the container as root before handing it to you, so it
never lets anything in your image lead that work onto the node itself:

- A mount destination (``--container-mounts``, ``--container-mount-home``) is
  resolved the way the container will see it: links in the image are followed
  inside the image. With ``/lib -> usr/lib`` in the image, a mount on
  ``/lib/modules`` lands on the image's ``/usr/lib/modules``; an absolute link
  such as ``/data -> /scratch`` leads to the image's own ``/scratch``, never the
  node's. ``/etc``, where Spur writes the container's account and DNS files, and
  ``/.spur``, where it writes the job's entry script, are resolved the same way.
- The container refuses to start when the image's ``/dev``, ``/proc``, ``/sys``
  or ``/run`` is a link, or when ``/etc/passwd`` or ``/etc/group`` is a link or
  anything but a regular file, such as a FIFO or a device node.
- A user name or home directory containing ``:`` or a control character such
  as a newline is refused, since it would corrupt the container's
  ``/etc/passwd``.
- The image's ``/etc/resolv.conf`` is replaced with the node's DNS settings,
  even when the image ships it as a link.

Container Mounts
----------------

Each mount takes one of these forms:

.. list-table::
   :header-rows: 1
   :widths: 40 60

   * - Form
     - Mounts
   * - ``<src>:<dst>``
     - The host path ``<src>`` at ``<dst>``. ``<src>`` must be an absolute path.
       On a native node it must exist; Kubernetes partitions differ, see below.
   * - ``tmpfs:<dst>``
     - An empty in-memory filesystem at ``<dst>``, writable by every user in the
       container unless it is ``:ro``. Add ``:size=<n>`` to cap it, in bytes or
       with a ``K``, ``M`` or ``G`` suffix in powers of 1024, either case
       (``tmpfs:/cache:size=8G``). Without a size, a native node uses the
       kernel's default of half the node's memory; on Kubernetes the volume
       counts against the pod's memory limit. Its contents are gone when the
       container exits. ``tmpfs:/tmp`` gives the job a private ``/tmp``.
   * - ``pvc:<claim>:<dst>``
     - A PersistentVolumeClaim, on Kubernetes partitions only.

Append ``:ro`` to any form for read-only, or ``:rw`` to say read-write
explicitly. ``<dst>`` must be an absolute path other than ``/`` and
``/.spur``, which holds the job's entry script, and it cannot contain ``..``.
A destination whose last part is a ``size=`` option, such as ``tmpfs:/size=1G``,
is an error rather than a mount at a folder named ``size=1G``. Two mounts cannot
share a destination, however it is spelled, and one request holds at most 64
mounts. These two commands are the same:

.. code-block:: bash

   sbatch --container-image=/images/pytorch.sqsh \
          --container-mounts=/data:/data:ro,/scratch:/scratch,tmpfs:/cache:size=8G \
          train.sh

   sbatch --container-image=/images/pytorch.sqsh \
          --container-mounts=/data:/data:ro \
          --container-mounts=/scratch:/scratch \
          --container-mounts=tmpfs:/cache:size=8G \
          train.sh

A comma always separates two mounts, so a path that contains a comma cannot be
mounted. An empty value, such as ``--container-mounts=$MOUNTS`` with ``MOUNTS``
unset, means no mounts.

A mount Spur cannot read is rejected when the job is submitted or the ``srun``
step is created, and the error names it. That covers an unknown flag, a relative
source or destination, an empty entry in a list, ``ro`` with ``rw``, a
destination with ``..`` in it, more than 64 mounts, and a reserved or repeated
destination. Mounts are only checked when the job or step
has a container image, since they are not used otherwise. The SpurJob
``volumes`` field takes the same forms and is checked the same way.

During a rolling upgrade, a ``tmpfs:`` mount is only applied by a spurd that
supports it. A job that asks for one stays pending until every node it was
placed on has been upgraded, and an ``srun`` step that asks for one is refused on
a node that has not.

A destination the image turns into ``/.spur`` or a folder above it, for example
through a link, is refused when the container starts, so a mount can never hide
the job's entry script.

On a native node, a bind source that does not exist fails the job or step. The
reason, naming the mount, is in the node agent's log, so ask your cluster
administrator if the job's output does not explain the failure. A ``pvc:`` mount
on a native node is refused at launch: the job stays pending, is retried, and is
held after repeated failures. Spur never drops a mount and runs the job without
it.

On Kubernetes partitions, a bind source that does not exist on the node is
created as an empty directory, so a wrong source path gives the job an empty
directory instead of an error.

.. note::

   Breaking change: these mounts used to be accepted, and were applied
   differently or silently dropped. They are now rejected:

   - an unknown flag such as ``:z``, or a third field other than ``ro`` or
     ``rw`` (any third field containing ``ro``, such as ``:error``, used to make
     a native mount read-only);
   - a relative source path, or a destination that is relative, ``/``, under
     ``/.spur``, contains ``..``, ends in a ``size=`` option, or is used by
     another mount;
   - more than 64 mounts in one request;
   - an empty entry inside a list, such as ``a,,b`` or a trailing comma;
   - a path containing a comma, which is now split into two mounts.

   The SpurJob ``volumes`` field is checked the same way, so a volume with one
   of these forms now fails the whole SpurJob at submit.

   The job's entry script moved from ``/tmp`` to ``/.spur``, so a mount on
   ``/tmp`` no longer hides it.

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

Containerized Job Steps
-----------------------

``srun`` steps run inside a container too, not just batch jobs. The container
flags above apply to a step's ``srun`` and behave as follows:

- **Step inside a containerized allocation.** When the batch job was submitted
  with ``--container-image`` (``sbatch --container-image ...``), a nested
  ``srun`` step enters that already-running container — it shares the job's
  image, mounts, and GPU devices. Pyxis does not support ``srun`` from inside a
  containerized ``sbatch`` script; Spur lets the step join the job's container,
  which is what multi-node launchers rely on. Naming the job's own image on the step
  (``srun --container-image`` with the same image, including one inherited from
  ``SRUN_CONTAINER``) also enters it. Two references are the same image when they
  are identical or resolve to the same squashfs file. A step cannot use a
  different image: ``srun --container-image`` naming one fails with an error
  rather than run in the wrong image.
  A nested ``srun`` there can authenticate only if the ``sbatch`` (or
  ``salloc``) was submitted with ``--container-auth-socket``.

- **Step with its own image.** A standalone ``srun --container-image``, an
  ``salloc`` allocation followed by ``srun --container-image``, or an
  ``srun --container-image`` inside an ``sbatch`` script that was submitted
  without ``--container-image``, creates a fresh container for that step.
  Different steps in one allocation can use different images and mounts. The job's
  cgroup limits and GPU allocation still apply to the step. A step without
  ``--container-image`` in such a batch job keeps running on the node's own
  root filesystem.

.. note::

   Breaking change: ``srun --container-image=B`` inside a script submitted with
   ``sbatch --container-image=A`` no longer runs the step in ``A`` and exits 0
   with the flag ignored. It now fails, so scripts that rely on that must drop
   the step's ``--container-image`` or submit the job without one.

Every container Spur starts for a job, the batch job's and each fresh step's, has
the job's spool folder ``/var/spool/spur/job<id>`` mounted read-only at the same
path. Spur keeps the wrappers it generates for steps with several tasks per node,
``--label`` or ``--mpi=pmix`` there, so those steps run in either kind of container.

Allocate first, then run steps from inside the allocation shell. By default a
single step fans out one container per allocated node; ``-N`` or ``-w`` limits
container creation or entry to the selected step nodes:

.. code-block:: bash

   salloc -N2 --gpus-per-node=8
   # ...now inside the allocation shell:
   srun --container-image trainer.sqsh <command>   # one container per node
   srun -N1 --container-image trainer.sqsh <command>   # one allocated node
   srun -w node002 --container-image trainer.sqsh <command>   # exact node

.. note::

   ``-N`` and ``-w`` only choose which allocated nodes run the step. They do not
   resize the job-level CPU, memory, or GPU cgroup on those nodes.

GPU allocation, bind mounts, and cancellation apply per step: a cancelled step
tears down its container and cleans up its rootfs without affecting the rest of
the allocation.

Interactive steps (``srun --pty``) run inside the container too — you get a real
terminal inside the container rather than on the host. ``--pty`` still ignores
``--nodes`` and uses only the first ``-w`` name. The step uses its own
``--container-image`` when given one, otherwise a nested ``srun --pty`` inherits
the image from its containerized ``sbatch`` job:

.. code-block:: bash

   salloc -N1 --gpus-per-node=8
   # ...now inside the allocation shell:
   srun --pty --container-image trainer.sqsh bash   # interactive shell in the container

   # or attach an interactive shell to a running containerized job
   # (enters its container via nsenter):
   sbatch -N1 --container-image trainer.sqsh long_job.sh
   srun --jobid=<id> --overlap --pty bash            # same container, interactive

A ``--pty`` step nested inside a containerized ``sbatch`` job enters
the parent's running container (like any other nested step) rather than building
a fresh one, and fails if it names a different image.

Isolating Steps From Each Other
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

One allocation can run several unrelated containers side by side on a node, each
kept apart from the others. Give each step its own image, its own share of the
allocation (:ref:`see the step resource flags <step-own-share>`):

.. code-block:: bash

   salloc -N1 --gpus-per-node=8 --cpus-per-task=64 --mem=512G
   # ...inside the allocation shell:
   srun --container-image=inference.sqsh -c 16 --mem=128G --gpus=2 ./serve.sh &
   srun --container-image=batch.sqsh -c 8 --mem=32G --gpus=1 ./process.sh &
   wait

Each such step runs from its own image in its own cgroup, with the CPU, memory
and GPU share it asked for held for it, and only its own GPUs. It still runs as your user and shares the node's
network namespace with the rest of the allocation.

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
