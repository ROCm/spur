Deploying with Ansible (recommended)
====================================

The ``spur-toolkit`` Ansible playbooks are the recommended way to stand up a real
cluster. They install the three Spur binaries, render ``spur.conf``, create
systemd-managed daemons and the Slurm-compatible symlinks (``sbatch``, ``squeue``,
``sinfo``, …), and stand up PostgreSQL accounting — a single ``ansible-playbook``
run takes a set of hosts from bare SSH to a working cluster. The playbooks live in
the ``ansible/`` directory of the `ROCm/spur-toolkit <https://github.com/ROCm/spur-toolkit>`_
repository; run all commands below from that directory.

Prerequisites
-------------

Control node
~~~~~~~~~~~~

The machine that runs ``ansible-playbook`` — your workstation is fine; it need not
join the cluster.

- ``ansible-core >= 2.14``:

  .. code-block:: bash

     python3 -m pip install --user 'ansible-core>=2.14'

- For the **WireGuard transport only**, also install the ``ansible.utils`` collection
  and ``netaddr``:

  .. code-block:: bash

     ansible-galaxy collection install -r requirements.yml
     python3 -m pip install --user netaddr

Target hosts
~~~~~~~~~~~~

- Reachable over SSH, with ``sudo`` or root access. Every play runs ``become: true``.
- ``systemd`` (the daemons run as systemd services).
- ``curl`` and ``tar``, only when binaries are installed via the ``install.sh`` fallback
  (see :ref:`ansible-quickstart`).

.. _ansible-quickstart:

Quickstart
----------

Build the three binaries in the ``ROCm/spur`` repository, point Ansible at them, edit
the inventory, and deploy.

.. code-block:: bash

   # 1. Build spur binaries (or skip to use a published release via install.sh)
   git clone https://github.com/ROCm/spur.git && cd spur
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y && source "$HOME/.cargo/env"
   sudo apt install -y protobuf-compiler build-essential
   cargo build --release -p spur-cli -p spurctld -p spurd
   SPUR_BUILD="$(pwd)/target/release"
   cd -

   # 2. Ansible + inventory (run from the toolkit's ansible/ directory)
   python3 -m pip install --user 'ansible-core>=2.14'
   cp inventory/hosts.example.ini inventory/hosts.ini
   $EDITOR inventory/hosts.ini

   # 3. Deploy
   ansible-playbook playbooks/deploy.yml -i inventory/hosts.ini -e spur_binary_src="$SPUR_BUILD"

``spur_binary_src`` points at the build-output directory; the ``spur_install`` role
reads ``spur``, ``spurctld``, and ``spurd`` from it by name. Omit it to install a
published release via ``install.sh`` instead, selected by ``spur_version``
(``latest`` | ``nightly`` | ``vX.Y.Z``):

.. code-block:: bash

   ansible-playbook playbooks/deploy.yml -i inventory/hosts.ini

``deploy.yml`` is idempotent — re-running on a healthy cluster re-applies config and
restarts daemons. Binaries roll out by content checksum, so an unchanged re-run is a
near no-op.

Topologies
----------

Deployment shape is determined entirely by the inventory groups and the
``spur_transport`` variable. High availability is auto-enabled when
``spur_controllers`` holds more than one host.

.. list-table::
   :header-rows: 1

   * - Shape
     - Inventory pattern
     - Transport
   * - Single-node
     - One host in **both** ``spur_controllers`` and ``spur_agents``
     - local loopback
   * - Multi-node, direct LAN
     - One host in ``spur_controllers``, all compute in ``spur_agents``
     - LAN IP, unencrypted
   * - Multi-node, WireGuard mesh
     - As above, plus ``spur_transport=wireguard`` (single controller only)
     - encrypted mesh on ``spur0``
   * - HA — multi-controller Raft
     - Odd N ≥ 3 hosts in ``spur_controllers``; auto-enabled
     - direct
   * - HA — separate compute
     - ``spur_controllers`` and ``spur_agents`` are disjoint sets
     - direct

Single-node
~~~~~~~~~~~

The controller and agent are the same host.

.. code-block:: ini

   [spur_controllers]
   node1 ansible_host=10.0.0.10 ansible_user=root

   [spur_agents]
   node1 ansible_host=10.0.0.10 ansible_user=root

Multi-node, direct LAN
~~~~~~~~~~~~~~~~~~~~~~~

One controller; all compute hosts in ``spur_agents``. The controller may also run an
agent (hyperconverged) by listing it in both groups.

