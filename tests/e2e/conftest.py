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

from cluster import SshNode, SpurCluster, ensure_bins


def _repo_root() -> Path:
    return Path(__file__).resolve().parent.parent.parent


def _get_nodes_config() -> list[str]:
    raw = os.environ.get("SPUR_TEST_NODES", "")
    nodes = [n.strip() for n in raw.split(",") if n.strip()]
    if not nodes:
        pytest.exit("SPUR_TEST_NODES not set — cannot run E2E tests", returncode=0)
    return nodes


def _get_ssh_user() -> str:
    user = os.environ.get("SPUR_TEST_SSH_USER", "")
    if not user:
        pytest.exit("SPUR_TEST_SSH_USER not set — cannot run E2E tests", returncode=0)
    return user


def _get_ssh_password() -> str | None:
    return os.environ.get("SPUR_TEST_SSH_PASSWORD") or None


def _get_ssh_key() -> str | None:
    key = os.environ.get("SPUR_TEST_SSH_KEY", "")
    return key if key else None


def _get_binaries_dir() -> str:
    return os.environ.get(
        "SPUR_TEST_BINARIES_DIR",
        str(_repo_root() / "target" / "release"),
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


def _make_remote_dir() -> str:
    """Generate a unique remote working directory path per test."""
    return f"/tmp/spur-e2e-{os.getpid()}-{int(time.time() * 1000)}"


def _deploy_cluster(ssh_nodes, remote_bin_dir):
    """Helper: create, deploy, and return a SpurCluster. Tears down on deploy failure."""
    remote_dir = _make_remote_dir()
    spur_cluster = SpurCluster(ssh_nodes, remote_dir, remote_bin_dir)
    try:
        spur_cluster.deploy()
    except Exception:
        spur_cluster.teardown()
        raise
    return spur_cluster


@pytest.fixture
def cluster(ssh_nodes, remote_bin_dir):
    """
    Per-test fixture: starts a fresh Spur cluster in a unique remote dir,
    yields it, then kills processes and removes the dir.
    """
    spur_cluster = _deploy_cluster(ssh_nodes, remote_bin_dir)
    yield spur_cluster
    spur_cluster.teardown()


@pytest.fixture
def multi_node_cluster(ssh_nodes, remote_bin_dir):
    """
    Per-test fixture for multi-node tests.
    Skips if fewer than 2 nodes are configured.
    """
    if len(ssh_nodes) < 2:
        pytest.skip(
            f"Multi-node tests require at least 2 nodes in SPUR_TEST_NODES "
            f"(got {len(ssh_nodes)})"
        )

    spur_cluster = _deploy_cluster(ssh_nodes, remote_bin_dir)
    yield spur_cluster
    spur_cluster.teardown()


@pytest.fixture
def gpu_cluster(ssh_nodes, remote_bin_dir):
    """
    Per-test fixture for GPU tests.
    Skips if fewer than 2 nodes are configured.
    """
    if len(ssh_nodes) < 2:
        pytest.skip(
            f"GPU tests require at least 2 nodes in SPUR_TEST_NODES "
            f"(got {len(ssh_nodes)})"
        )

    spur_cluster = _deploy_cluster(ssh_nodes, remote_bin_dir)
    yield spur_cluster
    spur_cluster.teardown()
