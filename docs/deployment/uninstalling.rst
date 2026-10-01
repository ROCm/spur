Uninstalling Spur
=================

This page covers removing Spur from a cluster or a single host: tearing down daemons with
Ansible, uninstalling by hand, and choosing what state to keep or destroy. Read the data
implications before wiping anything — a full wipe resets the Raft job-id counter and
orphans accounting history.

If you started a Kubernetes cluster with ``spur k8s up``, remove that cluster first, while
the Spur daemons still run. See :ref:`uninstall-k0s`.

.. _uninstall-k0s:

Remove the Spur-Managed k0s Cluster
-----------------------------------

Do this step only if you ran ``spur k8s up``. Do it **before** you stop the Spur daemons.
``spurd`` on each node stops and resets k0s. If ``spurd`` is already stopped, nothing does
this work, and the k0s components (kubelet, kube-proxy, the CNI and the containerd shims)
continue to run. Use :ref:`uninstall-k0s-manual` in that case.

Remove the workloads (optional)
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

Remove the applications that you installed in the cluster, for example with
``helm uninstall``. This step is optional, because the reset below deletes all cluster data.

Reset the cluster
~~~~~~~~~~~~~~~~~

Run as a cluster admin, with ``spurctld`` and ``spurd`` running on all nodes:

.. code-block:: bash

   spur k8s status          # phase: ready
   spur k8s down --reset

The command does not wait for the nodes. It prints ``k0s cluster teardown requested``, and
``spur k8s status`` shows ``phase: down`` immediately. Each ``spurd`` then stops the k0s
unit, runs ``k0s reset``, deletes the unit file (``k0scontroller.service`` or
``k0sworker.service``) and the join token ``/etc/k0s/token``, and runs
``systemctl daemon-reload``. It also deletes the CDI spec ``/etc/cdi/amd.json``. When a
node is done, its ``spurd`` log shows ``k0s component stopped reset=true``.

On each node, make sure that the reset is complete before you continue:

.. code-block:: bash

   systemctl is-active k0scontroller k0sworker   # inactive
   pgrep -af "^/var/lib/k0s/bin/"                 # no output
   ls /var/lib/k0s                                # No such file or directory

Remove what the reset keeps
~~~~~~~~~~~~~~~~~~~~~~~~~~~

``k0s reset`` removes the cluster state in ``/var/lib/k0s``. It does not remove these
items. Remove them on each node:

.. list-table::
   :header-rows: 1

   * - Item
     - Made by
   * - k0s binary ``/usr/local/bin/k0s`` (``[cluster].k0s_binary``)
     - ``spur k8s install-k0s`` or ``spurd``
   * - ``/etc/k0s`` (``k0s.yaml``, ``containerd.toml``, ``containerd.d``)
     - ``spurd``
   * - PersistentVolume data in ``/var/lib/local-path-provisioner``
       (``[cluster].local_path_dir``)
     - local-path provisioner
   * - ``/var/lib/kubelet``, ``/run/k0s``, ``/var/run/cdi``
     - kubelet, k0s, DRA drivers
   * - Additional CDI specs ``/etc/cdi/amd-<N>.json``
     - ``spurd`` (hosts with more than one GPU spec)
   * - ``/etc/cni/net.d``, ``/opt/cni/bin``
     - k0s CNI
   * - iptables rules (``KUBE-*`` chains, and ``cali-*`` chains with Calico) and the
       ``kube-router-*`` ipsets
     - kube-proxy, kube-router, Calico
   * - kubeconfig files from ``spur k8s kubeconfig``, and ``kubectl`` wrapper scripts that
       call ``k0s kubectl``
     - you

.. code-block:: bash

   sudo rm -f /usr/local/bin/k0s
   sudo rm -rf /etc/k0s /var/lib/kubelet /run/k0s /var/run/cdi
   sudo rm -rf /var/lib/local-path-provisioner     # deletes all PVC data
   sudo rm -f /etc/cdi/amd-*.json
   sudo rmdir /etc/cdi 2>/dev/null || true

Remove the CNI directories only if no other software on the host uses CNI:

.. code-block:: bash

   sudo rm -rf /etc/cni/net.d /opt/cni/bin

To remove the iptables rules and ipsets, reboot the host. ``k0s reset`` also recommends a
reboot. A reboot is the safest method, because it does not touch the rules of other
software.

If you cannot reboot, first look at the rules that do not belong to Kubernetes.
kube-router also adds rules without ``KUBE`` in the name: ``FORWARD`` rules with the
comments ``allow inbound traffic to pods``, ``allow outbound traffic from pods`` and
``allow outbound node port traffic ...``, and a ``MASQUERADE`` rule that matches the
``kube-router-*`` ipsets. The filter below removes them too:

.. code-block:: bash

   k8s='KUBE|cali-|kube-router|kube-bridge|node port traffic'
   sudo iptables-save | grep -vE "$k8s"
   sudo ip6tables-save | grep -vE "$k8s"

