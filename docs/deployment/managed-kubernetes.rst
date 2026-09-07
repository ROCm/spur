Spur-Managed Kubernetes (k0s)
=============================

Spur can **provision and own** a Kubernetes cluster across its own nodes using
`k0s <https://k0sproject.io>`_. This is the inverse of :doc:`kubernetes` (running
Spur *inside* an existing cluster): here ``spur k8s up`` builds the cluster and a
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
  Spur job, so it survives a spurd restart (spurd re-adopts it on startup).
- **Roles** are assigned automatically: a single node becomes an all-in-one
  ``controller --single``; with two or more nodes the control-plane node becomes a
  ``controller`` and the rest become ``worker`` s.

For Administrators
------------------

Prerequisites
~~~~~~~~~~~~~~

- A working native-host Spur deployment — ``spurctld`` on the head node and
  ``spurd`` on every node, all registered. See :doc:`native-host`.
- ``spurd`` must run as root (it manages systemd units).
- Outbound HTTPS to ``github.com`` on each node for the k0s download (or
  pre-stage the binary — see `Installing k0s`_).
- For the mesh-native CNI only: a WireGuard mesh (``spur0``) already established
  across the nodes via ``spur net join`` / ``spur net mesh``.
- The ports below, open between the cluster nodes. ``spur k8s silo prepare-node``
  opens them. See `Open the k0s ports`_.
- For ``silo install`` only: ``chrony`` installed on every worker node. The
  platform stack runs an exporter that mounts ``/run/chrony`` from the host, and
  Ubuntu's cloud image keeps time with ``systemd-timesyncd``, which creates no
  such directory. The exporter then never becomes healthy, and ArgoCD blocks on
  it instead of retrying the rest of the observability stack.

Open the k0s ports
~~~~~~~~~~~~~~~~~~~

k0s needs these ports between the nodes. ``spur k8s silo prepare-node`` opens them
with ``iptables``. This table says what each one carries, so you can open them
yourself when another tool owns the host firewall.

.. list-table::
   :header-rows: 1
   :widths: 12 10 30 48

   * - Port
     - Proto
     - Direction
     - Purpose
   * - ``6443``
     - TCP
     - worker → control plane
     - kube-apiserver. A worker cannot join without it.
   * - ``8132``
     - TCP
     - worker → control plane
     - konnectivity agent.
   * - ``10250``
     - TCP
     - control plane → worker
     - kubelet: logs, ``exec``, and metrics.
   * - ``9443``
     - TCP
     - control plane ↔ control plane
     - k0s join API. HA only.
   * - ``2380``
     - TCP
     - control plane ↔ control plane
     - etcd peer. HA only.
   * - ``6817``, ``6818``, ``6821``
     - TCP
     - node ↔ node
     - ``spurctld``, ``spurd``, and the controller's Raft port.
   * - ``80``, ``443``
     - TCP
     - client → cluster
     - The platform-stack gateway. ``silo install`` only.
   * - ``30000-32767``
     - TCP, UDP
     - client → cluster
     - NodePort range.

The CNI also needs pod and service traffic to pass between the nodes. Calico
adds ``179``/TCP for BGP, and ``4789``/UDP when you configure VXLAN.
``silo prepare-node`` opens neither, because the CNI is a per-cluster choice.

.. warning::

   A stock cloud image often ends its ``INPUT`` chain with a catch-all reject
   and opens only ``22``. The worker then logs
   ``Failed to connect to apiserver [...] no route to host`` and never joins,
   while ``spur k8s status`` still reports ``phase: ready``. The phase describes
   the node components SPUR starts, not Kubernetes membership. Run
   ``kubectl get nodes`` to confirm that the workers joined.

   A single-node cluster hits this too. A pod reaches the cluster's own service
   IP through a DNAT to the node's ``6443``, and that packet crosses ``INPUT``.
   Without the rule, every in-cluster API client fails with ``no route to host``.

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
   storage_provisioner = "local-path"  # default StorageClass for PVCs; or "none"
   # local_path_dir = "/mnt/scratch/local-path"  # point local-path at a big disk (default /var/lib)
   k0s_version = "v1.36.2+k0s.0"     # pinned; or "latest"

