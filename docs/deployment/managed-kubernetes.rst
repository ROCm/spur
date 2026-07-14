SPUR-Managed Kubernetes (k0s)
=============================

SPUR can **provision and own** a Kubernetes cluster across its own nodes using
`k0s <https://k0sproject.io>`_. This is the inverse of :doc:`kubernetes` (running
SPUR *inside* an existing cluster): here ``spur k8s up`` builds the cluster and a
spurd-owned systemd unit keeps k0s running on each node.

One command assigns roles, mesh IPs and pod CIDRs, installs a pinned k0s on every
node, brings up the control plane, mints join tokens, joins the workers, and
(optionally) programs a WireGuard-native CNI — all driven by the existing
spurctld/spurd control plane.

.. note::

   This feature is gated on ``[cluster].enabled``. With it off (the default),
   spurd never touches systemd or k0s, and nothing here applies.

Overview
--------

- **spurctld** (on the head node) owns the cluster lifecycle: role/IP/CIDR
  assignment and phase, replicated through Raft so it survives a restart.
- **spurd** (on every node) owns that node's k0s systemd unit: it installs k0s if
  missing, writes the config/join-token, and reconciles the unit. k0s is never a
  SPUR job, so it survives a spurd restart (spurd re-adopts it on startup).
- **Roles** are assigned automatically: a single node becomes an all-in-one
  ``controller --single``; with two or more nodes the control-plane node becomes a
  ``controller`` and the rest become ``worker`` s.

For Administrators
------------------

Prerequisites
~~~~~~~~~~~~~~

- A working native-host SPUR deployment — ``spurctld`` on the head node and
  ``spurd`` on every node, all registered. See :doc:`native-host`.
- ``spurd`` must run as root (it manages systemd units).
- Outbound HTTPS to ``github.com`` on each node for the k0s download (or
  pre-stage the binary — see `Installing k0s`_).
- For the mesh-native CNI only: a WireGuard mesh (``spur0``) already established
  across the nodes via ``spur net join`` / ``spur net mesh``.

Configure the cluster
~~~~~~~~~~~~~~~~~~~~~~~

Add a ``[cluster]`` section to ``spur.conf`` on every node (spurd reads
``k0s_version`` / ``cni`` from it; spurctld reads the CIDRs and control-plane
choice):

.. code-block:: toml

   [network]
   wg_cidr = "10.44.0.0/16"          # mesh CIDR; node mesh IPs are allocated from here

   [cluster]
   enabled = true
   control_plane_node = "head-node"  # hostname of the k0s control plane (else: first node)
   pod_cidr = "10.42.0.0/16"         # per-node /24s are carved from this
   service_cidr = "10.43.0.0/16"
   cni = "kuberouter"                # "kuberouter" (default) or "calico" (see Networking)
   cni_mtu = 1450                    # Calico MTU; headroom for WireGuard overhead on the mesh
   k0s_version = "v1.36.2+k0s.0"     # pinned; or "latest"

Installing k0s
~~~~~~~~~~~~~~~

``spur k8s up`` auto-installs the pinned k0s on any node that is missing it, so
usually you do nothing. To pre-stage it (e.g. for an air-gapped or
network-restricted node), run on that node **as root**:

.. code-block:: bash

   sudo spur k8s install-k0s                     # the pinned version -> /usr/local/bin/k0s
   sudo spur k8s install-k0s --version latest    # newest k0s release
   sudo spur k8s install-k0s --version v1.36.2+k0s.0 --path /opt/bin/k0s --force

The binary is downloaded from the official k0s GitHub release and SHA-256
verified before it is installed.

Bring the cluster up
~~~~~~~~~~~~~~~~~~~~~~

From the head node (or any host that can reach ``spurctld``):

.. code-block:: bash

   spur k8s up --controller http://localhost:6817

This is idempotent and asynchronous — spurctld reconciles toward ``Ready``:
control plane first, then workers join with freshly minted tokens. A fresh
cluster typically reaches ``Ready`` in one to two minutes (mostly the k0s
download + control-plane bootstrap).

Check status
~~~~~~~~~~~~~

.. code-block:: bash

   spur k8s status
   # phase: ready
   # control-plane: head-node
   #   head-node   controller  active   enabled=true
   #   node-2      worker      active   enabled=true
   #   ...

``phase`` moves ``down -> provisioning -> ready``. Per-node ``component_state`` is
queried live from each agent.

Networking / CNI
~~~~~~~~~~~~~~~~~

