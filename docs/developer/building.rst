Building
========

Prerequisites
-------------

- **Rust** — install via `rustup <https://rustup.rs>`_; the repo's ``rust-toolchain.toml`` pins the required version automatically
- **protobuf-compiler** — ``sudo apt install protobuf-compiler`` (Debian/Ubuntu) or ``sudo dnf install protobuf-compiler`` (Fedora/RHEL)
- Linux (tested on Ubuntu 22.04+, Fedora 38+)

Build
-----

.. code-block:: bash

   cargo build              # debug build
   cargo build --release    # optimized build

All crates build in one invocation — no separate steps needed.

Running Tests
-------------

.. code-block:: bash

   cargo test

All tests are self-contained. No external services needed (no database, no network, no GPU). The E2E suite below is the only one that requires actual hardware (nodes with SSH access, optionally GPUs), and it is ignored by default.

.. important::

   The E2E suites (both native-host and Kubernetes) do not support parallel test execution. Always pass ``--test-threads=1``.

End-to-End Tests (Native-Host)
------------------------------

The E2E suite deploys Spur to real nodes over SSH and runs integration tests against a live cluster. Build the release binaries first (``cargo build --release``), as the suite copies them to the remote nodes. You can also run it on a single machine by SSH-ing into itself — just ensure ``ssh localhost`` works without a password prompt and leave ``SPUR_TEST_BM_NODES`` at its default (``localhost``).

Environment Variables
~~~~~~~~~~~~~~~~~~~~~

Export these so the test process can read them:

.. list-table::
   :header-rows: 1

   * - Variable
     - Description
     - Example
   * - ``SPUR_TEST_BM_NODES`` *(optional)*
     - Comma-separated list of node hostnames or IPs. Defaults to ``localhost``.
     - ``10.0.1.10,10.0.1.11,10.0.1.12``
   * - ``SPUR_TEST_BM_SSH_USER`` *(optional)*
     - SSH username. Defaults to current user (``$USER``).
     - ``vm``
   * - ``SPUR_TEST_BM_SSH_KEY`` *(optional)*
     - Path to SSH private key. If unset, uses the default SSH agent or key.
     - ``~/.ssh/id_ed25519``
   * - ``SPUR_TEST_BM_BINARIES_DIR`` *(optional)*
     - Path to release binaries. Defaults to ``target/release``.
     - ``./target/release``
   * - ``SPUR_TEST_BM_DEPLOY_DIR`` *(optional)*
     - Path to ``deploy/native-host/`` scripts. Auto-detected from the workspace if unset.
     - ``./deploy/native-host``
   * - ``SPUR_TEST_BM_REMOTE_DIR`` *(optional)*
     - Remote working directory on nodes. Defaults to ``/tmp/spur-bm-{pid}-{timestamp}``.
     - ``/tmp/spur-e2e``
   * - ``SPUR_TEST_BM_GPU_VENV`` *(optional)*
     - Path to a pre-existing Python venv with GPU test dependencies. If unset, the test harness auto-provisions a fresh venv with PyTorch on each node.
     - ``/opt/spur-ci/gpu-venv``

Node Setup
~~~~~~~~~~

**Node packages** — Test nodes need ``python3`` (3.12+) and ``python3-venv`` for the GPU test suite's auto-provisioning. Ensure they are installed:

.. code-block:: bash

   IFS=',' read -ra NODES <<< "$SPUR_TEST_BM_NODES"
   for node in "${NODES[@]}"; do
     ssh "${SPUR_TEST_BM_SSH_USER}@${node}" \
       "sudo apt-get update -qq && sudo apt-get install -y -qq python3 python3-venv"
   done

**AppArmor (Ubuntu 24.04+)** — AppArmor restricts unprivileged user namespaces by default. Since Spur uses namespaces for job isolation, ``spurd`` needs an AppArmor profile granting ``userns`` permission. The profile requires a predictable path to the binary — set ``SPUR_TEST_BM_REMOTE_DIR`` to a fixed path, then provision:

