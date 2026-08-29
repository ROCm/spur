# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""End-to-end tests for the raw WireGuard mesh, driven over real `wg` interfaces.

Mesh bring-up with no k0s: 1 controller + N workers join the mesh and every node
reaches every other over the mesh IPs (controller↔worker AND worker↔worker). The
k0s-over-mesh scenarios live in ``test_wg_k0s.py``.

The ``raw_wg_mesh`` fixture stands up the mesh per test and skips when a node
can't run a real WireGuard data plane.
"""

from __future__ import annotations

import pytest


class TestMeshBringUp:
    def test_all_nodes_reach_each_other_over_mesh(self, raw_wg_mesh):
        """Every meshed node pings every other over its mesh IP — including
        worker↔worker, not just node↔controller (the hub-and-spoke default)."""
        indices = list(range(len(raw_wg_mesh.nodes)))
        raw_wg_mesh.assert_all_to_all(indices)

    def test_each_node_peers_every_other(self, raw_wg_mesh):
        """After full-mesh bring-up, each node's wg peer table lists all others."""
        indices = list(range(len(raw_wg_mesh.nodes)))
        for i in indices:
            keys = set(raw_wg_mesh.wg_peer_keys(i))
            expected = {raw_wg_mesh.pubkeys[j] for j in indices if j != i}
            assert expected <= keys, (
                f"node {raw_wg_mesh.node_names[i]} missing peers: "
                f"{expected - keys}"
            )

    def test_worker_to_worker_has_endpoint(self, raw_wg_mesh):
        """The worker↔worker peers carry a real endpoint (the bug where a full
        mesh had AllowedIPs but no endpoint, so those tunnels never connected)."""
        if len(raw_wg_mesh.nodes) < 3:
            pytest.skip("worker↔worker endpoint check needs >= 3 nodes")
        # From worker 1's view, the peer for worker 2 must have an endpoint set.
        peer = raw_wg_mesh.wg_peer(1, raw_wg_mesh.pubkeys[2])
        assert peer is not None, "worker 2 not a peer of worker 1"
        assert peer.endpoint is not None, (
            f"worker↔worker peer has no endpoint (hub-and-spoke regression): {peer}"
        )