Prepare the node
~~~~~~~~~~~~~~~~

.. note::

   **SPUR assumes the node is already configured.** Preparing a node is the
   operator's job, and ``spur k8s silo install`` neither performs it nor
   requires it. The install runs a read-only sanity check first, names what
   looks wrong, and continues.

   ``spur k8s silo prepare-node`` remains available as a convenience for a bare
   node. It is not a step in the deployment workflow, and a node prepared by
   another tool needs it no more than a node prepared by hand.

``spur k8s silo prepare-node`` makes a bare node ready to run k0s. It runs locally as
root and needs no controller, so it works before any cluster exists.

It gives the k0s data directory its own disk, opens the ports the cluster
needs, raises the kernel limits the platform stack exhausts, and makes sure the
node keeps the time:

.. code-block:: bash

   sudo spur k8s silo prepare-node --data-disk /dev/sdb
   sudo spur k8s silo prepare-node --data-disk /dev/sdb --dry-run   # report, change nothing

The data disk
'''''''''''''

k0s keeps etcd, every container image, and every kubelet volume under
``/var/lib/k0s``. The platform stack takes that well past 100 GB, which is more
than a stock cloud image's root disk holds.

.. warning::

   Run this **before the node first runs k0s**. k0s records an absolute path for
   every kubelet volume, so a data directory that moves onto another device later
   breaks each of those mounts. The command refuses to run once ``/var/lib/k0s``
   holds data on the root filesystem, because at that point the move is unsafe.

The command formats the device ext4 only when it carries no filesystem, and it
adds an ``/etc/fstab`` entry keyed by UUID with ``nofail``. A device that already
carries another filesystem is refused unless you pass ``--force-format``, which
erases it. A second run reports what is already in place and changes nothing.

Omit ``--data-disk`` to leave the data directory on the root filesystem.

The firewall
''''''''''''

The command then adds two ``ACCEPT`` rules at the head of the ``INPUT`` chain,
one for TCP and one for UDP, covering the ports in `Open the k0s ports`_. It
inserts at the head so the rules sit ahead of a catch-all reject.

It adds and never removes. The node keeps the policy it arrived with, and the
``FORWARD`` chain stays untouched, because the CNI writes its own rules there.
A second run finds the rules already in place and changes nothing.

The command speaks ``iptables`` and nothing else. It stops with an error when
``iptables`` is absent or cannot read the ``INPUT`` chain. Open the ports
yourself when ``ufw``, ``firewalld``, or another tool owns the ruleset: those
tools rewrite the whole ruleset on reload, and they would drop a rule SPUR
inserted behind their back.

Finally the command installs and enables ``spur-k0s-firewall.service``, which
puts the same two rules back after a reboot. iptables rules live in kernel
memory, so a reboot empties the chain and something has to write them back.

The unit carries the two rules and nothing else. It tests for each rule before
it inserts it, exactly as the command does, so a boot adds nothing to a node
that already carries them. It runs after ``netfilter-persistent.service``,
because that unit flushes the chain before it restores.

.. warning::

   The command no longer writes ``/etc/iptables/rules.v4``. An earlier version
   saved the whole live ruleset there with ``iptables-save``, which fails on a
   node where the cluster already runs: the snapshot picks up the CNI's chains,
   and those match against ipsets that the CNI creates at start-up.
   ``iptables-restore`` resolves every line before it applies any, so one
   missing ipset leaves the boot with **no** rules at all, SPUR's included.

   A node prepared by that version still holds the bad file. Delete
   ``/etc/iptables/rules.v4``, or remove the lines that name an ipset, then run
   ``spur k8s silo prepare-node`` again to install the unit.