If no other software (for example Docker or a host firewall in iptables) has rules there,
flush all tables and destroy the ipsets. The ``ipset`` command is not always installed
(for example on Ubuntu 24.04). If it is missing, install it first, for example with
``sudo apt-get install ipset``:

.. code-block:: bash

   for t in filter nat mangle raw; do
     sudo iptables -t "$t" -F;  sudo iptables -t "$t" -X
     sudo ip6tables -t "$t" -F; sudo ip6tables -t "$t" -X
   done
   sudo ipset list -n | grep '^kube-router' | xargs -r -n1 sudo ipset destroy

A flush does not touch rules in a separate nftables table, for example ``table inet``
rules of a host firewall.

.. _uninstall-k0s-manual:

If the Spur daemons are already stopped
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

Reset k0s by hand on each node. Do this only while ``spurd`` is stopped. A running
``spurd`` writes the unit again and restarts k0s. ``spurd`` also enables the unit with
``Restart=always``, so k0s starts again at the next boot until you reset it.

.. code-block:: bash

   sudo systemctl disable --now k0scontroller k0sworker
   sudo k0s reset
   sudo rm -f /etc/systemd/system/k0scontroller.service \
     /etc/systemd/system/k0sworker.service /etc/k0s/token /etc/cdi/amd.json
   sudo systemctl daemon-reload

Then do the steps in "Remove what the reset keeps".

Ansible Teardown
----------------

``teardown.yml`` stops and disables the Spur daemons across the cluster. By default it
leaves binaries, on-disk state, systemd unit files, and PostgreSQL in place:

.. code-block:: bash

   ansible-playbook playbooks/teardown.yml -i inventory/hosts.ini

Plain teardown stops and disables the ``spurctld`` and ``spurd`` services, reaps any stray
daemons started outside systemd, and, when ``spur_transport=wireguard``, brings the
WireGuard interface down. It does **not** delete
the ``*.service`` unit files — those are only stopped and disabled — and it does not remove
binaries or the accounting database.

To also remove on-disk state, pass ``-e wipe=true``:

.. code-block:: bash

   ansible-playbook playbooks/teardown.yml -i inventory/hosts.ini -e wipe=true

A wipe additionally removes ``spur_home`` (default ``/root/spur``) — the entire state
directory, containing the Raft log and job queue, node registrations, logs, ``spur.conf``,
and job output files — and deletes the WireGuard config at
``/etc/wireguard/<interface>.conf`` (default ``/etc/wireguard/spur0.conf``).

Neither mode touches PostgreSQL. To drop the accounting database, run this on the
accounting host (the database and role both default to ``spur``):

.. code-block:: bash

   sudo -u postgres dropdb spur
   sudo -u postgres dropuser spur

Manual Uninstall (Single Host)
------------------------------

For a host installed with ``install.sh`` or by copying binaries, remove Spur by hand in
this order.

Stop and disable the daemons
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

.. code-block:: bash

   sudo systemctl disable --now spurctld spurd
   sudo pkill -x spurctld
   sudo pkill -x spurd
   sudo pkill -x spurauthd

``spurstepd`` (the per-job supervisor) continues to run after ``spurd`` stops, because the
``spurd`` unit uses ``KillMode=process``. Make sure that no jobs run, then stop it:

.. code-block:: bash

   pgrep -ax spurstepd
   sudo pkill -x spurstepd

Use ``pkill -x`` (exact process name), not ``pkill -f <pattern>``. In ``ssh host '...'`` or
``bash -c '...'``, the pattern also matches the command line of that shell, and
``pkill -f`` stops the shell.

Remove binaries and symlinks
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

If Spur was installed via ``install.sh``, its built-in uninstaller removes the core
binaries and its own symlink set:

.. code-block:: bash

   curl -fsSL https://raw.githubusercontent.com/ROCm/spur/main/install.sh | bash -s -- uninstall

To uninstall from a custom directory, set ``INSTALL_DIR``:

.. code-block:: bash

   INSTALL_DIR=/opt/spur/bin curl -fsSL https://raw.githubusercontent.com/ROCm/spur/main/install.sh | bash -s -- uninstall

The ``install.sh`` uninstaller removes only the binaries
``spur spurctld spurd spurstepd spurauthd`` and the symlinks
``sbatch srun squeue scancel sinfo sacct scontrol``. It does **not** remove the
extra symlinks the Ansible installer adds. If Ansible installed Spur, remove the full set
by hand:

.. code-block:: bash

   cd /root/.local/bin
   rm -f spur spurctld spurd spurstepd spurauthd \
     sbatch squeue sinfo scancel sacct sacctmgr scontrol salloc srun \
     sattach scrontab sdiag smd sprio sreport sshare sstat strigger

Remove systemd unit files
~~~~~~~~~~~~~~~~~~~~~~~~~~~

The unit files are created by Ansible; ``install.sh`` does not create them. Remove them if
present:

.. code-block:: bash

   sudo rm -f /etc/systemd/system/spurctld.service /etc/systemd/system/spurd.service
   sudo systemctl daemon-reload

Remove state and config
~~~~~~~~~~~~~~~~~~~~~~~~~

