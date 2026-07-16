Kubernetes Deployment
=====================

Deploy Spur on an existing Kubernetes cluster. The controller runs as a StatefulSet with Raft consensus, and compute nodes are managed by the ``spur-k8s-operator``.

Prerequisites
-------------

- Kubernetes cluster with ``kubectl`` configured

Build and load container images:

.. code-block:: bash

   # Build
   docker build --target runtime -t spur:<tag> .

   # Load onto each node (if not using a registry)
   docker save spur:<tag> -o spur.tar
   # SCP to each node, then:
   sudo ctr -n k8s.io images import spur.tar

Components
----------

- **spurctld** — Controller. Runs as a StatefulSet with Raft consensus for high availability. Handles accounting (backed by PostgreSQL via ``accounting.database_url``) and serves the Slurm-compatible REST API on port 6820.
- **spurd** — Node agent. Runs on each compute node (DaemonSet or Deployment).
- **spur-k8s-operator** — Watches ``SpurJob`` custom resources and submits them to the controller.

Example manifests for production-style deployment live in ``examples/k8s/``.

Deploy
------

.. note::

   Before applying, review the manifests and update namespaces, image names/tags, resource limits, and storage classes to match your environment. Ensure the ``--controller`` argument in ``spurd.yaml`` includes the ``http://`` scheme (e.g. ``http://spurctld.spur.svc.cluster.local:6817``).

Apply manifests in order:

.. code-block:: bash

   kubectl apply -f examples/k8s/namespace.yaml
   kubectl apply -f examples/k8s/configmap.yaml
   kubectl apply -f examples/k8s/rbac.yaml
   kubectl apply -f examples/k8s/spurjob-crd.yaml
   kubectl apply -f examples/k8s/spurctld.yaml
   kubectl apply -f examples/k8s/spurd.yaml
   kubectl apply -f examples/k8s/operator.yaml
   kubectl apply -f examples/k8s/pdb.yaml

Configuration
-------------

The ConfigMap (``examples/k8s/configmap.yaml``) embeds ``spur.conf``:

.. code-block:: toml

   cluster_name = "spur-k8s"

   [controller]
   peers = [
     "spurctld-0.spurctld.spur.svc.cluster.local:6821",
     "spurctld-1.spurctld.spur.svc.cluster.local:6821",
     "spurctld-2.spurctld.spur.svc.cluster.local:6821",
   ]

   [scheduler]
   interval_secs = 2
   plugin = "backfill"

   [[partitions]]
   name = "default"
   state = "UP"
   default = true

Raft peers use StatefulSet DNS names. The node ID is auto-derived from each pod's
position in ``peers`` by matching the pod hostname (e.g. ``spurctld-0``) against
each entry's host part, so ``controller.node_id`` never needs to be set. Each
pod's hostname must correspond to its own ``peers`` entry.

Resolution precedence is: explicit ``controller.node_id`` -> position in
``peers`` -> hostname ordinal. The resolved id must fall within
``1..=len(peers)``; if a pod's hostname matches no entry (or matches more than
one), the controller fails fast at startup rather than joining with a wrong ID.

Adjust partition definitions and node resources to match your cluster hardware.

Submitting Jobs
---------------

Jobs are submitted as ``SpurJob`` custom resources:

.. code-block:: yaml

   apiVersion: spur.amd.com/v1alpha1
   kind: SpurJob
   metadata:
     name: training-run
   spec:
     script: |
       #!/bin/bash
       #SBATCH --job-name=train
       #SBATCH -N 2
       #SBATCH --gres=gpu:8
       torchrun --nnodes=2 train.py

Apply with ``kubectl``:

.. code-block:: bash

   kubectl apply -f job.yaml

The operator watches SpurJob resources, submits them to the controller, and updates status fields as the job progresses.

Verify
------

.. code-block:: bash

   # All pods running
   kubectl get pods -n spur

   # Controller logs (check Raft leader election)
   kubectl logs statefulset/spurctld -n spur

   # Node registration
   kubectl exec -n spur spurctld-0 -- spur nodes
