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

MPI plugin (optional)
~~~~~~~~~~~~~~~~~~~~~

Open MPI jobs use ``--mpi=pmix``, which loads ``spur_mpi_pmix.so`` on compute
nodes at runtime (``spurd`` itself does not link libpmix).

**For production clusters**, use a **release or nightly tarball** from GitHub
Releases (or your artifactory mirror). Both ship ``lib/spur/spur_mpi_pmix.so``
when built with ``BUILD_MPI_PLUGIN=1`` (see ``.github/workflows/release.yml``
and ``nightly.yml``). See :doc:`/deployment/native-host` for the full MPI deploy
and upgrade steps.

**For local development**, build the plugin when libpmix development packages are
installed (``pkg-config pmix`` must succeed):

.. code-block:: bash

   cargo build --release -p spur-mpi-pmix
   sudo install -D target/release/spur_mpi_pmix.so /usr/lib/spur/spur_mpi_pmix.so

Install the resulting ``spur_mpi_pmix.so`` on **every agent**, using the same
glibc as the running ``spurd``. If ``pkg-config pmix`` is missing on agents but
system PMIx headers exist, compile on the agent with ``gcc -fPIC -shared`` and
the system PMIx ``-I`` / ``libpmix.so`` path (see :doc:`/deployment/native-host`).

``spurd`` launches multi-rank ``--mpi=pmix`` jobs via per-rank fork and
``PMIx_server_setup_fork`` (see :doc:`/deployment/native-host`). Agents need
Open MPI libraries matching the application build; ``mpirun`` is not used for
bootstrap.

Without libpmix, the crate still builds a stub plugin that fails at load time with
an actionable error. Container/nightly images include a functional plugin via
``BUILD_MPI_PLUGIN=1`` (see ``Dockerfile`` and ``.github/workflows/nightly.yml``).

Native-host MPI E2E tests (``pytest -m mpi``) additionally require ``mpicc`` on
the test nodes and ``SPUR_TEST_MPI_PLUGIN`` (or the default plugin path under
``target/release/../lib/spur/``).

Running Tests
-------------

.. code-block:: bash

   cargo test

All tests are self-contained. No external services needed (no database, no network, no GPU). The E2E suite below is the only one that requires actual hardware (nodes with SSH access, optionally GPUs), and it is ignored by default.

.. important::

   The E2E suites do not support parallel test execution. Do not use ``pytest-xdist``.

End-to-End Tests (Native-Host)
------------------------------

The native-host E2E suite lives in ``tests/native_host/e2e/`` and uses pytest. It SSHes into pre-provisioned nodes, deploys Spur, runs tests, and tears down the cluster after each test. Build the release binaries first (``cargo build --release``).

Prerequisites
~~~~~~~~~~~~~

- Python 3.11+ with ``pip install -r tests/requirements.txt``
- Pre-provisioned nodes accessible via SSH (password, key, or ssh-agent)
- Container tests require ``squashfs-tools`` on the runner and all nodes
- GPU tests require GPU hardware on the nodes, plus a Python venv with PyTorch (auto-provisioned if ``SPUR_TEST_GPU_VENV`` is unset)
- GPU test scripts (``gpu_test.hip``, ``distributed_test.py``, ``inference_test.py``) live in ``tests/native_host/e2e/fixtures/`` and are shipped to nodes by the harness

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
     - SSH password. If neither password nor key is set, ssh-agent is used.
     - ``vm``
   * - ``SPUR_TEST_SSH_KEY`` *(optional)*
     - Path to SSH private key. If neither password nor key is set, ssh-agent is used.
     - ``~/.ssh/id_ed25519``
   * - ``SPUR_TEST_BINARIES_DIR`` *(optional)*
     - Path to release binaries on the test runner. Defaults to ``{repo}/target/release`` (repo root is derived from the test layout, not the shell working directory).
     - ``/home/user/spur/target/release``
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

Running on Localhost (single-node quickstart)
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

No separate hardware is needed. The test framework SSHes to ``localhost``; the
controller, agent, and (for accounting tests) a Postgres container all run on
the same machine. Requires Docker for accounting tests.

