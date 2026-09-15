Extending the CLI with Plugins
==============================

A plugin adds a command to ``spur`` without a change to Spur itself. When
``spur <name>`` is not a built-in command, Spur looks on ``PATH`` for an
executable named ``spur-<name>`` and runs it. This is the same mechanism
kubectl uses, and Spur reads no metadata from a plugin: any executable file
whose name starts with ``spur-`` is one.

Writing a Plugin
----------------

Name the executable ``spur-<name>``, make it executable, and put it on
``PATH``:

.. code-block:: bash

   cat > /usr/local/bin/spur-hello <<'EOF'
   #!/usr/bin/env bash
   echo "hello from $SPUR_PLUGIN_NAME, controller $SPUR_CONTROLLER_ADDR"
   EOF
   chmod +x /usr/local/bin/spur-hello

   spur hello world      # runs: spur-hello world

Resolution
----------

A built-in command always wins. ``spur queue`` runs the built-in job queue even
when a ``spur-queue`` executable exists on ``PATH``; such a plugin never runs
and is reported by ``spur plugin list``.

When no built-in command matches, Spur takes the longest executable that
exists, so a plugin can own a whole command tree:

.. list-table::
   :header-rows: 1

   * - Command
     - Executable tried, longest first
     - Arguments the plugin gets
   * - ``spur silo install demo``
     - ``spur-silo-install-demo``, ``spur-silo-install``, ``spur-silo``
     - the part of the command that is left over

Only the directories on ``PATH`` are searched, and the first directory that
holds a match wins. An underscore in the file name stands for a dash in the
command name, so ``spur-my_tool`` answers to ``spur my-tool``.

Spur replaces its own process with the plugin, so the exit code and the signal
handling of the plugin are the ones you see.

Environment
-----------

Spur exports these variables before it runs a plugin:

.. list-table::
   :header-rows: 1

   * - Variable
     - Value
   * - ``SPUR_CONTROLLER_ADDR``
     - Controller endpoints, from ``spur.conf`` when the variable is not
       already set
   * - ``SPUR_CONF``
     - Path of the configuration file in use
   * - ``SPUR_BIN``
     - Absolute path of the running ``spur`` binary
   * - ``SPUR_VERSION``
     - Version string of that binary
   * - ``SPUR_PLUGIN_NAME``
     - Resolved plugin name, for example ``silo``

No user identity and no token are exported. A plugin that needs cluster data
asks Spur for it, for example with ``$SPUR_BIN k8s kubeconfig --admin``.

Listing Plugins
---------------

.. code-block:: bash

   spur plugin list

It prints every ``spur-*`` executable on ``PATH`` with its path, and marks a
plugin that shadows a built-in command. ``spur help`` also lists the plugin
names it finds.