Kernel limits
'''''''''''''

The command last raises two ``inotify`` limits, and writes them to
``/etc/sysctl.d/90-spur-k0s.conf`` for the next boot:

.. list-table::
   :header-rows: 1
   :widths: 40 14 46

   * - sysctl
     - Floor
     - Why
   * - ``fs.inotify.max_user_instances``
     - ``8192``
     - Stock Ubuntu allows 128, which the platform stack exhausts.
   * - ``fs.inotify.max_user_watches``
     - ``524288``
     - A recent kernel scales this from RAM and already sits far higher. The
       floor protects a small-memory node, where the default is 8192.

These are floors, not targets. A node that already sits higher keeps its value,
and the drop-in records the value the command settled on, so the boot-time apply
never lowers the node.

An ``inotify`` limit counts **per UID on the host**. A container gets no
namespace of its own, so kubelet, containerd, and every pod that runs as root
draw from one pool. The platform stack is full of config watchers, and the
cluster runs out.

.. warning::

   This failure arrives late and reads as something else. Once the pool is
   empty, ``inotify_init1`` returns ``EMFILE``, which a Go workload reports as
   ``failed to create fsnotify watcher: too many open files``. That looks like a
   file-descriptor limit, so raising ``ulimit -n`` is the obvious fix and changes
   nothing. Worse, a kubelet that cannot open a watcher stops tracking ConfigMap
   and Secret updates, and no pod crashes to say so.

Time sync
'''''''''

The command last makes sure ``chrony`` runs. The requirement is chrony itself,
not a correct clock: the platform stack's ``otel-lgtm-stack`` chart ships a
``nodeexporter-chrony-exporter`` DaemonSet that reads
``unix:///run/chrony/chronyd.sock`` from a ``hostPath`` mount.

The command tests for that socket. A node that serves it keeps whatever it has.
A node that does not gets ``chrony`` from ``apt-get``, started at once. The
clock then converges over a few minutes.

.. warning::

   ``systemd-timesyncd`` is not enough, although it keeps the clock correct.
   Ubuntu's cloud image runs it by default and it creates no socket, so the
   exporter never becomes healthy and ArgoCD reports ``otel-lgtm-stack`` as
   degraded. Installing chrony disables ``systemd-timesyncd``, so the node ends
   with one time source rather than two.

A right clock matters on its own, because skew invalidates a TLS certificate, an
OIDC token and an etcd lease. Any NTP client would do for that. The socket is
what makes it chrony.

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
download + control-plane bootstrap). ``spur k8s up`` requires cluster admin
(``root``, or an accounting admin — see `Access control`_).

Scope the cluster to a subset of nodes
''''''''''''''''''''''''''''''''''''''

By default ``spur k8s up`` enrolls every registered node. To build a smaller
cluster, scope it with ``--nodes`` (a hostlist), ``--partition``, and/or
``--selector`` (repeatable ``key=value``, ANDed) — the three are unioned
together and resolved once, at up-time:

.. code-block:: bash

   spur k8s up --nodes "gpu[01-08]"
   spur k8s up --partition batch
   spur k8s up --selector zone=z1 --selector gpu=mi300

A scoped cluster's membership is frozen until you grow or shrink it with
``add-nodes`` / ``remove-nodes`` (see `Adding and removing worker nodes`_).

High-availability control plane
'''''''''''''''''''''''''''''''''

By default the cluster runs a single control-plane node (``control_plane_node``
in ``spur.conf``, or the first node). For HA, request 3 or 5 control planes:

.. code-block:: bash

   spur k8s up --replicas 3                                    # lowest-named 3 nodes
   spur k8s up --control-plane-nodes cp-1,cp-2,cp-3             # explicit set; overrides --replicas

``--control-plane-nodes`` (or a single ``--control-plane-node``) always wins
over ``--replicas``. The first control-plane node is the etcd bootstrap.