.. code-block:: bash

   # 1. Install dependencies
   sudo apt-get install -y openssh-server
   pip install paramiko tomli-w pytest pytest-timeout

   # 2. Generate an SSH keypair and authorise it for the current user
   ssh-keygen -t rsa -f /tmp/e2e_id_rsa -N ""
   cat /tmp/e2e_id_rsa.pub >> ~/.ssh/authorized_keys
   chmod 600 ~/.ssh/authorized_keys

   # 3. Start sshd (allow root login if running as root)
   echo "PermitRootLogin yes" | sudo tee -a /etc/ssh/sshd_config
   sudo ssh-keygen -A          # generate host keys if missing
   sudo /usr/sbin/sshd

   # 4. Build release binaries
   cargo build --release

   # 5. Run the e2e tests
   cd tests/native_host/e2e
   SPUR_TEST_NODES=localhost \
   SPUR_TEST_SSH_USER=$(whoami) \
   SPUR_TEST_SSH_KEY=/tmp/e2e_id_rsa \
   SPUR_TEST_BINARIES_DIR=$(git rev-parse --show-toplevel)/target/release \
   pytest test_preemption_qos_hierarchy.py -v

.. note::

   If running as root, add ``"auth": {"allow_root_jobs": True}`` to the
   ``cluster_config_overrides`` fixture in your test class, or jobs will be
   rejected by ``spurd`` (root job execution is off by default).

Running the Tests
~~~~~~~~~~~~~~~~~

.. code-block:: bash

   export SPUR_TEST_NODES=10.0.1.10,10.0.1.11,10.0.1.12

   # Run the full native-host suite
   pytest tests/native_host/e2e/ -v

   # Run a specific test
   pytest tests/native_host/e2e/test_single_node.py::TestJobLifecycle::test_job_cancel -v

Tests that require more nodes than provided, or missing GPU/container prerequisites, are automatically skipped.

End-to-End Tests (WireGuard mesh)
---------------------------------

The WireGuard mesh tests are part of the native-host suite
(``tests/native_host/e2e/test_wg_mesh.py`` for raw mesh bring-up,
``tests/native_host/e2e/test_wg_k0s.py`` for k0s-over-mesh). They stand up a
**real** WireGuard mesh (kernel ``wg`` interfaces via ``spur net init``/``join``)
across the SSH test nodes, marked ``wireguard``.

They run **automatically** wherever the nodes allow — there is no opt-in flag.
The ``wireguard`` fixtures *enable* WireGuard rather than merely detect it: they
turn ``network.wg_enabled`` on in the cluster config and, if the node lacks the
data plane, install it (``wireguard-tools``, then the ``wireguard`` kernel module
or the ``wireguard-go`` userspace fallback). A test skips only where a data plane
genuinely cannot be provided, or where sudo/node-count requirements are unmet —
the same capability-gated model as the GPU and multi-CP k0s fixtures.

Automated today: mesh bring-up (all-to-all reachability including worker↔worker), k0s-over-mesh (cluster reaches ready, node InternalIPs are mesh IPs, the controller stays meshed), cross-node pod datapath proven to ride the tunnel (pod-to-pod over the pod CIDR with a rising per-peer ``wg`` transfer counter), service-CIDR reachability (a ClusterIP service reached across nodes over the mesh — kube-proxy DNATs it to a pod IP that the mesh carries, even though the service CIDR is never in any WireGuard ``AllowedIPs``), online node add/remove with ``net remove-peer`` cleanup, graceful k8s remove/add that keeps the node a Spur worker in the mesh with the same WireGuard key across the cycle, and login-node reachability.

Scaffolded but currently skipped: the 3-controller HA re-election (D5 — needs a multi-controller fixture the base harness does not yet provide).

Because the mesh is root-owned and ``spurd`` runs rootful, the fixtures require
passwordless (or ``SPUR_TEST_SSH_PASSWORD``-backed) sudo, and ``SPUR_TEST_NODES``
must list the nodes (as for the rest of the native-host suite): mesh bring-up
(D1) needs >= 2 nodes; the k0s-over-mesh scenarios need 3. Nodes are assumed
Debian/Ubuntu (the installer is apt-based).