.. code-block:: ini

   [spur_controllers]
   ctl ansible_host=10.0.0.10 ansible_user=root

   [spur_agents]
   ctl   ansible_host=10.0.0.10 ansible_user=root
   gpu-1 ansible_host=10.0.0.11 ansible_user=root
   gpu-2 ansible_host=10.0.0.12 ansible_user=root

   [all:vars]
   spur_transport=direct

Multi-node, WireGuard mesh
~~~~~~~~~~~~~~~~~~~~~~~~~~~

Set ``spur_transport=wireguard`` to run node-to-node traffic over an encrypted
WireGuard mesh. See :ref:`ansible-wireguard`.

.. code-block:: ini

   [spur_controllers]
   ctl ansible_host=ctl.example.com ansible_user=root

   [spur_agents]
   gpu-1 ansible_host=gpu1.example.com ansible_user=root
   gpu-2 ansible_host=gpu2.example.com ansible_user=root

   [all:vars]
   spur_transport=wireguard
   spur_wg_cidr=10.44.0.0/16
   spur_wg_port=51820

.. note::

   WireGuard is **single-controller only** — the mesh has no multi-controller
   command. HA therefore requires the ``direct`` transport.

HA — multi-controller Raft
~~~~~~~~~~~~~~~~~~~~~~~~~~~

List three (or more) controllers; the controllers may also run agents
(hyperconverged). HA is auto-enabled once ``spur_controllers`` has more than one host.

.. code-block:: ini

   [spur_controllers]
   ctl-0 ansible_host=10.0.0.10 ansible_user=root
   ctl-1 ansible_host=10.0.0.11 ansible_user=root
   ctl-2 ansible_host=10.0.0.12 ansible_user=root

   [spur_agents]
   ctl-0 ansible_host=10.0.0.10 ansible_user=root
   ctl-1 ansible_host=10.0.0.11 ansible_user=root
   ctl-2 ansible_host=10.0.0.12 ansible_user=root

.. note::

   Use an odd number of controllers, N ≥ 3, in production. Three controllers tolerate
   one failure. An even N gives the same tolerance as N-1, and N=2 has zero tolerance
   (code-path testing only). Raft membership is fixed after the first init — adding,
   removing, or reordering a controller requires a state wipe
   (``-e spur_wipe_state=true``). Compute agents are not Raft members and can be added
   or removed freely.

HA — separate compute
~~~~~~~~~~~~~~~~~~~~~~

Keep the control plane and compute plane on disjoint hosts. A full HA template ships
at ``inventory/hosts.ha.example.ini``.

.. code-block:: ini

   [spur_controllers]
   ctl-0 ansible_host=10.0.0.10 ansible_user=root
   ctl-1 ansible_host=10.0.0.11 ansible_user=root
   ctl-2 ansible_host=10.0.0.12 ansible_user=root

   [spur_agents]
   gpu-1 ansible_host=10.0.0.21 ansible_user=root
   gpu-2 ansible_host=10.0.0.22 ansible_user=root

Non-leader controllers forward client RPCs to the leader, so clients can talk to any
controller. Every agent and controller has all controller endpoints (comma-joined) in
its environment, so ``spurd`` and the CLI rotate past a dead endpoint automatically —
no VIP or DNS is needed.

What deploy.yml does
--------------------

The ``deploy.yml`` play runs in this order:

1. **Preflight and install** on the controllers, agents, and login nodes: checks for
   ``curl``/``tar``/``bash``, checks for port conflicts on 6817/6818/6821, creates the
   directory layout and install dir, installs the binaries (from ``spur_binary_src`` or
   ``install.sh``), creates the Slurm-compatible symlinks, and prepends the install dir
   to ``PATH`` in ``/etc/environment``.
2. **WireGuard mesh** — only when ``spur_transport=wireguard``; skipped entirely
   otherwise.
3. **Accounting stack** on the accounting host: installs PostgreSQL, creates the role
   and database, and opens remote TCP for each controller. Runs before the controllers
   so Postgres is up when ``spurctld`` connects.
4. **Start controllers**: renders ``spur.conf``, installs ``spurctld.service``, sets the
   controller endpoints in ``/etc/environment``, enforces the Raft-membership guard,
   optionally wipes state, starts ``spurctld``, and waits for port 6817 (and, in HA, a
   Raft leader).
5. **Start agents** in parallel: installs ``spurd.service`` pointing at all controllers,
   restarts ``spurd``, and waits for port 6818.
6. **Login nodes** (empty group → no-op): sets client environment only.
7. **Verify** on the first controller: waits for agents to register, prints
   ``spur nodes``, submits a single-node test job (and a multi-node one when there is
   more than one agent), waits for ``COMPLETED``, and prints the output.

Accounting
----------

