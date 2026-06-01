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

The native-host E2E suite lives in ``tests/e2e/`` and uses pytest. It SSHes into pre-provisioned nodes, deploys Spur, runs tests, and tears down the cluster after each test. Build the release binaries first (``cargo build --release``).

Prerequisites
~~~~~~~~~~~~~

- Python 3.11+ with ``pip install -r tests/e2e/requirements.txt``
- Pre-provisioned nodes accessible via SSH (password or key)
- Container tests require ``squashfs-tools`` on the runner and all nodes
- GPU tests require GPU hardware (ROCm/CUDA) on the nodes, plus a Python venv with PyTorch (auto-provisioned if ``SPUR_TEST_GPU_VENV`` is unset)

Environment Variables
~~~~~~~~~~~~~~~~~~~~~

.. list-table::
   :header-rows: 1

   * - Variable
     - Description
     - Example
   * - ``SPUR_TEST_NODES`` *(required)*
     - Comma-separated list of node IPs/hostnames. First node becomes the controller.
     - ``10.0.1.10,10.0.1.11,10.0.1.12``
   * - ``SPUR_TEST_SSH_USER`` *(required)*
     - SSH username for all nodes.
     - ``vm``
   * - ``SPUR_TEST_SSH_PASSWORD`` *(optional)*
     - SSH password. One of ``SPUR_TEST_SSH_PASSWORD`` or ``SPUR_TEST_SSH_KEY`` must be set.
     - ``vm``
   * - ``SPUR_TEST_SSH_KEY`` *(optional)*
     - Path to SSH private key (alternative to password). One of ``SPUR_TEST_SSH_PASSWORD`` or ``SPUR_TEST_SSH_KEY`` must be set.
     - ``~/.ssh/id_ed25519``
   * - ``SPUR_TEST_BINARIES_DIR`` *(optional)*
     - Path to release binaries (local). Defaults to ``target/release``.
     - ``./target/release``
   * - ``SPUR_TEST_REMOTE_BIN_DIR`` *(optional)*
     - Fixed remote path for binaries on nodes. If set, not cleaned up (useful for CI + AppArmor). If unset, an ephemeral temp path is used and cleaned up after the session.
     - ``/tmp/spur-e2e-bin``
   * - ``SPUR_TEST_CONTROLLER_PORT`` *(optional)*
     - Port for spurctld. Defaults to ``6817``.
     - ``6817``
   * - ``SPUR_TEST_AGENT_PORT`` *(optional)*
     - Port for spurd. Defaults to ``6818``.
     - ``6818``
   * - ``SPUR_TEST_GPU_VENV`` *(optional)*
     - Path to a pre-existing Python venv (on nodes) with PyTorch. If unset, the GPU tests provision a fresh venv automatically.
     - ``/opt/gpu-venv``
   * - ``SPUR_TEST_TORCH_INDEX`` *(optional)*
     - PyPI index URL for installing PyTorch (used when auto-provisioning the venv). Defaults to ``https://download.pytorch.org/whl/rocm6.3``.
     - ``https://download.pytorch.org/whl/cu124``

Node Setup
~~~~~~~~~~

**AppArmor (Ubuntu 24.04+)** — Container tests need unprivileged user namespaces, which AppArmor restricts by default. The recommended approach is to set ``SPUR_TEST_REMOTE_BIN_DIR`` to a fixed path and provision an AppArmor profile for ``spurd``:

.. code-block:: bash

   export SPUR_TEST_REMOTE_BIN_DIR=/tmp/spur-e2e-bin

   # Provision AppArmor profile on each node:
   IFS=',' read -ra NODES <<< "$SPUR_TEST_NODES"
   SPURD_PATH="${SPUR_TEST_REMOTE_BIN_DIR}/spurd"

   PROFILE="abi <abi/4.0>,
   profile spur-e2e ${SPURD_PATH} flags=(unconfined) {
     userns,
   }"

   for node in "${NODES[@]}"; do
     ssh "${SPUR_TEST_SSH_USER:-vm}@${node}" \
       "echo '${PROFILE}' | sudo apparmor_parser -r"
   done

In CI, set ``SPUR_TEST_REMOTE_BIN_DIR`` to a fixed path and provision the profile once during node setup. The directory is not cleaned up when this variable is set, so binaries persist between runs.

If you do **not** set ``SPUR_TEST_REMOTE_BIN_DIR``, binaries go into an ephemeral temp path (cleaned up after each pytest session). Since the path is unpredictable, you cannot provision an AppArmor profile for it. In that case, disable the restriction on test nodes instead:

.. code-block:: bash

   # On each node (persists across reboots):
   sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
   echo kernel.apparmor_restrict_unprivileged_userns=0 | sudo tee /etc/sysctl.d/99-spur-userns.conf

Running the Tests
~~~~~~~~~~~~~~~~~

.. code-block:: bash

   export SPUR_TEST_NODES=10.0.1.10,10.0.1.11,10.0.1.12

   # Run the full suite
   pytest tests/e2e/ -v

   # Run a specific test
   pytest tests/e2e/test_single_node.py::TestJobLifecycle::test_job_cancel -v

Tests that require more nodes than provided, or missing GPU/container prerequisites, are automatically skipped.

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