The tests use two pytest markers: ``wireguard`` (all of them) and ``k0s`` (the
scenarios that also need a converged k0s cluster). To select them:

.. code-block:: bash

   export SPUR_TEST_NODES=10.0.1.10,10.0.1.11,10.0.1.12

   # All WireGuard tests
   pytest tests/native_host/e2e/ -v -m wireguard

   # Only the raw mesh-bring-up scenarios (no k0s convergence needed)
   pytest tests/native_host/e2e/ -v -m "wireguard and not k0s"

In CI these tests run inside the existing ``E2E / native-host`` job and its
shards — no separate suite, path, or status check, and no CI-side WireGuard
setup (the fixtures install it on the nodes). Where a shard's nodes can't provide
a data plane, the WireGuard tests skip while the rest of the shard runs normally.

End-to-End Tests (Kubernetes)
-----------------------------

The K8s E2E suite lives in ``tests/k8s/e2e/`` and uses pytest with the Kubernetes Python client. It deploys Spur's controller (StatefulSet), operator (Deployment), and SpurJob CRD into a Kubernetes cluster, then submits SpurJobs and verifies their lifecycle.

You need a running Kubernetes cluster with:

1. **kubectl access** — a valid ``KUBECONFIG`` pointing to the cluster
2. **A Spur container image** available to the cluster (either in a registry or pre-loaded via ``ctr -n k8s.io images import``)
3. **RBAC permissions** to create namespaces, CRDs, StatefulSets, Deployments, and Pods

Prerequisites
~~~~~~~~~~~~~

- Python 3.11+ with ``pip install -r tests/requirements.txt``
- ``kubectl`` access to the cluster
- A Spur container image available to the cluster

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
     - Kubernetes namespace for the test run. Defaults to ``spur-ci-{pid}-{timestamp}``. Set explicitly for local runs so cleanup and log inspection are predictable.
     - ``spur-ci-local``

Setup
~~~~~

**Build and load the image:**

.. code-block:: bash

   # Build the Spur container image
   docker build --target runtime -t spur:ci .

   # If running a local cluster (e.g. kind):
   kind load docker-image spur:ci

   # If running on native-host K8s nodes, load via containerd:
   docker save spur:ci -o /tmp/spur-ci.tar
   # On each node:
   sudo ctr -n k8s.io images import /tmp/spur-ci.tar

The harness applies manifests from ``tests/k8s/e2e/manifests/`` (CRD, RBAC, controller, operator). Production-oriented samples live under ``examples/k8s/``. You do not need a separate ``kubectl apply`` before ``pytest`` unless you are debugging RBAC outside the suite.

Running the Tests
~~~~~~~~~~~~~~~~~

.. code-block:: bash

   export SPUR_CI_IMAGE=spur:ci
   export SPUR_TEST_NS=spur-ci-local

   # Run the full K8s suite
   pytest tests/k8s/e2e/ -v

   # Run SpurJob lifecycle tests only
   pytest tests/k8s/e2e/test_spurjob.py -v

   # Run Raft HA tests only
   pytest tests/k8s/e2e/test_raft_ha.py -v

Cleanup
~~~~~~~

The session ``k8s_suite`` fixture creates one namespace per pytest run. Class-scoped fixtures deploy the controller and operator once per test class (SpurJob lifecycle vs Raft HA); an autouse fixture removes SpurJobs between tests. Session teardown deletes the cross-namespace auxiliary namespace (``{SPUR_TEST_NS}-user1``) first, then the primary namespace and the SpurJob CRD.

If pytest exits abnormally (SIGKILL, node reboot, etc.), namespaces may be left behind. Clean up stale namespaces manually:

.. code-block:: bash

   # List test namespaces (includes cross-ns suffix -user1)
   kubectl get ns | grep spur-ci

   # Delete auxiliary namespace first if both exist
   kubectl delete ns "${SPUR_TEST_NS}-user1" --ignore-not-found
   kubectl delete ns <namespace-name>