PostgreSQL accounting is enabled by default (``spur_accounting_enabled=true``).
Accounting is served **in-process by** ``spurctld`` on port 6817 — there is no separate
accounting daemon (Slurm's ``slurmdbd``). Only PostgreSQL is a distinct service.

By default Postgres is installed on the first controller. To place it on a dedicated
node, add that host to a ``[spur_accounting_node]`` group and name it with ``-e``:

.. code-block:: ini

   [spur_accounting_node]
   acct-0 ansible_host=10.0.0.20 ansible_user=root

.. code-block:: bash

   ansible-playbook playbooks/deploy.yml -i inventory/hosts.ini -e spur_accounting_host=acct-0

To disable accounting entirely, pass ``spur_accounting_enabled=false``. Jobs still run;
only ``sacct`` and fairshare become unavailable.

.. code-block:: bash

   ansible-playbook playbooks/deploy.yml -i inventory/hosts.ini -e spur_accounting_enabled=false

.. warning::

   The database credentials default to ``spur`` / ``spur`` / ``spur``. This is fine for
   a lab, but **change** ``spur_accounting_db_password`` **for any production
   deployment.**

.. _ansible-wireguard:

WireGuard mesh
--------------

Set ``spur_transport=wireguard`` in the inventory to run all node-to-node traffic over
an encrypted WireGuard mesh. The ``spur_wireguard`` role installs ``wireguard-tools``,
initializes the mesh on the controller, and joins every agent and login node to it.

.. code-block:: ini

   [all:vars]
   spur_transport=wireguard
   spur_wg_cidr=10.44.0.0/16
   spur_wg_port=51820

The mesh defaults are ``spur_wg_cidr=10.44.0.0/16``, ``spur_wg_port=51820``, and
``spur_wg_interface=spur0``. The control node needs the ``ansible.utils`` collection and
``netaddr`` (see Prerequisites).

The mesh interface is enabled as a ``wg-quick@<iface>`` systemd unit
(``spur_wg_persist=true``, the default) so it is recreated on boot from
``/etc/wireguard/<iface>.conf``; the controller's reconcile then re-pushes peer
membership. Set ``spur_wg_persist=false`` to skip boot enablement (the interface then
only lasts until reboot).

.. note::

   WireGuard is **single-controller only**; HA requires the ``direct`` transport.

SPUR-managed k0s cluster
------------------------

Set ``spur_k8s_enabled=true`` to render the ``[cluster]`` section into the controller
``spur.conf`` (pod/service CIDRs, CNI, MTU) so ``spur k8s up`` is drivable. Combine with
``spur_transport=wireguard`` and ``spur_k8s_cni=calico`` to run pod traffic over the mesh
(Calico ``bird`` native routing; the API is advertised on the control-plane mesh IP).

.. code-block:: ini

   [all:vars]
   spur_transport=wireguard
   spur_k8s_enabled=true
   spur_k8s_pod_cidr=10.42.0.0/16
   spur_k8s_service_cidr=10.43.0.0/16
   spur_k8s_cni=calico
   spur_k8s_control_plane_nodes=k8-master        # 1 name = single CP; 3 or 5 = HA (etcd quorum)
   spur_k8s_nodes=k8-master,gpu-1                 # scope k0s to a subset; empty = whole inventory

``spur_k8s_control_plane_nodes`` is a single CSV that covers both the single and HA cases: one
name is a single control plane, three or five names form an HA control plane (the first is the
etcd bootstrap). It is the k0s control plane and is independent of how many SPUR controllers
(spurctld) run. For an HA k0s control plane, name each control-plane node explicitly
(``spur_k8s_control_plane_nodes=cp1,cp2,cp3``); the ``[N-M]`` hostlist form is only expanded by
``spur_k8s_nodes`` / ``--nodes``.

Run ``deploy.yml`` first (so the controller is rendered with ``[cluster]``), then bring
the cluster up and grow it:

.. code-block:: bash

   ansible-playbook playbooks/k8s_up.yml        -i inventory/hosts.ini
   ansible-playbook playbooks/k8s_add_nodes.yml -i inventory/hosts.ini -e k8s_new_nodes=gpu-3

``k8s_up.yml`` calls ``spur k8s up`` on the first controller and polls ``spur k8s status``
until the cluster is ``ready`` (or fails on ``degraded``). ``k8s_add_nodes.yml`` wraps
``spur k8s add-nodes`` for already-registered agents. The k0s binary is pre-installed by the
``spur_agent`` role on each k0s-scoped node at deploy time (pinned ``spur_k8s_version``), so
``spur k8s up`` does not wait on a runtime download.

.. note::

   When the spurctld controller is **not** itself a k0s node (a mesh-only head node),
   set ``spur_k8s_control_plane_nodes`` to the intended k8s control-plane agent(s). The
   controller still stays on the mesh — it is carried in the mesh membership even without
   a k0s role, so the reconcile does not prune it.

Key variables
-------------

Override any variable per run with ``-e key=value`` (repeatable). The most useful
overrides:

.. list-table::
   :header-rows: 1

   * - Variable
     - Default
     - Purpose
   * - ``spur_binary_src``
     - *(unset)*
     - Local directory of pre-built binaries. Unset → install via ``install.sh``.
   * - ``spur_version``
     - ``latest``
     - ``install.sh`` channel when ``spur_binary_src`` is unset: ``latest`` | ``nightly`` | ``vX.Y.Z``.
   * - ``spur_transport``
     - ``direct``
     - ``direct`` (unencrypted LAN) or ``wireguard`` (encrypted mesh).
   * - ``spur_accounting_enabled``
     - ``true``
     - Deploy PostgreSQL accounting.
   * - ``spur_accounting_host``
     - *(first controller)*
     - Host that runs PostgreSQL.
   * - ``spur_accounting_db_password``
     - ``spur``
     - Accounting database password. Change for production.
   * - ``spur_wg_cidr``
     - ``10.44.0.0/16``
     - WireGuard mesh subnet.
   * - ``spur_wg_persist``
     - ``true``
     - Enable ``wg-quick@<iface>`` so the mesh interface survives reboot.
   * - ``spur_k8s_enabled``
     - ``false``
     - Render the ``[cluster]`` section and allow ``k8s_up.yml`` to run.
   * - ``spur_k8s_pod_cidr`` / ``spur_k8s_service_cidr``
     - ``10.42.0.0/16`` / ``10.43.0.0/16``
     - k0s pod and service networks.
   * - ``spur_k8s_cni``
     - ``calico``
     - ``calico`` (bird native routing over the mesh) or ``kuberouter`` (k0s default).
   * - ``spur_k8s_control_plane_nodes``
     - *(first controller)*
     - CSV of k0s control-plane node names — one for a single CP, 3 or 5 for HA (etcd quorum).
   * - ``spur_k8s_nodes``
     - *(whole inventory)*
     - Hostlist/CSV scoping the k0s cluster to a subset.
   * - ``spur_log_level``
     - ``info``
     - Daemon log verbosity.
   * - ``spur_wipe_state``
     - ``false``
     - Wipe controller Raft state on (re)deploy. Use only for a fresh install or an intentional Raft reinit.

.. warning::

   ``spur_wipe_state=true`` resets the Raft job-id counter — job IDs restart at 1 and
   existing ``sacct`` history is effectively lost. The default, ``false``, preserves
   history. Use ``true`` only for a genuine fresh install.

Day-2 operations
----------------

The toolkit ships lifecycle playbooks for a running cluster. Run each from the
``ansible/`` directory against the same inventory.

- **Add agents** — starts ``spurd`` on new hosts and refreshes controller config
  without bouncing it. The hosts must already be in ``[spur_agents]``.

  .. code-block:: bash

     ansible-playbook playbooks/add_nodes.yml -i inventory/hosts.ini -e new_nodes=gpu-3,gpu-4

- **Remove agents** — drains each node, waits for ``DRAINED``, stops ``spurd``, and
  removes the node from the controller.

  .. code-block:: bash

     ansible-playbook playbooks/remove_nodes.yml -i inventory/hosts.ini -e nodes_to_remove=gpu-3,gpu-4

- **Manage accounts** — declaratively apply QoS, accounts, and users at runtime (no
  restart). Requires accounting enabled.

  .. code-block:: bash

     ansible-playbook playbooks/manage_accounts.yml -i inventory/hosts.ini

- **Healthcheck** — read-only diagnostics (daemons active, leader elected, Postgres up,
  agent ports listening). Exits non-zero on problems, so it works as a cron probe.

  .. code-block:: bash

     ansible-playbook playbooks/healthcheck.yml -i inventory/hosts.ini

For upgrading a live cluster, see :doc:`upgrading`. For tearing a cluster down, see
:doc:`uninstalling`.

.. note::

   Spur has no runtime partition CLI (unlike Slurm's ``scontrol create/update/delete
   partition``). To change partitions, edit ``[[partitions]]`` in the controller
   ``spur.conf`` template and re-run ``deploy.yml`` (a brief controller restart).

See Also
--------

- :doc:`native-host`
- :doc:`upgrading`
- :doc:`uninstalling`
- :doc:`/admin-guide/configuration`
