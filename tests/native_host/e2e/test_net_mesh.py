# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for `spur net` (WireGuard mesh).

Bringing an interface up rewrites `/etc/wireguard` and adds routes on a shared
test node, so anything mutating is opt-in behind `SPUR_TEST_WIREGUARD=1`. The
read-only and error paths run everywhere, since those are what a user hits
first on a node where WireGuard was never set up.
"""

import os

import pytest

MUTATING = os.environ.get("SPUR_TEST_WIREGUARD") == "1"
mutating = pytest.mark.skipif(
    not MUTATING,
    reason="brings a WireGuard interface up on a shared node; "
    "set SPUR_TEST_WIREGUARD=1 to run",
)

TEST_IFACE = "spurtest0"
TEST_CIDR = "10.45.0.0/24"


def net_cli(cluster, args: list[str]) -> tuple[int, str]:
    return cluster.cli_with_env(["spur", "net"] + args, {})


class TestStatus:
    def test_status_on_a_missing_interface_explains_itself(self, cluster):
        """The first thing a user runs on an unconfigured node. A bare `wg
        show` error would leave them with nothing to act on."""
        code, out = net_cli(cluster, ["status", "--interface", "spurtest-absent"])
        assert code != 0, out
        assert "is not up" in out, out
        assert "spur net init" in out, out

    def test_status_names_the_interface_it_looked_for(self, cluster):
        _, out = net_cli(cluster, ["status", "--interface", "spurtest-absent"])
        assert "spurtest-absent" in out, out


class TestArgumentValidation:
    def test_join_requires_an_endpoint(self, cluster):
        code, out = net_cli(
            cluster, ["join", "--server-key", "x", "--address", "10.45.0.2"]
        )
        assert code != 0, out

    def test_join_requires_a_server_key(self, cluster):
        code, out = net_cli(
            cluster, ["join", "--endpoint", "host:51820", "--address", "10.45.0.2"]
        )
        assert code != 0, out

    def test_mesh_requires_a_config(self, cluster):
        code, out = net_cli(cluster, ["mesh", "--self", "10.45.0.1"])
        assert code != 0, out

    def test_a_missing_mesh_config_is_reported_by_path(self, cluster):
        code, out = net_cli(
            cluster,
            ["mesh", "--config", "/nonexistent/mesh.json", "--self", "10.45.0.1"],
        )
        assert code != 0, out
        assert "/nonexistent/mesh.json" in out, out


class TestMeshPlanning:
    """`--dry-run` is the only way to inspect a mesh plan without root, so it
    has to validate the membership fully and never reach `wg` or `ip`."""

    def test_the_plan_lists_every_peer_but_self(self, cluster):
        config = _membership(cluster, "mesh-plan.json", ["10.45.0.1", "10.45.0.2"])
        code, out = net_cli(
            cluster,
            ["mesh", "--config", config, "--self", "10.45.0.1", "--dry-run",
             "--interface", TEST_IFACE],
        )
        assert code == 0, out
        assert "key-10.45.0.2" in out, out
        assert "key-10.45.0.1" not in out, f"self must not be listed as a peer:\n{out}"

    def test_the_plan_does_not_query_the_interface(self, cluster):
        config = _membership(cluster, "mesh-noiface.json", ["10.45.0.1", "10.45.0.2"])
        _, out = net_cli(
            cluster,
            ["mesh", "--config", config, "--self", "10.45.0.1", "--dry-run",
             "--interface", "spurtest-absent"],
        )
        assert "is not up" not in out, (
            f"--dry-run must not touch the interface:\n{out}"
        )

    def test_a_self_outside_the_membership_is_rejected(self, cluster):
        """Applying a mesh that omits this node would strip its own peers, so
        the mismatch has to fail before anything is written."""
        config = _membership(cluster, "mesh-nostranger.json", ["10.45.0.1"])
        code, out = net_cli(
            cluster,
            ["mesh", "--config", config, "--self", "10.45.0.9", "--dry-run"],
        )
        assert code != 0, out
        assert "is not present in" in out, out

    def test_a_malformed_mesh_ip_is_rejected(self, cluster):
        config = cluster.write_file(
            "mesh-badip.json",
            '{"nodes": [{"public_key": "k", "mesh_ip": "not-an-ip", '
            '"endpoint": "10.45.0.1:51820"}]}\n',
            executable=False,
        )
        code, out = net_cli(
            cluster, ["mesh", "--config", config, "--self", "not-an-ip", "--dry-run"]
        )
        assert code != 0, out
        assert "mesh_ip" in out, out

    def test_a_malformed_pod_cidr_is_rejected(self, cluster):
        config = cluster.write_file(
            "mesh-badcidr.json",
            '{"nodes": [{"public_key": "k", "mesh_ip": "10.45.0.1", '
            '"endpoint": "10.45.0.1:51820", "pod_cidr": "10.42.1.0"}]}\n',
            executable=False,
        )
        code, out = net_cli(
            cluster, ["mesh", "--config", config, "--self", "10.45.0.1", "--dry-run"]
        )
        assert code != 0, out
        assert "pod_cidr" in out, out

    def test_pod_routes_are_only_planned_when_requested(self, cluster):
        """A CNI usually owns pod routes; installing them unasked would fight
        it, so the plan must say it is leaving them alone."""
        config = cluster.write_file(
            "mesh-pod.json",
            '{"nodes": ['
            '{"public_key": "key-a", "mesh_ip": "10.45.0.1", '
            '"endpoint": "10.45.0.1:51820", "pod_cidr": "10.42.1.0/24"},'
            '{"public_key": "key-b", "mesh_ip": "10.45.0.2", '
            '"endpoint": "10.45.0.2:51820", "pod_cidr": "10.42.2.0/24"}'
            "]}\n",
            executable=False,
        )
        code, out = net_cli(
            cluster,
            ["mesh", "--config", config, "--self", "10.45.0.1", "--dry-run"],
        )
        assert code == 0, out
        assert "routes not programmed" in out, out

        code, out = net_cli(
            cluster,
            ["mesh", "--config", config, "--self", "10.45.0.1", "--dry-run",
             "--program-routes"],
        )
        assert code == 0, out
        assert "route 10.42.2.0/24" in out, out


def _membership(cluster, filename: str, mesh_ips: list[str]) -> str:
    nodes = ", ".join(
        f'{{"public_key": "key-{ip}", "mesh_ip": "{ip}", "endpoint": "{ip}:51820"}}'
        for ip in mesh_ips
    )
    return cluster.write_file(
        filename, f'{{"nodes": [{nodes}]}}\n', executable=False
    )


@mutating
class TestInterfaceLifecycle:
    @pytest.fixture
    def wg_node(self, cluster):
        cluster.wireguard_preflight()
        cluster.root_agent_preflight()
        yield cluster
        cluster.nodes[0].exec_allow_fail(
            f"sudo wg-quick down {TEST_IFACE} 2>/dev/null || true"
        )
        cluster.nodes[0].exec_allow_fail(
            f"sudo rm -f /etc/wireguard/{TEST_IFACE}.conf"
        )

    def test_init_brings_the_interface_up(self, wg_node):
        code, out = wg_node.sudo_cli(
            ["spur", "net", "init", "--cidr", TEST_CIDR, "--interface", TEST_IFACE]
        )
        assert code == 0, out

        code, status = net_cli(wg_node, ["status", "--interface", TEST_IFACE])
        assert code == 0, status
        assert "interface" in status.lower(), status

    def test_init_writes_a_private_config(self, wg_node):
        code, out = wg_node.sudo_cli(
            ["spur", "net", "init", "--cidr", TEST_CIDR, "--interface", TEST_IFACE]
        )
        assert code == 0, out

        mode = wg_node.nodes[0].exec_allow_fail(
            f"sudo stat -c%a /etc/wireguard/{TEST_IFACE}.conf"
        ).strip()
        assert mode == "600", (
            f"the config holds a private key and must not be world-readable, got {mode}"
        )

    def test_a_peer_can_be_added(self, wg_node):
        code, out = wg_node.sudo_cli(
            ["spur", "net", "init", "--cidr", TEST_CIDR, "--interface", TEST_IFACE]
        )
        assert code == 0, out

        peer_key = wg_node.nodes[0].exec("wg genkey | wg pubkey").strip()
        code, out = wg_node.sudo_cli(
            [
                "spur",
                "net",
                "add-peer",
                "--key",
                peer_key,
                "--allowed-ip",
                "10.45.0.9/32",
                "--interface",
                TEST_IFACE,
            ]
        )
        assert code == 0, out
        assert "Peer added" in out, out

        _, status = net_cli(wg_node, ["status", "--interface", TEST_IFACE])
        assert peer_key in status, status
