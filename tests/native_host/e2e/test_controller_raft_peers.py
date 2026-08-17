# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for Raft node_id derivation from a hostname peer list.

Separate module because it needs a differently configured cluster than
test_controller_raft.py: peers addressed by hostname and no explicit node_id.
That cluster cannot coexist with the module-scoped one there, since both bind
the same ports on the same nodes.

The hostname form only works where every node resolves every peer, which a
stock host does not do -- the fixture probes for it and skips otherwise.
"""
import pytest

pytestmark = pytest.mark.suite_ha


class TestNodeIdDerivation:
    def test_node_id_is_derived_from_peer_position(self, hostname_raft_cluster):
        """Peers listed by hostname must let each controller pick its id from
        its own position rather than needing an explicit node_id."""
        cluster = hostname_raft_cluster
        for i in cluster.controller_indices:
            log = cluster.spurctld_log(i)
            assert "position in controller.peers" in log, (
                f"controller {i} did not derive its node_id from the peer list:\n{log}"
            )
            assert f"node_id={i + 1}" in log.replace('"', ""), (
                f"controller {i} should be node_id {i + 1}:\n{log}"
            )

    def test_the_derived_cluster_elects_a_leader(self, hostname_raft_cluster):
        """Derivation is only correct if it produces a working voter set: a
        duplicated or out-of-range id would leave the cluster leaderless."""
        leader = hostname_raft_cluster.wait_raft_leader()
        assert leader in hostname_raft_cluster.controller_indices
