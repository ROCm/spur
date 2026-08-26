# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Fixtures for the native-host WireGuard mesh E2E suite.

This suite stands up a REAL WireGuard mesh (kernel `wg` interfaces) across the
SSH test nodes, so every fixture here is gated three ways:

* ``SPUR_TEST_NODES`` must list the nodes (shared with the base suite),
* ``SPUR_TEST_WG=1`` must opt in (the mesh + rootful daemons are invasive), and
* a per-node preflight (`wg_available`) skips if the `wg` tool / module is
  missing — mirroring how the base suite's mpi/gpu fixtures skip on absent deps.

Session scaffolding (SSH connections, binary upload) is re-declared here rather
than imported: pytest fixtures are scoped to the conftest's directory tree, and
this suite is a sibling directory of ``native_host/e2e`` (``native_host/wg_e2e``),
not nested under it — so it does not inherit that suite's conftest/fixtures. The
underlying *helpers* (`SshNode`, `ensure_bins`, `make_remote_dir`, `SpurCluster`)
are imported from ``cluster`` (on the pytest pythonpath) so there is no
duplicated logic — only the fixture wiring is repeated.
"""

from __future__ import annotations

import logging
import os
from pathlib import Path

import pytest

from cluster import (
    SpurCluster,
    SshNode,
    ensure_bins,
    make_remote_dir,
)
from wg_cluster import WG_IFACE, WgMesh, wg_available

logger = logging.getLogger(__name__)

_REPO_ROOT = Path(__file__).resolve().parents[3]


# The `wireguard` and `k0s` markers are registered centrally in tests/pytest.ini
# so they resolve identically whether this suite runs standalone or folded into
# the full tree — no per-conftest registration that could drift from a sibling's.


# --- env helpers (mirror native_host/e2e/conftest.py) ---


def _nodes_config() -> list[str]:
    raw = os.environ.get("SPUR_TEST_NODES", "")
    nodes = [n.strip() for n in raw.split(",") if n.strip()]
    if not nodes:
        pytest.exit("SPUR_TEST_NODES not set — cannot run WG E2E tests", returncode=1)
    return nodes


def _ssh_user() -> str:
    user = os.environ.get("SPUR_TEST_SSH_USER", "")
    if not user:
        pytest.exit("SPUR_TEST_SSH_USER not set — cannot run WG E2E tests", returncode=1)
    return user


def _ssh_password() -> str | None:
    return os.environ.get("SPUR_TEST_SSH_PASSWORD") or None


def _ssh_key() -> str | None:
    return os.environ.get("SPUR_TEST_SSH_KEY") or None


def _binaries_dir() -> str:
    return os.environ.get(
        "SPUR_TEST_BINARIES_DIR", str(_REPO_ROOT / "target" / "release")
    )


def _wg_addresses() -> dict[int, str] | None:
    """Optional explicit per-node mesh addresses, positional like SPUR_TEST_NODES.

    Mirrors spur-toolkit's per-node ``spur_wg_address`` inventory var: the
    operator declares each node's mesh IP rather than relying on an assignment
    algorithm. Example: ``SPUR_TEST_WG_ADDRESSES=10.44.0.1,10.44.0.2,10.44.0.3``.
    When unset, WgMesh falls back to its k0s-matching default. When set, the
    count must match SPUR_TEST_NODES.
    """
    raw = os.environ.get("SPUR_TEST_WG_ADDRESSES", "").strip()
    if not raw:
        return None
    addrs = [a.strip() for a in raw.split(",") if a.strip()]
    nodes = _nodes_config()
    if len(addrs) != len(nodes):
        pytest.exit(
            f"SPUR_TEST_WG_ADDRESSES has {len(addrs)} entries but SPUR_TEST_NODES "
            f"has {len(nodes)} — they must match", returncode=1
        )
    return {i: a for i, a in enumerate(addrs)}


def _sudo_prefix() -> str:
    """Match SpurCluster._sudo_prefix: password-backed sudo -S, else sudo -n."""
    pw = os.environ.get("SPUR_TEST_SSH_PASSWORD", "")
    if pw:
        escaped = pw.replace("'", "'\"'\"'")
        return f"echo '{escaped}' | sudo -S "
    return "sudo -n "


# --- session scaffolding ---


@pytest.fixture(scope="session")
def ssh_nodes():
    nodes = [
        SshNode(host, _ssh_user(), password=_ssh_password(), key_path=_ssh_key())
        for host in _nodes_config()
    ]
    yield nodes
    for node in nodes:
        node.close()


@pytest.fixture(scope="session")
def remote_bin_dir(ssh_nodes, tmp_path_factory):
    fixed = os.environ.get("SPUR_TEST_REMOTE_BIN_DIR", "")
    if fixed:
        yield fixed
        return
    session_tmp = tmp_path_factory.getbasetemp()
    remote_path = f"/tmp/spur-wg-e2e-bin-{session_tmp.name}"
    yield remote_path
    for node in ssh_nodes:
        node.exec_allow_fail(f"rm -rf '{remote_path}'")


@pytest.fixture(scope="session", autouse=True)
def _ensure_bins(ssh_nodes, remote_bin_dir):
    ensure_bins(ssh_nodes, _binaries_dir(), remote_bin_dir)


@pytest.fixture(scope="session", autouse=True)
def _wg_preflight(ssh_nodes):
    """Skip the whole suite unless every node can run a real WireGuard mesh."""
    if not os.environ.get("SPUR_TEST_WG", "").strip():
        pytest.skip("SPUR_TEST_WG not set — WG mesh suite is opt-in")
    sudo = _sudo_prefix()
    for node in ssh_nodes:
        ok, reason = wg_available(node, sudo)
        if not ok:
            pytest.skip(f"WireGuard unavailable on {node.host}: {reason}")


# --- mesh fixtures ---


@pytest.fixture
def wg_mesh(ssh_nodes, remote_bin_dir):
    """A raw WireGuard mesh across all nodes (no spur daemons).

    Yields a :class:`WgMesh` with the interface already provisioned via
    ``net init`` on node 0 and ``net join`` on the rest, promoted to a full
    mesh. Torn down (interfaces + confs removed) after the test.
    """
    if len(ssh_nodes) < 2:
        pytest.skip(f"WG mesh needs >= 2 nodes (got {len(ssh_nodes)})")
    c = SpurCluster(ssh_nodes, make_remote_dir(), remote_bin_dir)
    c.provision()  # resolves node_names; no daemons started
    # Bringing up spur0 shells out to `wg`/`ip` via sudo on every node, so this
    # fixture needs the same rootful preflight as the k0s fixtures — otherwise a
    # node without NOPASSWD sudo (and no SPUR_TEST_SSH_PASSWORD) fails mid-bringup
    # instead of skipping cleanly.
    c.root_agent_preflight()
    mesh = WgMesh(ssh_nodes, c.node_names, remote_bin_dir, _sudo_prefix(),
                  iface=WG_IFACE, wg_addresses=_wg_addresses())
    indices = list(range(len(ssh_nodes)))
    try:
        mesh.bring_up(indices)
    except Exception:
        mesh.teardown(indices)
        c.teardown()
        raise
    yield mesh
    mesh.teardown(indices)
    c.teardown()


def _deploy_wg_k0s(ssh_nodes, remote_bin_dir, *, control_plane_index: int = 0):
    """Deploy a rootful, WG-enabled, calico k0s cluster over the mesh.

    Returns a running SpurCluster (daemons up, k0s NOT yet `up`) with the
    WireGuard mesh already established. ``wg_enabled=true`` only tells spurd to
    *read* its mesh key/IP from ``spur0`` — it does not create the interface, so
    the mesh must be stood up (``net init``/``join``) before the daemons start,
    otherwise spurd reports no ``wg_pubkey`` and the reconcile runs meshless.

    The live :class:`WgMesh` is attached as ``c.wg_mesh`` so the fixture can tear
    it down; ``c.wg_mesh_indices`` records which nodes it covers.
    """
    c = SpurCluster(ssh_nodes, make_remote_dir(), remote_bin_dir)
    c.provision()
    c.root_agent_preflight()  # skips if no sudo

    # Start from a clean k0s slate: a prior test (or a crashed run) can leave
    # k0s services + an etcd datadir behind, which corrupts this cluster's
    # membership. Reset before bringing anything up — cheap on an already-clean
    # node, essential after a dirty one.
    _reset_k0s_all_nodes(c)

    # Establish the mesh first (spur0 up on every node) so spurd reads its
    # wg_pubkey + mesh IP at startup and the reconcile has a real mesh to
    # converge. bring_up() idempotently pre-cleans any stale interface.
    indices = list(range(len(ssh_nodes)))
    mesh = WgMesh(ssh_nodes, c.node_names, remote_bin_dir, _sudo_prefix(),
                  iface=WG_IFACE, wg_addresses=_wg_addresses())
    try:
        mesh.bring_up(indices)
    except Exception:
        mesh.teardown(indices)
        c.teardown()
        raise
    c.wg_mesh = mesh
    c.wg_mesh_indices = indices

    overrides = {
        "network": {"wg_enabled": True, "wg_cidr": "10.44.0.0/16"},
        "cluster": {"enabled": True, "cni": "calico",
                    "control_plane_node": c.node_names[control_plane_index]},
    }
    try:
        c.start(config_overrides=overrides, agent_as_root=True)
    except Exception:
        mesh.teardown(indices)
        c.teardown()
        raise
    return c


def _reset_k0s_all_nodes(c):
    """Forcibly reset k0s on every node, independent of the controller.

    `spur k8s down --reset` only flags phase=Down; the controller's reconcile
    loop stops the k0s components *asynchronously*. Because the fixture kills
    spurctld/spurd right after (c.teardown()), that async reset is cut off — so
    without this, k0s services and the etcd datadir survive and the NEXT
    wg_k0s_cluster test inherits dirty state (membership won't shrink, the API
    server refuses connections). Reset each node directly so every test starts
    from a clean k0s slate.
    """
    sudo = c._sudo_prefix()
    for node in c.nodes:
        node.exec_allow_fail(
            f"{sudo}systemctl stop k0scontroller k0sworker 2>/dev/null || true"
        )
        node.exec_allow_fail(f"{sudo}k0s stop 2>/dev/null || true")
        node.exec_allow_fail(f"{sudo}k0s reset 2>/dev/null || true")
        node.exec_allow_fail(
            f"{sudo}rm -rf /etc/k0s /var/lib/k0s /run/k0s 2>/dev/null || true"
        )
        # The steps above are best-effort (|| true) so a dirty node can't wedge
        # teardown — but a reset that silently failed would resurface as a
        # baffling failure in the NEXT test that inherits the leftover k0s state.
        # Verify the datadir is actually gone and warn loudly if not, so the
        # blame lands on the node that failed to clean up, not its victim.
        leftover = node.exec_allow_fail(
            "test -e /var/lib/k0s && echo DIRTY || echo CLEAN"
        )
        if "DIRTY" in leftover:
            logger.warning(
                "k0s reset left /var/lib/k0s behind on %s — the next wg_k0s test "
                "may inherit dirty state (stale etcd membership / API refusals)",
                node.host,
            )


def _teardown_wg_k0s(c):
    """Tear down a cluster from _deploy_wg_k0s: daemons first, then k0s, mesh."""
    # Ask the controller to bring k0s down (best effort), but do NOT rely on the
    # async reset completing — force a direct per-node k0s reset below.
    try:
        c.k8s_down(reset=True)
        c.wait_k8s_phase("down", timeout=120)
    except Exception:
        pass
    # Kill spurctld/spurd BEFORE the manual k0s reset: the k8s_down(reset=True)
    # above kicks off an async reconcile that stops the k0s components itself, so
    # if the daemons are still alive while _reset_k0s_all_nodes runs its
    # `k0s reset`/`rm -rf`, that reconcile races the manual cleanup — the very
    # race the setup-side reset exists to avoid. Tearing the daemons down first
    # leaves nothing contending with the manual reset.
    c.teardown()
    _reset_k0s_all_nodes(c)
    mesh = getattr(c, "wg_mesh", None)
    if mesh is not None:
        mesh.teardown(getattr(c, "wg_mesh_indices", []))


@pytest.fixture
def wg_k0s_cluster(ssh_nodes, remote_bin_dir):
    """Rootful k0s-over-WireGuard cluster (calico, wg_enabled). Needs >= 3 nodes
    for an etcd quorum path and rootful sudo. Tears k0s down on exit."""
    if len(ssh_nodes) < 3:
        pytest.skip(f"k0s-over-mesh needs >= 3 nodes for quorum (got {len(ssh_nodes)})")
    c = _deploy_wg_k0s(ssh_nodes, remote_bin_dir)
    yield c
    _teardown_wg_k0s(c)


@pytest.fixture
def wg_login_topology(ssh_nodes, remote_bin_dir):
    """3-node topology for the login-node reachability scenario:

      node0 = spur controller (spurctld + spurd, mesh head, NOT in k0s scope)
      node1 = k8s control-plane (spurd, k0s CP)
      node2 = login node (spurd, meshed, NOT in k0s scope)

    All three are meshed; only node1 is in the k0s scope. Asserts the login
    node reaches every mesh node. Yields (cluster, mesh_indices).
    """
    if len(ssh_nodes) < 3:
        pytest.skip(f"login-node topology needs 3 nodes (got {len(ssh_nodes)})")
    c = _deploy_wg_k0s(ssh_nodes, remote_bin_dir, control_plane_index=1)
    yield c
    _teardown_wg_k0s(c)
