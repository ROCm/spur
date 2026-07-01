# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""
Pytest configuration and fixtures for Spur native-host E2E tests.

See docs/developer/building.rst for full environment variable reference.
"""

import os
import time
from pathlib import Path

import pytest

from cluster import SshNode, SpurCluster, ensure_bins, make_remote_dir

_REPO_ROOT = Path(__file__).resolve().parents[3]


def _get_nodes_config() -> list[str]:
    raw = os.environ.get("SPUR_TEST_NODES", "")
    nodes = [n.strip() for n in raw.split(",") if n.strip()]
    if not nodes:
        pytest.exit("SPUR_TEST_NODES not set — cannot run E2E tests", returncode=1)
    return nodes


def _get_ssh_user() -> str:
    user = os.environ.get("SPUR_TEST_SSH_USER", "")
    if not user:
        pytest.exit("SPUR_TEST_SSH_USER not set — cannot run E2E tests", returncode=1)
    return user


def _get_ssh_password() -> str | None:
    return os.environ.get("SPUR_TEST_SSH_PASSWORD") or None


def _get_ssh_key() -> str | None:
    key = os.environ.get("SPUR_TEST_SSH_KEY", "")
    return key if key else None


def _get_binaries_dir() -> str:
    return os.environ.get(
        "SPUR_TEST_BINARIES_DIR",
        str(_REPO_ROOT / "target" / "release"),
    )


@pytest.fixture(scope="session")
def ssh_nodes():
    """
    Session-scoped SSH connections to all nodes.
    Stays open for the entire test run.
    """
    nodes_config = _get_nodes_config()
    ssh_user = _get_ssh_user()
    ssh_password = _get_ssh_password()
    ssh_key = _get_ssh_key()

    nodes = []
    for host in nodes_config:
        node = SshNode(host, ssh_user, password=ssh_password, key_path=ssh_key)
        nodes.append(node)

    yield nodes

    for node in nodes:
        node.close()


@pytest.fixture(scope="session")
def remote_bin_dir(ssh_nodes, tmp_path_factory):
    """
    Session-scoped remote directory for binaries.

    If SPUR_TEST_REMOTE_BIN_DIR is set, uses that fixed path (not cleaned up).
    This is useful for CI where a predictable path is needed for AppArmor profiles.

    Otherwise, generates an ephemeral path from tmp_path_factory and cleans up
    at session end.
    """
    fixed = os.environ.get("SPUR_TEST_REMOTE_BIN_DIR", "")
    if fixed:
        yield fixed
        return

    session_tmp = tmp_path_factory.getbasetemp()
    remote_path = f"/tmp/spur-e2e-bin-{session_tmp.name}"

    yield remote_path

    for node in ssh_nodes:
        node.exec_allow_fail(f"rm -rf '{remote_path}'")


@pytest.fixture(scope="session", autouse=True)
def _ensure_bins(ssh_nodes, remote_bin_dir):
    """
    Session-scoped: uploads binaries to all nodes once.
    Skips upload if binary already exists with matching size.
    """
    ensure_bins(ssh_nodes, _get_binaries_dir(), remote_bin_dir)


def _deploy_cluster(ssh_nodes, remote_bin_dir, *, agent_as_root: bool = False,
                    config_overrides: dict | None = None,
                    agent_labels: dict[int, dict[str, str]] | None = None):
    """Helper: create, deploy, and return a SpurCluster. Tears down on deploy failure."""
    c = SpurCluster(ssh_nodes, make_remote_dir(), remote_bin_dir)
    try:
        c.deploy(config_overrides=config_overrides, agent_as_root=agent_as_root,
                 agent_labels=agent_labels)
    except Exception:
        c.teardown()
        raise
    return c


def _provision_cluster(ssh_nodes, remote_bin_dir):
    """Helper: create and provision (but do not start) a SpurCluster."""
    c = SpurCluster(ssh_nodes, make_remote_dir(), remote_bin_dir)
    try:
        c.provision()
    except Exception:
        c.teardown()
        raise
    return c


@pytest.fixture
def cluster_config_overrides():
    """Override this fixture in tests/classes to customise the cluster config.

    Return a dict that will be deep-merged into the default config before
    spurctld starts.  The default (no overrides) returns None.
    """
    return None


@pytest.fixture
def cluster(ssh_nodes, remote_bin_dir, cluster_config_overrides):
    """
    Per-test fixture: a fully running Spur cluster with default config.
    Torn down (processes killed, dirs removed) after the test.
    """
    spur_cluster = _deploy_cluster(ssh_nodes, remote_bin_dir,
                                   config_overrides=cluster_config_overrides)
    yield spur_cluster
    spur_cluster.teardown()


@pytest.fixture
def unstarted_cluster(ssh_nodes, remote_bin_dir):
    """
    Per-test fixture: a provisioned cluster (dirs created, hostnames
    resolved) but **not started**.

    The test should write any scripts or files it needs, then call
    ``cluster.start(config_overrides)`` to bring up the daemons with
    the desired configuration.
    """
    spur_cluster = _provision_cluster(ssh_nodes, remote_bin_dir)
    yield spur_cluster
    spur_cluster.teardown()


@pytest.fixture
def multi_node_cluster(ssh_nodes, remote_bin_dir, cluster_config_overrides):
    """
    Per-test fixture for multi-node tests.
    Skips if fewer than 2 nodes are configured.
    """
    if len(ssh_nodes) < 2:
        pytest.skip(
            f"Multi-node tests require at least 2 nodes in SPUR_TEST_NODES "
            f"(got {len(ssh_nodes)})"
        )

    spur_cluster = _deploy_cluster(ssh_nodes, remote_bin_dir,
                                   config_overrides=cluster_config_overrides)
    yield spur_cluster
    spur_cluster.teardown()


@pytest.fixture
def accounting_cluster(ssh_nodes, remote_bin_dir):
    """
    Per-test fixture: a running cluster with Postgres on node 0.

    Accounting runs inside spurctld. Skips if node 0 lacks Docker.
    """
    if len(ssh_nodes) < 1:
        pytest.skip("accounting tests require at least one node")
    try:
        ensure_bins(ssh_nodes[:1], _get_binaries_dir(), remote_bin_dir,
                    with_accounting=True)
    except FileNotFoundError as e:
        pytest.skip(f"accounting binaries unavailable: {e}")

    c = SpurCluster(ssh_nodes, make_remote_dir(), remote_bin_dir)
    try:
        c.enable_accounting()
    except RuntimeError as e:
        pytest.skip(str(e))
    try:
        c.deploy()
    except Exception:
        c.teardown()
        raise
    yield c
    c.teardown()


def _any_node_has_gpu(nodes: list[SshNode]) -> bool:
    for node in nodes:
        probe = node.exec_allow_fail(
            "ls /dev/kfd /dev/dri/card* /dev/dri/renderD* 2>/dev/null | head -1"
        )
        if probe.strip():
            return True
    return False


@pytest.fixture
def gpu_cluster(request, ssh_nodes, remote_bin_dir):
    """
    Per-test fixture for GPU tests.

    Skips the entire test if no node has GPU device nodes.
    Decorate a test with ``@pytest.mark.rootful`` to launch spurd via sudo.
    """
    if len(ssh_nodes) < 1:
        pytest.skip("GPU tests require at least one node in SPUR_TEST_NODES")
    if not _any_node_has_gpu(ssh_nodes):
        pytest.skip("no GPU device nodes (/dev/kfd, /dev/dri/card*, /dev/dri/renderD*) on any node")

    as_root = request.node.get_closest_marker("rootful") is not None
    c = _deploy_cluster(ssh_nodes, remote_bin_dir, agent_as_root=as_root)
    yield c
    c.teardown()


@pytest.fixture(scope="class")
def label_cluster(ssh_nodes, remote_bin_dir):
    """Class-scoped cluster for node label and partition selector tests."""
    if len(ssh_nodes) < 2:
        pytest.skip(
            f"Label cluster requires at least 2 nodes in SPUR_TEST_NODES "
            f"(got {len(ssh_nodes)})"
        )

    c = _deploy_cluster(
        ssh_nodes,
        remote_bin_dir,
        config_overrides={
            "partitions": [
                {
                    "name": "gpu",
                    "state": "UP",
                    "selector": {"gpu": "mi300x"},
                    "max_time": "1:00:00",
                },
                {
                    "name": "catchall",
                    "state": "UP",
                    "default": True,
                    "nodes": "ALL",
                    "max_time": "1:00:00",
                },
            ],
        },
        agent_labels={0: {"gpu": "mi300x"}},
    )
    yield c
    c.teardown()
