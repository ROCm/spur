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

A function without an RDMA device or a ``uverbs`` node is skipped. A NIC's
stable id comes from its PCI address, so it survives reboots and driver
reloads. An on-disk CDI spec of kind ``amd.com/nic`` or ``amd.com/vnic``
replaces discovery for that kind.

``scontrol show node`` lists the NICs with the node's other GRES:

.. code-block:: text

   Gres=gpu:mi300x:8,nic:pollara400:8

Requesting NICs
---------------

.. code-block:: bash

   sbatch --gres=gpu:8,nic:8 train.sh
   srun --gres=nic:pollara400:2 ./ib_test

An untyped request (``nic:2``) matches NICs of any type.

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
default), a job on a NIC node can open only the ``uverbs`` nodes of its own
NICs, plus ``rdma_cm``. Opening another NIC fails with ``EPERM``, and a job
that requested no NICs gets no RDMA device at all. To keep all of
``/dev/infiniband/`` open to every job instead:

.. code-block:: toml

   [cgroup]
   constrain_rdma_devices = false

This does not reset a NIC's RDMA state between jobs.

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