.. important::

   Every control-plane node — whether picked automatically, named with
   ``--control-plane-node``, or listed in ``--control-plane-nodes`` — **must be
   part of the cluster's node scope**. If you also pass ``--nodes``,
   ``--partition``, or ``--selector``, make sure the control-plane node(s) are
   included in that selection; otherwise ``spur k8s up`` is rejected with
   ``control-plane node <name> is not a registered node`` (explicit list) or
   ``control-plane node <name> is not among the selected cluster nodes``
   (auto-picked). Leave ``--nodes``/``--partition``/``--selector`` unset to
   scope the cluster to the whole inventory, which trivially satisfies this —
   any registered node is then a valid control-plane choice.

On a multi-control-plane mesh cluster there is no floating VIP — a VRRP address
cannot follow WireGuard cryptokey routing — so ``spur k8s up`` enables k0s
node-local load balancing (``nodeLocalLoadBalancing`` with ``EnvoyProxy``) on
every control plane. This gives each node a local Envoy that round-robins across
all controllers, so konnectivity has a cluster-wide ``:8132`` endpoint instead of
pinning every agent to a single controller. It is on automatically for any
multi-control-plane count (today 3 or 5) and off for a single control plane (no
balancing needed).

.. note::

   k0s does not hot-reload node-local load balancing, and Spur only writes a
   controller's ``k0s.yaml`` while bringing that controller up — an
   already-active control plane is never rewritten in place. A fresh
   ``spur k8s up --replicas 3`` is therefore unaffected (controllers render the
   setting before any worker joins), but an **existing** HA cluster cannot pick
   up the fix by restarting workers alone: reprovision the control plane with
   ``spur k8s down --reset`` followed by ``spur k8s up``.

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

**kuberouter** (default) — the built-in k0s CNI. The control-plane API is advertised
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

Storage
~~~~~~~

k0s bundles no storage, so a plain cluster has no ``StorageClass`` and any
``PersistentVolumeClaim`` stays ``Pending``. By default Spur ships the
`local-path-provisioner <https://github.com/rancher/local-path-provisioner>`_
(``storage_provisioner = "local-path"``) as the cluster's **default**
StorageClass — RWO, node-local — so PVC workloads bind out of the box. The
control-plane agent writes the manifest into the k0s manifest-deployer directory,
which k0s applies automatically (no in-cluster client).

Local-path stores volumes under ``local_path_dir`` (default
``/var/lib/local-path-provisioner``, on the root filesystem). If PVCs will hold
much data — model caches, datasets — point it at a large scratch disk:

.. code-block:: ini

   [cluster]
   local_path_dir = "/mnt/scratch/local-path"

Set ``storage_provisioner = "none"`` to bring your own storage.

Adding and removing worker nodes
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

How a worker joins depends on how the cluster was scoped at ``spur k8s up``:

**Whole-inventory cluster** (``spur k8s up`` with no scope flags) — every
registered node is a member. Start ``spurd`` on the new node (registered to the
same controller) and, if using the mesh CNI, join it to the mesh first. On the
next reconcile tick it is assigned a role + mesh IP + pod CIDR and joins
automatically. No further command is needed.

**Scoped cluster** (``spur k8s up --nodes/--partition/--selector``) — membership
is frozen at up-time, so a newly-registered node stays *outside* the cluster until
you add it explicitly. Grow the cluster online, no ``down``/``--reset`` needed:

.. code-block:: bash

   spur k8s add-nodes --nodes gpu[09-12]        # or --partition <p> / --selector k=v
   spur k8s status                              # the new workers converge to active

Added nodes are workers; they are unioned into the member set and enrolled by the
reconcile loop exactly as an in-scope node is. Adding a node already in the
cluster is a no-op.

Remove a worker gracefully — cordon, drain (evict pods, PDB-aware), then stop and
``k0s reset`` the node:

