Deployment
==========

Guides for deploying Spur in production across multiple nodes. The Ansible
toolkit is the recommended path; the manual guide covers the same steps by hand.

- **Install** — :doc:`ansible` (recommended) or :doc:`native-host` (manual systemd)
- **MPI** — :ref:`PMIx setup <mpi-pmix-install>` and :ref:`mpirun setup <mpirun-setup>` (agent prerequisites)
- **Kubernetes** — :doc:`kubernetes` and :doc:`managed-kubernetes`
- **Configure** — :doc:`partitioning`
- **Scale the controllers** — :doc:`controller-ha`
- **Operate** — :doc:`upgrading` and :doc:`uninstalling`
