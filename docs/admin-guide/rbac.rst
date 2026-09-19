Roles and operators
===================

Spur binds a verified identity to one of four roles. Sites assign those roles;
they cannot invent new ones. The role set is the same whichever
authentication plugin is in use.

* **User** — submit and manage own jobs.
* **Coordinator** — reserved for a future grant hierarchy. Not assigned today.
* **Operator** — manage jobs, reservations, and accounting records. Cannot
  drain/remove nodes, change partitions, mint admission tokens, or reconfigure
  the cluster.
* **Administrator** — full control-plane tenancy.

Binding
-------

Spur checks several sources for each authenticated caller and grants the
highest role any of them yields. A caller who matches nothing is a User.

Three sources apply whether ``[auth] plugin`` is ``"jwt"`` or ``"spur"``:

``[auth] cluster_admins``
   Usernames listed in ``spur.conf`` are Administrator.

``[auth] admin_groups`` and ``[auth] operator_groups``
   Group names listed in ``spur.conf``. A caller in a listed group is
   Administrator or Operator respectively. Membership is read from the
   verifying host's own user directory (``/etc/group``, LDAP, SSSD — whatever
   NSS is configured to use there), so the controller and each agent must be
   able to resolve the caller's groups. Names are matched case-insensitively.

Accounting admin level
   The controller also grants Operator or Administrator to a user whose
   accounting record says so — ``sacctmgr modify user name=bob set
   adminlevel=Operator`` (or ``Admin``); see :doc:`accounting`. ``spurctld``
   reads these records from PostgreSQL into an in-memory cache rather than
   querying the database on every RPC. Until that cache has been populated —
   the first moments after startup, or while the database is unreachable —
   accounting is not consulted and the caller is treated as a plain User. Spur
   denies rather than guesses, so a privileged command may be refused briefly
   after a restart; retry once the controller has finished loading.

The remaining source depends on the plugin.

With ``plugin = "spur"``, the credential mint takes the caller's UID from the
kernel and never marks anyone an administrator, so there is no admin flag to
grant a role. Instead, a caller the mint verified as UID 0 (root on the local
host) becomes Administrator, but only if you opt in by setting
``allow_uid_zero_administrator = true`` under ``[auth]`` in ``spur.conf``. It
is off by default: root on a login node is not automatically root on the
cluster. JWT user tokens, including ``spur token user --admin``, are refused
on this plugin.

With ``plugin = "jwt"``, a token carrying the ``admin`` claim — minted by
``spur token user --admin`` — is Administrator. ``allow_uid_zero_administrator``
has no effect here, because a JWT only asserts a UID; nothing has verified it
against the kernel.

Agents apply the same rules, except that they have no accounting database, so
the accounting admin level never applies on a node. They use
``cluster_admins``, the two group lists, and — depending on the plugin — the
JWT ``admin`` claim or verified UID 0, when deciding who may attach to, exec
in, or stream a job.

Job list pins a non-operator to their own jobs. ``get_job`` and
``get_job_steps`` use the same pin: an identified User asking for another
tenant's job id gets ``NOT_FOUND``. Operators and Administrators see every
job in full. A caller with no verified identity is not pinned either, so they
also see every job in full; that path exists only under ``mode = "permissive"``
or ``"disabled"``. Under ``mode = "required"`` the auth layer rejects the call
before the handler.

``CancelJob`` with no verified identity and an empty ``user`` is treated as the
in-cluster daemon (the Kubernetes operator) and can cancel any job. A named
unauthenticated user is still an ordinary owner check. REST cancel always
requires a Bearer token, so it does not have this hole. On a native-host
controller, do not leave gRPC reachable under ``permissive``: set
``mode = "required"`` so an unauthenticated empty-user cancel never reaches
the handler, and restrict port 6817 at the network layer either way.

Controller-to-agent RPCs carry a separate controller identity rather than a
user credential, and are not subject to any of the above. Each token is minted
for one agent: the controller Pings that agent, then stamps
``spur/<cluster>/agent/<hostname>`` and the advertised boot epoch. A captured
token is not valid on another agent.

See :ref:`privileged-operations` in :doc:`configuration` for which operations
require which role, and :doc:`/developer/native-credential-mint` for how
native credentials are issued and verified.
