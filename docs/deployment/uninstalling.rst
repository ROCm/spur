Uninstalling Spur
=================

This page covers removing Spur from a cluster or a single host: tearing down daemons with
Ansible, uninstalling by hand, and choosing what state to keep or destroy. Read the data
implications before wiping anything — a full wipe resets the Raft job-id counter and
orphans accounting history.

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

Remove binaries and symlinks
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

If Spur was installed via ``install.sh``, its built-in uninstaller removes the core
binaries and its own symlink set:

.. code-block:: bash

   curl -fsSL https://raw.githubusercontent.com/ROCm/spur/main/install.sh | bash -s -- uninstall

To uninstall from a custom directory, set ``INSTALL_DIR``:

.. code-block:: bash

   INSTALL_DIR=/opt/spur/bin curl -fsSL https://raw.githubusercontent.com/ROCm/spur/main/install.sh | bash -s -- uninstall

The ``install.sh`` uninstaller removes only the binaries ``spur spurctld spurd`` and the
symlinks ``sbatch srun squeue scancel sinfo sacct scontrol``. It does **not** remove the
extra symlinks the Ansible installer adds. If Ansible installed Spur, remove the full set
by hand:

.. code-block:: bash

   cd /root/.local/bin
   rm -f spur spurctld spurd \
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

.. warning::

   Wiping Raft state resets the job-id counter and orphans old ``sacct`` correlation. To
   preserve your data across a teardown and reinstall, run plain teardown (no ``wipe``),
   keep ``spur_home`` and the PostgreSQL database, and redeploy with the default
   ``spur_wipe_state=false``.

See Also
--------

- :doc:`ansible`
- :doc:`upgrading`