.. code-block:: bash

   spur k8s remove-nodes --nodes gpu12
   spur k8s remove-nodes --nodes gpu12 --drain-timeout 180
   spur k8s remove-nodes --nodes gpu12 --force   # proceed past running jobs / a blocked drain

.. warning::

   ``remove-nodes`` is **destructive**: it runs ``k0s reset`` on the departing
   node, wiping its k0s state (etcd/kine data, pulled images, containerd state,
   certs). Re-adding the same node later re-downloads k0s and re-seeds state
   (~262 MB plus an image re-pull). For a temporary "stop scheduling here", use
   ``spur node drain`` (the SPUR-scheduling layer) instead — it does not touch k0s.

   ``--force`` only skips the running-jobs guard (the jobs keep running) and lets
   the drain proceed past a ``PodDisruptionBudget`` or its timeout — it can evict
   pods that would otherwise block. ``remove-nodes`` refuses a control-plane node,
   the last remaining worker (would empty the member set), and any node not in a
   scoped cluster.

``spur k8s remove-nodes`` is distinct from ``spur node remove``: the former is the
graceful, k0s-aware path for shrinking a running cluster (drain + reset); the
latter is inventory-only (it does not drain pods or stop k0s) and is for
decommissioning a host from SPUR entirely. Use ``k8s remove-nodes`` first, then
``node remove`` if the host is also leaving SPUR.

Install the platform stack
~~~~~~~~~~~~~~~~~~~~~~~~~~

``spur k8s up`` gives you Kubernetes and nothing on top of it. ``silo install``
deploys the cluster-forge platform stack (ArgoCD, Gitea, OpenBao, and the AI/ML
tooling) onto it.

**It brings the cluster up itself.** A cluster that is not ready yet is not an
error: the command requests the same bring-up that ``spur k8s up`` does, then
waits for the cluster to reach ready before it installs anything. On a node that
is registered and configured, this is the only command you run.

**It assumes the node is configured, and prepares nothing.** How a node gets its
disk, its firewall rules and its kernel limits is the operator's business, and
the command neither performs that work nor requires any particular tool to have
done it.

It does run a read-only sanity check first. The check names what looks wrong and
the install continues, because each of these fails late and as something else: an
unraised ``inotify`` limit as a kubelet that quietly stops tracking ConfigMap
updates, a full disk as an image pull that never finishes. The check covers:

.. list-table::
   :header-rows: 1
   :widths: 32 68

   * - Check
     - Reported when
   * - Free space
     - ``/var/lib/k0s`` holds less than 150 GB. One install of size ``medium``
       used 141 GB.
   * - Ports
     - The ``INPUT`` chain does not accept the TCP or UDP ports k0s needs.
   * - Kernel limits
     - An ``inotify`` limit sits below the floor in `Kernel limits`_.
   * - chrony
     - Nothing serves ``/run/chrony/chronyd.sock``, which the platform
       stack's chrony exporter reads.

The check changes nothing and stops nothing. Which device holds the data
directory is not checked: keeping it on the root filesystem is a supported
choice, so only the free space on it matters.

**It needs ``git`` on the node**, which is what clones cluster-forge. The
command stops at once when ``git`` is absent, rather than minutes in, at the
clone. Pass ``--release`` as a release archive URL to download the sources over
HTTP instead, which needs no ``git``.

The command needs two more things that ``spur k8s up`` does not set up. Prepare
both before the first run:

* **A cluster-admin kubeconfig for root.** The command runs ``kubectl`` as root
  and reads no ``KUBECONFIG`` variable. Set ``[cluster]
  allow_admin_kubeconfig = true`` in ``spur.conf``, or write
  ``/root/.kube/config`` yourself.
