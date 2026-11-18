AMD AI NICs
===========

Spur schedules AMD Pensando Pollara AI NICs per job, the same way it schedules
GPUs: a node reports each NIC, the scheduler allocates specific NICs, and the
job is given the RDMA device nodes and environment of the NICs it holds.
This page covers native (bare-metal) nodes. Kubernetes nodes are not covered
yet.

Prerequisites
-------------

On each node, load the NIC driver with RDMA support and the user-space verbs
interfaces:

* ``ionic`` and ``ionic_rdma``
* ``ib_uverbs`` (``/dev/infiniband/uverbsN``)
* ``rdma_ucm`` (``/dev/infiniband/rdma_cm``)

``ibv_devices`` lists one ``ionic_N`` device per NIC once these are loaded.

Discovery
---------

With ``[devices] auto_detect = true`` (the default), ``spurd`` finds every
PCI function from vendor ``0x1dd8`` that is bound to ``ionic`` and exposes an
RDMA device. These names match the AMD network device plugin for Kubernetes:

.. list-table::
   :header-rows: 1
   :widths: 18 18 20 44

   * - PCI device
     - CDI kind
     - GRES
     - Notes
   * - ``0x1002``
     - ``amd.com/nic``
     - ``nic:pollara400``
     - Physical function.
   * - ``0x1003``
     - ``amd.com/vnic``
     - ``vnic:pollara400``
     - SR-IOV virtual function.

Every RDMA-enabled ``ionic`` function is claimed this way, whether it sits on a
Pollara card or on another Pensando card such as a storage NIC, and is typed
``pollara400``. ``spurd`` logs the NICs it claims at ``info`` when the set
changes, and once when it no longer claims any. A function without a
``uverbs`` node is skipped, with a warning when that changes. A function
without an RDMA device is an Ethernet-only function and is skipped quietly,
unless no ``ionic`` function on the node has one, which logs a single warning
(is ``ionic_rdma`` loaded?). The stable id of a NIC comes from its PCI address,
so it survives reboots and driver reloads. An on-disk CDI spec of kind
``amd.com/nic`` or ``amd.com/vnic`` replaces discovery for that kind, and
discovery does not log the functions of a kind it replaces.

``scontrol show node`` lists the NICs with the node's other GRES:

.. code-block:: text

   Gres=gpu:mi300x:8,nic:pollara400:8

Requesting NICs
---------------

.. code-block:: bash

   sbatch --gres=gpu:8,nic:8 train.sh
   srun -N 1 --gres=nic:pollara400:2 ./ib_test

An untyped request (``nic:2``) matches NICs of any type. Every step of a job
gets all of the job's NICs on its node; ``--gres=nic`` on an ``srun`` inside an
existing allocation does not narrow them.

Job environment
---------------

For the NICs it holds on a node, a job gets:

.. list-table::
   :header-rows: 1
   :widths: 28 72

   * - Variable
     - Value
   * - ``NCCL_IB_HCA``
     - ``=ionic_0,ionic_3``: the job's RDMA devices. NCCL and RCCL read it; the
       leading ``=`` makes them match names exactly, so ``ionic_1`` does not
       also select ``ionic_10``.
   * - ``PCIDEVICE_AMD_COM_NIC``
     - PCI addresses of the job's NICs, as the AMD network device plugin sets
       it. ``PCIDEVICE_AMD_COM_VNIC`` for virtual functions.
   * - ``SPUR_JOB_NICS``
     - Positional indices of the job's NICs on the node. ``SPUR_JOB_VNICS`` for
       virtual functions.

These replace any value the job sets itself. GPU visibility variables are not
affected by NICs.

Isolation
---------

With ``[cgroup] constrain_devices`` and ``constrain_rdma_devices`` (both on by
default), a job on a NIC node can open the ``uverbs`` node of a NIC only if the
NIC is allocated to it. Opening another job's NIC fails with ``EPERM``, and a
job that requested no NICs reaches none of them. RDMA nodes Spur does not
allocate stay open to every job: ``rdma_cm``, and the devices of HCAs Spur does
not manage, such as another vendor's HCA. An RDMA-enabled ``ionic`` storage NIC
is claimed and gated like any other. To keep all of ``/dev/infiniband/`` open to
every job instead:

.. code-block:: toml

   [cgroup]
   constrain_rdma_devices = false

The ``EPERM`` needs the cgroup v2 BPF device filter. If installing it fails,
``spurd`` warns and runs the job without device isolation, unless
``[cgroup] required = true``, which refuses the job instead.

Limits of RDMA gating:

* The RDMA devices a job cannot open are fixed when the job starts, from the
  NICs the node had discovered then, and apply to all of its steps.
* A NIC whose ``uverbs`` node appears before discovery has picked it up (about
  two refresh intervals after ``ionic_rdma`` loads) is not withheld from jobs
  started in that window.
* A virtual function Spur did not hand out when a job started stays open to
  that job for its whole run, even once a later refresh adds it and Spur
  allocates it to another job. This covers virtual functions created after the
  job started and those left out of a private CDI spec. Keep a private spec
  complete, and requeue jobs that started before NICs were added.
* Renaming or renumbering RDMA devices, or reloading ``ionic_rdma``, while
  ``spurd`` runs is not picked up: restart ``spurd`` afterwards.
* Gating compares paths under ``/dev/infiniband/``. A device node for the same
  RDMA device elsewhere is not withheld.
* Gating does not reset a NIC's RDMA state between jobs.

Configuring NICs by hand
------------------------

Without discovery, list the NICs as a file-backed GRES. Each file is one NIC:

.. code-block:: toml

   [[devices.gres]]
   name = "nic"
   type = "pollara400"
   file = "/dev/infiniband/uverbs[0-7]"

A job then gets the ``uverbs`` nodes it was allocated and ``SPUR_JOB_NICS``,
but not ``NCCL_IB_HCA`` or ``PCIDEVICE_AMD_COM_NIC``, as a GRES entry has no
RDMA device name or PCI address. Use a CDI spec with the
``spur.amd.com/rdma-device`` and ``spur.amd.com/pci-bdf`` annotations to
provide them.
