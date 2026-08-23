========================
WireGuard mesh clusters
========================

This is the authoritative, end-to-end guide to running a Spur cluster over an
encrypted WireGuard mesh: mesh bring-up, why peer endpoints matter for
worker-to-worker connectivity, online node removal, HA over the mesh, and
bringing up a SPUR-managed k0s cluster inside the meshed cluster.

The :doc:`ansible` and :doc:`native-host` pages show the mechanics (variables and
by-hand commands respectively); this page is the conceptual reference they link
to. When a mesh detail matters for correctness, it is explained here.

Why a mesh (and why endpoints matter)
======================================

With ``[network] wg_enabled = true`` every node gets a stable address on the mesh
CIDR (default ``10.44.0.0/16``): the first controller is ``.1``, and each other
node is ``.2``, ``.3``, … All control-plane traffic (scheduler, Raft, agent
heartbeats) and — when k0s runs over the mesh — pod traffic ride the encrypted
``spur0`` interface instead of the underlay LAN.

WireGuard is a **cryptokey router**: a peer entry needs both an *AllowedIPs* set
(which mesh/pod addresses route to that peer) and an *endpoint* (the peer's real
underlay ``host:port`` to send packets to). ``spur net join`` establishes exactly
one tunnel — worker→controller — so a plain join/add-peer flow yields a
**hub-and-spoke**: every node can reach the controller, but two workers have each
other's AllowedIPs with **no endpoint**, so worker↔worker packets are dropped.

A full mesh therefore requires each peer to be advertised **with its underlay
endpoint**. Two mechanisms supply this:

- ``spur net add-peer --endpoint <host:port>`` — register one peer with its
  underlay endpoint (not its mesh IP, which is circular).
- ``spur net mesh --config <membership.json> --self <mesh-ip>`` — apply a full
  mesh from a shared membership document on every node, wiring all remaining
  node↔node tunnels (including controller↔controller) in one pass.

Under a SPUR-managed k0s cluster the controller's reconcile loop maintains this
automatically; for a pure-scheduler mesh (no k0s) the ``spur net mesh`` pass is
what converges the cluster to all-to-all.

Mesh bring-up
=============

On the bootstrap controller (assigned ``.1``):

.. code-block:: bash

   sudo spur net init --cidr 10.44.0.0/16 --port 51820 --interface spur0
   wg show spur0 public-key      # the server key workers need to join

On each other node (controllers included, for HA), join the mesh and read back
its own public key:

.. code-block:: bash

   sudo spur net join \
       --endpoint <bootstrap-underlay>:51820 \
       --server-key <bootstrap-pubkey> \
       --address 10.44.0.2 \
       --prefix-len 16 \
       --interface spur0
   wg show spur0 public-key

Then, on the bootstrap controller, register each joiner **with its underlay
endpoint** so worker↔worker tunnels can form (not just hub-and-spoke):

.. code-block:: bash

   sudo spur net add-peer \
       --key <joiner-pubkey> \
       --allowed-ip 10.44.0.2/32 \
       --endpoint <joiner-underlay>:51820 \
       --interface spur0

Verify all-to-all reachability over the mesh IPs (every node should reach every
other, not just the controller):

.. code-block:: bash

   spur net status                # peers + handshake times
   ping -c1 10.44.0.3             # from a worker, to another worker's mesh IP

Boot persistence
----------------

``spur net init`` / ``join`` bring the interface up but do not enable it for boot.
Enable the ``wg-quick@<iface>`` unit so the interface is recreated on reboot from
``/etc/wireguard/<iface>.conf``:

.. code-block:: bash

   sudo systemctl enable wg-quick@spur0

The Ansible toolkit does this automatically when ``spur_wg_persist=true`` (the
default).

Removing a node from the mesh
=============================

.. important::

   **Deregister the node from the cluster first, then drop its mesh peer** — not
   the other way around. ``spur net remove-peer`` is a purely local ``wg``
   mutation; it does not touch cluster state. Under a SPUR-managed k0s cluster the
   controller's reconcile loop rebuilds mesh membership from live node inventory
   every ~30s, so if the node is still registered it will simply re-push the peer
   you just removed. Run ``spur node remove <node>`` (or the equivalent) first, so
   the reconcile no longer includes it, then remove the peer.