* **A TLS certificate for the domain.** ``--cert-option`` defaults to
  ``existing``, which needs ``--tls-cert`` and ``--tls-key``. Pass
  ``--cert-option generate`` to build a self-signed pair instead. The command
  builds that pair itself and needs no certificate tool on the node. The pair
  lasts 365 days and the key file is created ``0600``.

.. code-block:: bash

   # a pre-staged certificate (the default)
   sudo spur k8s silo install --domain cf.example.com \
       --tls-cert /etc/spur/cf.example.com.crt \
       --tls-key /etc/spur/cf.example.com.key

   # a self-signed certificate, for a test cluster
   sudo spur k8s silo install --domain cf.example.com --cert-option generate

   # choose a release and a size
   sudo spur k8s silo install --domain cf.example.com --cert-option generate \
       --release v2.2.2 --size large

   # reinstall over an installed stack
   sudo spur k8s silo install --domain cf.example.com --cert-option generate --force

``--release`` takes a tag, a branch, or a release archive URL. An empty value
takes the release SPUR pins, so two installs a month apart deploy the same
platform stack.

**This runs locally, so it is not usable from a workstation.** The command needs
root, a cluster that can pull images, and network access to the cluster-forge
repository. It needs no other tool on the node: it installs no Helm and no
``kubectl``, and it runs no external deployer.

The cluster must already be ``ready``; the command refuses to run otherwise. A
second run is a no-op once the stack is installed, unless you pass ``--force``.