Delete ``spur_home`` (default ``/root/spur``), which holds Raft and scheduler state, logs,
the config file, and job output:

.. code-block:: bash

   sudo rm -rf /root/spur

Without the Ansible layout, the controller keeps its state in ``state_dir`` (default
``/var/spool/spur``). ``spurd`` also keeps its per-job supervisor state there. Remove it:

.. code-block:: bash

   sudo rm -rf /var/spool/spur

The config file lives at either ``/etc/spur/spur.conf`` or, under the Ansible layout,
``<spur_home>/etc/spur.conf``. If you placed a system-wide config under ``/etc/spur/``,
remove that directory too:

.. code-block:: bash

   sudo rm -rf /etc/spur

If the ``[update]`` config block was used, remove the update-check cache as well:

.. code-block:: bash

   sudo rm -rf /var/cache/spur

Remove WireGuard
~~~~~~~~~~~~~~~~~

Only if the WireGuard transport was configured (``spur net init`` was run):

.. code-block:: bash

   sudo wg-quick down spur0
   sudo rm -f /etc/wireguard/spur0.conf

Drop the accounting database
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

On the accounting host only — nothing removes this automatically:

.. code-block:: bash

   sudo -u postgres dropdb spur
   sudo -u postgres dropuser spur

To remove PostgreSQL entirely as well:

.. code-block:: bash

   sudo apt-get remove --purge postgresql postgresql-contrib

.. note::

   Spur creates no dedicated ``spur`` OS user or group; the daemons run as root. The only
   ``spur`` "user" is the PostgreSQL role dropped above — there is no system account to
   delete.

What Is Destroyed vs Preserved
------------------------------

.. list-table::
   :header-rows: 1

   * - Artifact
     - Location
     - Plain teardown
     - ``-e wipe=true`` / manual ``rm -rf spur_home``
   * - Job queue, node registrations, Raft log
     - ``<spur_home>/state``
     - preserved
     - destroyed
   * - Job output files ``spur-<JOBID>.out``
     - job working dir (default under ``spur_home``)
     - preserved
     - destroyed (if under ``spur_home``)
   * - ``spur.conf``
     - ``<spur_home>/etc/spur.conf``
     - preserved
     - destroyed
   * - Accounting history (``sacct``)
     - PostgreSQL ``spur`` database
     - preserved
     - preserved (drop manually)
   * - Binaries and symlinks
     - ``spur_install_dir``
     - preserved
     - preserved (remove manually)
   * - systemd unit files
     - ``/etc/systemd/system/spur{ctld,d}.service``
     - left (disabled)
     - left (remove manually)
   * - WireGuard interface and config
     - ``spur0`` / ``/etc/wireguard/spur0.conf``
     - interface downed
     - config file removed
   * - Controller state without the Ansible layout
     - ``state_dir`` (default ``/var/spool/spur``)
     - preserved
     - preserved (remove manually)

The k0s cluster has its own life cycle. Neither teardown mode above touches it.

.. list-table::
   :header-rows: 1

   * - Artifact
     - Location
     - ``spur k8s down``
     - ``spur k8s down --reset``
   * - Cluster state: workloads, Secrets, etcd data
     - ``/var/lib/k0s``
     - preserved (unit stopped and disabled)
     - destroyed
   * - k0s systemd unit and join token
     - ``/etc/systemd/system/k0s{controller,worker}.service``, ``/etc/k0s/token``
     - left (disabled)
     - destroyed
   * - GPU CDI spec
     - ``/etc/cdi/amd.json``
     - destroyed
     - destroyed
   * - PersistentVolume data (local-path)
     - ``/var/lib/local-path-provisioner``
     - preserved
     - preserved (remove manually)
   * - k0s config and binary
     - ``/etc/k0s``, ``/usr/local/bin/k0s``
     - preserved
     - preserved (remove manually)
   * - CNI files, iptables rules, ipsets
     - ``/etc/cni/net.d``, ``/opt/cni/bin``, kernel
     - preserved
     - preserved (reboot or remove manually)

.. warning::

   Wiping Raft state resets the job-id counter and orphans old ``sacct`` correlation. To
   preserve your data across a teardown and reinstall, run plain teardown (no ``wipe``),
   keep ``spur_home`` and the PostgreSQL database, and redeploy with the default
   ``spur_wipe_state=false``.

Verify the Removal
------------------

On each host, after a reboot or an iptables flush:

.. code-block:: bash

   pgrep -ax 'spurctld|spurd|spurstepd|spurauthd'  # no output
   pgrep -af '^/var/lib/k0s/bin/'                    # no output
   systemctl list-unit-files | grep -Ei 'spur|k0s'   # no output
   command -v spur spurctld spurd spurstepd k0s      # no output
   ls -d /etc/spur /etc/k0s /var/lib/k0s /var/spool/spur   # all missing
   sudo iptables-save | grep -cE 'KUBE|cali-|kube-'  # 0

See Also
--------

- :doc:`ansible`
- :doc:`managed-kubernetes`
- :doc:`upgrading`