.. code-block:: bash

   IFS=',' read -ra NODES <<< "$SPUR_TEST_BM_NODES"
   SPURD_PATH="${SPUR_TEST_BM_REMOTE_DIR}/bin/spurd"

   PROFILE="abi <abi/4.0>,
   profile spur-e2e ${SPURD_PATH} flags=(unconfined) {
     userns,
   }"

   for node in "${NODES[@]}"; do
     ssh "${SPUR_TEST_BM_SSH_USER}@${node}" \
       "echo '${PROFILE}' | sudo apparmor_parser -r"
   done

Alternatively, disable the restriction system-wide (less secure, not recommended for production):

.. code-block:: bash

   sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0

Running the Tests
~~~~~~~~~~~~~~~~~

The single-node and multi-node suites do not require GPU nodes:

.. code-block:: bash

   # Single-node tests
   cargo test -p spur-tests --lib native_host::single_node -- --ignored --test-threads=1

   # Multi-node tests
   cargo test -p spur-tests --lib native_host::multi_node -- --ignored --test-threads=1

For GPU testing there is a separate suite with its own single-node and multi-node variants — these require nodes with actual GPUs:

.. code-block:: bash

   # Single-node GPU tests
   cargo test -p spur-tests --lib native_host::gpu::single_node -- --ignored --test-threads=1

   # Multi-node GPU tests
   cargo test -p spur-tests --lib native_host::gpu::multi_node -- --ignored --test-threads=1

End-to-End Tests (Kubernetes)
-----------------------------

The K8s E2E suite deploys Spur's controller (StatefulSet), operator (Deployment), and SpurJob CRD into a Kubernetes cluster, then submits SpurJobs and verifies their lifecycle. Like the native-host suite, it is ignored by default.

You need a running Kubernetes cluster with:

1. **kubectl access** — a valid ``KUBECONFIG`` pointing to the cluster
2. **A Spur container image** available to the cluster (either in a registry or pre-loaded via ``ctr -n k8s.io images import``)
3. **RBAC permissions** to create namespaces, CRDs, StatefulSets, Deployments, and Pods

Environment Variables
~~~~~~~~~~~~~~~~~~~~~

Export these so the test process can read them:

.. list-table::
   :header-rows: 1

   * - Variable
     - Description
     - Example
   * - ``KUBECONFIG`` *(optional)*
     - Path to kubeconfig file. Defaults to ``~/.kube/config``.
     - ``/home/user/.kube/config``
   * - ``SPUR_CI_IMAGE`` *(optional)*
     - Container image for the controller and operator. Defaults to ``spur:ci``.
     - ``ghcr.io/rocm/spur:abc123``
   * - ``SPUR_TEST_NS`` *(optional)*
     - Kubernetes namespace for the test run. Defaults to ``spur-ci-{pid}-{timestamp}``. Strongly recommended to set explicitly when running locally so that the namespace is predictable and RBAC can be provisioned once.
     - ``spur-test-local``
   * - ``SPUR_DEPLOY_DIR`` *(optional)*
     - Path to ``deploy/k8s/`` manifests. Auto-detected from the workspace if unset.
     - ``./deploy/k8s``

Setup
~~~~~

**Build and load the image:**

.. code-block:: bash

   # Build the Spur container image
   docker build -f deploy/Dockerfile -t spur:ci .

   # If running a local cluster (e.g. kind):
   kind load docker-image spur:ci

   # If running on native-host K8s nodes, load via containerd:
   docker save spur:ci -o /tmp/spur-ci.tar
   # On each node:
   sudo ctr -n k8s.io images import /tmp/spur-ci.tar

**Apply RBAC** — the test suite needs cluster-wide permissions for CRDs and pod management. The default manifest binds to namespace ``spur``, so patch it to match your ``SPUR_TEST_NS``:

.. code-block:: bash

   sed "s/namespace: spur/namespace: $SPUR_TEST_NS/" deploy/k8s/rbac.yaml | kubectl apply -f -

Running the Tests
~~~~~~~~~~~~~~~~~

.. code-block:: bash

   # Single-node tests
   cargo test -p spur-tests --lib k8s::single_node -- --ignored --test-threads=1

   # Multi-node tests
   cargo test -p spur-tests --lib k8s::multi_node -- --ignored --test-threads=1

Cleanup
~~~~~~~

The test harness creates a namespace per run but does not delete it automatically. Clean up after local runs:

.. code-block:: bash

   # List test namespaces
   kubectl get ns | grep spur-ci

   # Delete a specific one
   kubectl delete ns <namespace-name>