When a node leaves, drop its peer entry so it does not linger as a "ghost" peer
(and, on the node itself, tear the interface down so it does not rejoin on
reboot):

.. code-block:: bash

   # 1. Deregister from the cluster so the reconcile stops advertising it:
   sudo spur node remove <departed-node>

   # 2. On the controller — drop the departed node's peer (idempotent):
   sudo spur net remove-peer --key <departed-node-pubkey> --interface spur0

   # 3. On the departed node — stop and de-persist the interface:
   sudo systemctl disable --now wg-quick@spur0
   sudo rm -f /etc/wireguard/spur0.conf

In an HA mesh, remove the peer on **every** controller, not just the bootstrap —
otherwise the others keep a stale peer.

.. note::

   ``spur net add-peer --program-routes`` (used only on the no-CNI bare-mesh test
   path) installs a kernel route for the peer's pod CIDR. ``remove-peer`` does not
   remove that route, so on a bare-mesh setup drop it by hand
   (``ip route del <pod-cidr> dev spur0``). With a CNI (the normal case) the CNI
   owns the routes and this does not apply.

.. note::

   The Ansible ``remove_nodes.yml`` playbook automates all of the above — the
   controller-side ``remove-peer`` (on all controllers) and the node-side teardown
   — but that WireGuard cleanup ships in `spur-toolkit#23
   <https://github.com/ROCm/spur-toolkit/pull/23>`_, which is not yet merged. Until
   it lands, perform these steps manually.

High availability over the mesh
===============================

HA (3 or 5 controllers with Raft) is supported over WireGuard. All controllers
join the same mesh; because Raft elections require the controllers to reach each
other directly, the full-mesh pass must wire controller↔controller tunnels — a
plain hub-and-spoke would leave the non-bootstrap controllers unable to elect a
leader if the bootstrap goes down.

Point every controller's ``hosts`` / ``peers`` at the **mesh** IPs (not underlay
addresses), in the same order on every controller (``node_id`` is the 1-based
position):

.. code-block:: toml

   [controller]
   hosts = ["10.44.0.1", "10.44.0.3", "10.44.0.4"]
   peers = [
     "10.44.0.1:6821",
     "10.44.0.3:6821",
     "10.44.0.4:6821",
   ]

Raft elects a leader automatically; clients and agents may target any controller
and are redirected to the current leader. See :doc:`native-host` for the full
per-node controller/agent setup. An inventory-driven, multi-controller
WireGuard-mesh flow is added by `spur-toolkit#23
<https://github.com/ROCm/spur-toolkit/pull/23>`_ (not yet merged — until it lands
the Ansible role supports a single controller under WireGuard only).

k0s cluster inside the mesh
===========================

Once the mesh is up, a SPUR-managed k0s cluster can run pod traffic over it. Set
``[cluster] enabled = true`` (or ``spur_k8s_enabled=true`` in Ansible) with Calico
``bird`` native routing so pods ride the mesh:

- ``pod_cidr`` (default ``10.42.0.0/16``) and ``service_cidr`` (``10.43.0.0/16``)
  are carved per node; each node's pod ``/24`` is folded into its peer's
  ``AllowedIPs`` so cross-node pod traffic routes over ``spur0``.
- The k8s API is advertised on the control-plane's mesh IP.

The controller reconcile converges the full mesh (endpoints included) as part of
``spur k8s up``, so a k0s-over-mesh cluster does not need the manual ``net mesh``
pass. See :doc:`managed-kubernetes` for running Spur inside an existing Kubernetes
cluster.

.. note::

   The Ansible ``spur_k8s_*`` variables and the ``k8s_up.yml`` /
   ``k8s_add_nodes.yml`` playbooks referenced for this flow ship in `spur-toolkit#23
   <https://github.com/ROCm/spur-toolkit/pull/23>`_, which is not yet merged. Until
   it lands, drive ``spur k8s up`` / ``add-nodes`` directly (as shown above) rather
   than via those playbooks.