How the bootstrap works
'''''''''''''''''''''''

cluster-forge delivers itself through ArgoCD, so the install is a bootstrap.
``silo install`` puts ArgoCD, OpenBao and Gitea on the cluster by hand, and then
creates the one ArgoCD Application that owns everything after that, the three
bootstrapped components included.

The order is forced, not chosen. ArgoCD comes first because everything after it
is an ArgoCD Application. OpenBao comes next, because the Gitea init job reads
the OpenBao root token, waits on the OpenBao service, and pulls its own user
password from an OpenBao path. Gitea comes last of the three.

Every stage renders a Helm chart, and **SPUR renders the charts in the cluster
rather than on the node**. It starts a short-lived pod on ArgoCD's own image,
sends each chart in over ``kubectl exec``, runs ``helm template`` there, and
applies the result from the node. The pod gets no ServiceAccount token, and the
command deletes it when the install ends.

That pod carries the same Helm that ArgoCD reconciles these charts with
afterwards. Rendering the bootstrap with any other Helm renders each component
twice, by two versions that can disagree. The image comes from the ArgoCD
chart's own ``appVersion``, so there is no second version to keep in step, and
the cluster pulls that image anyway.

The bootstrap ends by creating the ``cluster-forge`` Application, which points
ArgoCD at the repositories Gitea now holds. ArgoCD then adopts ArgoCD, OpenBao
and Gitea, and deploys the rest of the stack over the next several minutes.
Nothing waits for that.

Every stage probes for its own result first, so an install that fails part way
resumes instead of repeating stages that take minutes.

What ``silo install`` does around the bootstrap
''''''''''''''''''''''''''''''''''''''''''''''''

cluster-forge expects the cluster RKE2 builds, which prepares itself through
files under ``/etc/rancher`` and ``/var/lib/rancher`` that k0s never reads.
``silo install`` therefore does that preparation itself, through the Kubernetes
API. Each step is idempotent, so a re-run changes nothing.

.. list-table::
   :header-rows: 1
   :widths: 30 70

   * - Step
     - Reason
   * - Stage ``/root/.kube/config``
     - The command runs ``kubectl`` as root and reads no ``KUBECONFIG``
       variable. An existing file is never overwritten.
   * - Alias the cluster-forge StorageClasses
     - cluster-forge asks for ``default``, ``mlstorage``, ``direct`` and
       ``multinode``. Each missing name is created on the default provisioner.
   * - Create the ``cluster-tls`` secret
     - The gateway's ``https`` listener names it. Without it the listener
       reports ``InvalidCertificateRef``.
   * - Register the ``HelmChartConfig`` kind
     - An RKE2-only CRD. ArgoCD caches the API resource list at startup, so the
       kind is registered before the bootstrap runs.
   * - Label one node ``cluster-bloom/first-node=true``
     - cluster-forge pins the Envoy proxy pods to it, and names this label. RKE2
       sets it through the node config, which k0s does not use.
   * - Create a MetalLB address pool
     - Runs after the bootstrap, because the CRD arrives with the platform
       stack. The pool holds the address of the node that runs the gateway.
       Only a Kubernetes node can answer for the address, and the k0s control
       plane node carries no kubelet.
   * - Recreate an empty unrecoverable database
     - See the note below.

.. note::

   cluster-forge registers the Kyverno admission webhook with
   ``failurePolicy: Fail`` before the Kyverno pod serves, and the k0s API server
   reaches that webhook through konnectivity. A PVC created inside that window
   is rejected. CloudNativePG records the instance serial before it creates the
   PVC, so the database then refuses to start and asks for a restore from
   backup. ``silo install`` watches for five minutes and recreates any such
   database that has no volume, because a database with no volume holds no data
   and ArgoCD self-heals it within about two minutes. A database that does have
   a volume is reported, not touched.

.. list-table::
   :header-rows: 1
   :widths: 15 85

   * - Size
     - Behaviour
   * - ``small``
     - Builds no ``cluster-values`` repository, so it **cannot disable an
       application** and leaves ``global.domain`` empty. Not the default.
   * - ``medium``
     - The default.
   * - ``large``
     - For a cluster with capacity to spare.

Read the state back with ``spur k8s status``, which grows a ``silo:`` line once
the stack has been installed:

.. code-block:: text

   phase: ready
   control-plane: cp-1
   silo: installed release=v1.2.3 size=medium domain=cf.example.com

A cluster that never ran ``silo install`` prints no ``silo:`` line at all.

Tear down
~~~~~~~~~

.. code-block:: bash

   spur k8s down            # stop + disable the k0s unit on every node
   spur k8s down --reset    # also `k0s reset` (destructive: wipes cluster state)

``--reset`` removes ``/var/lib/k0s`` on every node, along with the spurd-owned
systemd unit and cached join token, but leaves the WireGuard mesh (``spur0``)
intact. Purging the join token matters: a token minted against the torn-down
cluster's CA would fail the next join with a ``kubernetes-ca`` verification
error. To switch the CNI, tear down with ``--reset`` and bring the cluster back
up with the new ``cni`` setting.

For Users
---------

Users do not need Spur access — they interact with the cluster through the
standard Kubernetes tooling.

Get a kubeconfig
~~~~~~~~~~~~~~~~~

.. code-block:: bash

   spur k8s kubeconfig > mine.conf
   export KUBECONFIG=$PWD/mine.conf
   kubectl get nodes

A bare ``spur k8s kubeconfig`` mints a ServiceAccount kubeconfig scoped to
**your own** SPUR user/account namespace — no admin access required. It prints
to stdout so it can be redirected to a file.

Access control
'''''''''''''''

.. list-table::
   :header-rows: 1
   :widths: 30 20 50

   * - Command
     - Who
     - Result
   * - ``spur k8s kubeconfig``
     - anyone
     - Own scoped kubeconfig (namespace = own SPUR account).
   * - ``spur k8s kubeconfig --user <name>``
     - cluster admin
     - ``<name>``'s scoped kubeconfig. Requesting anyone but yourself needs admin.
   * - ``spur k8s kubeconfig --admin``
     - cluster admin
     - The k0s cluster-admin kubeconfig (full access). Mutually exclusive with ``--user``.

