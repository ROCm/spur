# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""End-to-end tests for the raw WireGuard mesh, driven over real `wg` interfaces.

This file covers SPUR-212 scenario D1 — mesh bring-up with no k0s: 1 controller +
N workers join the mesh and every node reaches every other over the mesh IPs
(controller↔worker AND worker↔worker). The k0s-over-mesh scenarios (D2–D9) live
in ``test_wg_k0s.py``.

Marked ``wireguard``; the ``wg_mesh`` fixture installs WireGuard where missing,
stands up the mesh per test, and tears it down afterward.
"""

from __future__ import annotations

import pytest

pytestmark = pytest.mark.wireguard


# --- D1: mesh bring-up + all-to-all -----------------------------------------


class TestMeshBringUp:
    def test_all_nodes_reach_each_other_over_mesh(self, wg_mesh):
        """Every meshed node pings every other over its mesh IP — including
        worker↔worker, not just node↔controller (the hub-and-spoke default)."""
        indices = list(range(len(wg_mesh.nodes)))
        wg_mesh.assert_all_to_all(indices)

    def test_each_node_peers_every_other(self, wg_mesh):
        """After full-mesh bring-up, each node's wg peer table lists all others."""
        indices = list(range(len(wg_mesh.nodes)))
        for i in indices:
            keys = set(wg_mesh.wg_peer_keys(i))
            expected = {wg_mesh.pubkeys[j] for j in indices if j != i}
            assert expected <= keys, (
                f"node {wg_mesh.node_names[i]} missing peers: "
                f"{expected - keys}"
            )

    def test_worker_to_worker_has_endpoint(self, wg_mesh):
        """The worker↔worker peers carry a real endpoint (the bug where a full
        mesh had AllowedIPs but no endpoint, so those tunnels never connected)."""
        if len(wg_mesh.nodes) < 3:
            pytest.skip("worker↔worker endpoint check needs >= 3 nodes")
        # From worker 1's view, the peer for worker 2 must have an endpoint set.
        peer = wg_mesh.wg_peer(1, wg_mesh.pubkeys[2])
        assert peer is not None, "worker 2 not a peer of worker 1"
        assert peer.endpoint is not None, (
            f"worker↔worker peer has no endpoint (hub-and-spoke regression): {peer}"
        )
