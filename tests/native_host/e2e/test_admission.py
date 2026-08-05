# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""
E2E tests for node admission.
"""

import time


def _wait_registered(cluster, timeout=30):
    """Poll sinfo until all nodes appear (positive assertion with early exit)."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            nodes = cluster.sinfo_nodes()
            if all(name in nodes for name in cluster.node_names):
                return nodes
        except Exception:
            pass
        time.sleep(2)
    nodes = cluster.sinfo_nodes()
    for name in cluster.node_names:
        assert name in nodes, f"node {name} not registered within {timeout}s"
    return nodes


def _assert_not_registered(cluster, timeout=15):
    """Poll sinfo for *timeout* seconds, failing immediately if any node appears."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            nodes = cluster.sinfo_nodes()
        except Exception:
            time.sleep(2)
            continue
        for name in cluster.node_names:
            assert name not in nodes, (
                f"node {name} registered unexpectedly"
            )
        time.sleep(2)


class TestAdmissionOpen:
    """Verify open mode (default) works without tokens."""

    def test_open_mode_agents_register_without_token(self, cluster):
        states = cluster.sinfo_nodes()
        for name in cluster.node_names:
            assert name in states, f"node {name} not found in sinfo"
        assert any(s.startswith("idle") for s in states.values())


class TestAdmissionToken:
    """Token mode admission tests. Uses unstarted_cluster for control over startup."""

    ADMISSION_CONFIG = {
        "admission": {"mode": "token"},
        "auth": {"plugin": "jwt", "jwt_key": "e2e-test-key"},
        "partitions": [
            {
                "name": "default",
                "state": "UP",
                "default": True,
                "nodes": "ALL",
                "max_time": "24:00:00",
                "default_time": "10:00",
            }
        ],
        "nodes": [],
    }

    def test_rejects_agent_without_token(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.start_controller(config_overrides=self.ADMISSION_CONFIG)
        cluster.start_agents(token=None)
        _assert_not_registered(cluster)

    def test_admits_agent_with_valid_token(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.start_controller(config_overrides=self.ADMISSION_CONFIG)

        token_output = cluster.cli(["spur", "token", "create"])
        token = token_output.strip().split("\n")[0]
        assert "." in token, f"unexpected token format: {token}"

        cluster.start_agents(token=token)
        _wait_registered(cluster)

    def test_revoked_token_blocks_registration(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.start_controller(config_overrides=self.ADMISSION_CONFIG)

        token_output = cluster.cli(["spur", "token", "create"])
        token = token_output.strip().split("\n")[0]
        token_id = token.split(".")[0]

        cluster.cli(["spur", "token", "revoke", token_id])

        cluster.start_agents(token=token)
        _assert_not_registered(cluster)

    def test_token_list_displays_tokens(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.start_controller(config_overrides=self.ADMISSION_CONFIG)

        out1 = cluster.cli(["spur", "token", "create"])
        token1 = out1.strip().split("\n")[0]
        id1 = token1.split(".")[0]

        out2 = cluster.cli(["spur", "token", "create", "--ttl", "1h"])
        token2 = out2.strip().split("\n")[0]
        id2 = token2.split(".")[0]

        list_out = cluster.cli(["spur", "token", "list"])
        assert id1 in list_out, f"token {id1} not in list output"
        assert id2 in list_out, f"token {id2} not in list output"
        assert "ID" in list_out
        assert "CREATED" in list_out

    def test_heartbeat_survives_after_admission(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.start_controller(config_overrides=self.ADMISSION_CONFIG)

        token_output = cluster.cli(["spur", "token", "create"])
        token = token_output.strip().split("\n")[0]

        cluster.start_agents(token=token)
        _wait_registered(cluster)

        time.sleep(45)

        states = cluster.sinfo_nodes()
        for name in cluster.node_names:
            assert name in states, f"node {name} disappeared after heartbeat interval"
            assert not states[name].startswith("down"), (
                "node marked down - heartbeat JWT likely failing"
            )