"Cluster admin" is ``root``, or a user the accounting layer marks as an admin
association (see :doc:`../admin-guide/accounting`). ``--admin`` additionally
requires ``[cluster] allow_admin_kubeconfig = true`` in ``spur.conf`` — it is
``false`` by default, since the admin check on ``caller`` is not yet backed by
authenticated identity. With it off, get the cluster-admin kubeconfig directly
on the control-plane node instead: ``k0s kubeconfig admin``.

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
   * - ``spur k8s up [--nodes <hostlist>] [--partition <p>] [--selector k=v] [--control-plane-node <h> | --control-plane-nodes <h1,h2,h3>] [--replicas 1|3|5]``
     - Provision + start the cluster (idempotent). Admin only. Control-plane
       node(s) must lie within the ``--nodes``/``--partition``/``--selector``
       scope (or leave that scope empty for the whole inventory).
   * - ``spur k8s add-nodes --nodes <hostlist> | --partition <p> | --selector k=v``
     - Add worker nodes to a running scoped cluster (no down/reset). Admin only.
   * - ``spur k8s remove-nodes --nodes <hostlist> [--drain-timeout <secs>] [--force]``
     - Drain + ``k0s reset`` + remove a worker (destructive; re-add re-seeds state). Admin only.
   * - ``spur k8s status``
     - Cluster phase + per-node component state.
   * - ``spur k8s kubeconfig [--user <name>] [--admin]``
     - Print a kubeconfig (redirect to a file). Bare = own scope; ``--user``/``--admin`` need admin.
   * - ``spur k8s down [--reset]``
     - Stop the cluster; ``--reset`` also wipes k0s state. Admin only.
   * - ``spur k8s silo prepare-node [--data-disk <dev>] [--force-format] [--dry-run]``
     - Make this node ready to run k0s (local; run as root, before the node first runs k0s). Mounts ``--data-disk`` at ``/var/lib/k0s``, opens the k0s ports with ``iptables``, raises the ``inotify`` limits, and installs ``chrony`` when nothing serves its socket.
   * - ``spur k8s install-k0s [--version <tag>|latest] [--path <p>] [--force]``
     - Install the k0s binary on this node (local; run as root).
   * - ``spur k8s silo install --domain <d> [--release <r>] [--size small|medium|large] [--cert-option existing|generate] [--tls-cert <p>] [--tls-key <p>] [--force]``
     - Deploy the cluster-forge platform stack (local; needs root). Brings the cluster up first when it is not ready, and reports what ``silo prepare-node`` has not done. ``--release`` takes a tag, a branch, or a release archive URL, and defaults to the pinned release. ``--cert-option existing`` is the default and needs ``--tls-cert`` and ``--tls-key``. Admin only.

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
     - Enable Spur-managed k0s. When off, spurd never touches systemd/k0s.
   * - ``distro``
     - ``k0s``
     - Kubernetes distribution SPUR manages. Only ``k0s`` is supported today.
   * - ``control_plane_node``
     - (first node)
     - Hostname of the k0s control plane.
   * - ``control_plane_replicas``
     - ``1``
     - HA control-plane count (1, 3, or 5). Overridden per-invocation by ``spur k8s up --replicas``.
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
   * - ``storage_provisioner``
     - ``local-path``
     - Storage Spur ships as the default StorageClass (``local-path`` or ``none``).
   * - ``local_path_dir``
     - ``/var/lib/local-path-provisioner``
     - On-node directory local-path stores PVs in; point at a big disk for data-heavy PVCs.
   * - ``k0s_version``
     - pinned
     - k0s release to install/run (a tag or ``latest``).
   * - ``k0s_binary``
     - ``/usr/local/bin/k0s``
     - Install path for the k0s binary.
   * - ``k8s_provisioning_timeout_secs``
     - ``600``
     - Seconds a node may stay non-``active`` during provisioning before the cluster is marked ``degraded``.
   * - ``allow_admin_kubeconfig``
     - ``false``
     - Allow ``spur k8s kubeconfig --admin`` to serve the cluster-admin kubeconfig over RPC.