**kuberouter** (default) — k0s's built-in CNI. The control-plane API is advertised
on the node's primary interface and workers join over it. No mesh required.

**calico** (``cni = "calico"``) — mesh-native routing. ``spur k8s up`` generates a
k0s config that advertises the API on the control-plane's **mesh IP** and runs
Calico in ``bird`` (BGP, no overlay) mode, and sets each worker's kubelet
``--node-ip`` to its mesh IP. Pod traffic then routes over the WireGuard mesh.
This requires the ``spur0`` mesh to be up first (``spur net join``); membership
reconciliation only maintains the peer set + ``AllowedIPs``, it does not create
the tunnel.

The controller continuously reconciles the full-mesh membership to every node
(pruning peers for departed nodes), so a reboot, a WireGuard restart, or a
control-plane failover self-heals.

Adding a node later
~~~~~~~~~~~~~~~~~~~~~

Start ``spurd`` on the new node (registered to the same controller) and, if using
the mesh CNI, join it to the mesh first. On the next ``spur k8s up`` (or the next
reconcile tick) it is assigned a role + mesh IP + pod CIDR and joins.

Tear down
~~~~~~~~~

.. code-block:: bash

   spur k8s down            # stop + disable the k0s unit on every node
   spur k8s down --reset    # also `k0s reset` (destructive: wipes cluster state)

``--reset`` removes ``/var/lib/k0s`` on every node but leaves the WireGuard mesh
(``spur0``) intact. To switch the CNI, tear down with ``--reset`` and bring the
cluster back up with the new ``cni`` setting.

For Users
---------

Users do not need SPUR access — they interact with the cluster through the
standard Kubernetes tooling.

Get a kubeconfig
~~~~~~~~~~~~~~~~~

Ask an administrator (or run, if you have controller access):

.. code-block:: bash

   spur k8s kubeconfig > admin.conf
   export KUBECONFIG=$PWD/admin.conf
   kubectl get nodes

``spur k8s kubeconfig`` prints the admin kubeconfig to stdout so it can be
redirected to a file.

Run a workload
~~~~~~~~~~~~~~~

Use ``kubectl`` normally:

.. code-block:: bash

   kubectl get nodes -o wide
   kubectl create deployment web --image=nginx
   kubectl run -it --rm probe --image=busybox -- sh

Request GPUs
~~~~~~~~~~~~~

GPU worker nodes advertise ``amd.com/gpu`` (containerd injects the devices from a
CDI spec spurd writes on join). Request them in a pod spec:

.. code-block:: yaml

   apiVersion: v1
   kind: Pod
   metadata:
     name: gpu-probe
   spec:
     restartPolicy: Never
     containers:
       - name: rocm
         image: rocm/dev-ubuntu-24.04:latest
         command: ["rocm-smi"]
         resources:
           limits:
             amd.com/gpu: 1

Command reference
-----------------

.. list-table::
   :header-rows: 1
   :widths: 36 64

   * - Command
     - Purpose
   * - ``spur k8s up [--control-plane-node <h>]``
     - Provision + start the cluster (idempotent).
   * - ``spur k8s status``
     - Cluster phase + per-node component state.
   * - ``spur k8s kubeconfig``
     - Print the admin kubeconfig (redirect to a file).
   * - ``spur k8s down [--reset]``
     - Stop the cluster; ``--reset`` also wipes k0s state.
   * - ``spur k8s install-k0s [--version <tag>|latest] [--path <p>] [--force]``
     - Install the k0s binary on this node (local; run as root).

Configuration reference (``[cluster]``)
---------------------------------------

.. list-table::
   :header-rows: 1
   :widths: 26 22 52

   * - Key
     - Default
     - Meaning
   * - ``enabled``
     - ``false``
     - Enable SPUR-managed k0s. When off, spurd never touches systemd/k0s.
   * - ``control_plane_node``
     - (first node)
     - Hostname of the k0s control plane.
   * - ``pod_cidr``
     - ``10.42.0.0/16``
     - Pod network; per-node /24s are carved from it.
   * - ``service_cidr``
     - ``10.43.0.0/16``
     - Service network.
   * - ``cni``
     - ``kuberouter``
     - ``kuberouter`` or ``calico`` (mesh-native bird routing).
   * - ``cni_mtu``
     - ``1450``
     - Calico MTU emitted into the generated k0s config (leaves WireGuard headroom).
   * - ``k0s_version``
     - pinned
     - k0s release to install/run (a tag or ``latest``).
   * - ``k0s_binary``
     - ``/usr/local/bin/k0s``
     - Install path for the k0s binary.
