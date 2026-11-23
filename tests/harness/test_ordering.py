# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Unit tests for k0s-last test ordering and the post-teardown node network check.

The ordering hooks are exercised through ``pytester`` with copies of the real root
and native-host conftests; the network check runs the real harness functions
against fake nodes.
"""

import hashlib
import importlib.util
from pathlib import Path

import pytest

import cluster as cluster_module
from cluster import NodeNetworkError, SshConnectionLost, _assert_node_network, assert_nodes_network

pytest_plugins = ["pytester"]

_TESTS_ROOT = Path(__file__).resolve().parents[1]
_ROOT_CONFTEST = _TESTS_ROOT / "conftest.py"
_NATIVE_CONFTEST = _TESTS_ROOT / "native_host" / "e2e" / "conftest.py"

DRIVER_IP = "10.0.0.9"


def _load_root_conftest():
    spec = importlib.util.spec_from_file_location("spur_root_conftest", _ROOT_CONFTEST)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class FakeItem:
    def __init__(self, nodeid, k0s=False):
        self.nodeid = nodeid
        self._k0s = k0s

    def get_closest_marker(self, name):
        return object() if name == "k0s" and self._k0s else None


def _ids(items):
    return [item.nodeid for item in items]


def test_order_k0s_last_is_a_stable_partition():
    order_k0s_last = _load_root_conftest().order_k0s_last
    items = [
        FakeItem("a", k0s=True), FakeItem("b"), FakeItem("c", k0s=True),
        FakeItem("d"), FakeItem("e"), FakeItem("f", k0s=True),
    ]

    assert _ids(order_k0s_last(items)) == ["b", "d", "e", "a", "c", "f"]


def test_order_k0s_last_keeps_the_order_when_nothing_is_k0s():
    order_k0s_last = _load_root_conftest().order_k0s_last
    items = [FakeItem("b"), FakeItem("a"), FakeItem("c")]

    assert _ids(order_k0s_last(items)) == ["b", "a", "c"]


def test_order_k0s_last_handles_no_items():
    assert _load_root_conftest().order_k0s_last([]) == []


E2E_TESTS = """\
import pytest


class TestPlain:
    def test_first(self, cluster):
        pass

    def test_second(self, cluster):
        pass


class TestMulticp:
    def test_up(self, k8s_multicp_cluster):
        pass

    def test_down(self, k8s_multicp_cluster):
        pass


class TestExplicit:
    @pytest.mark.k0s
    def test_marked(self, cluster):
        pass


class TestDrift:
    def test_drift(self, k8s_drift_cluster):
        pass


def test_wg(wg_k0s_cluster):
    pass


def test_native(k0s_native_cluster):
    pass


def test_last_plain(cluster):
    pass
"""

K0S_TESTS = {
    "TestMulticp::test_up", "TestMulticp::test_down", "TestExplicit::test_marked",
    "TestDrift::test_drift", "test_wg", "test_native",
}


@pytest.fixture
def e2e_layout(pytester, monkeypatch):
    # Selected by path, so the real root conftest treats this as the full suite.
    monkeypatch.setenv("SPUR_TEST_NODES", "node-a")
    pytester.makeini("[pytest]\naddopts = -p no:xdist\n")
    (pytester.path / "conftest.py").write_text(_ROOT_CONFTEST.read_text())
    e2e = pytester.path / "native_host" / "e2e"
    e2e.mkdir(parents=True)
    (e2e / "conftest.py").write_text(_NATIVE_CONFTEST.read_text())
    (e2e / "test_sample.py").write_text(E2E_TESTS)
    return pytester


def _collected(pytester, *args):
    result = pytester.runpytest("--collect-only", "-q", "native_host/e2e", *args)
    assert result.ret == 0, result.stdout.str()
    return [line.split("::", 1)[1] for line in result.outlines if "::" in line]


def _is_k0s(test_id):
    return test_id in K0S_TESTS


def test_collection_puts_k0s_tests_last(e2e_layout):
    ids = _collected(e2e_layout)

    assert len(ids) == 9
    first_k0s = next(i for i, t in enumerate(ids) if _is_k0s(t))
    assert all(_is_k0s(t) for t in ids[first_k0s:])
    assert not any(_is_k0s(t) for t in ids[:first_k0s])


def test_collection_keeps_relative_order_within_each_group(e2e_layout):
    ids = _collected(e2e_layout)

    assert [t for t in ids if not _is_k0s(t)] == [
        "TestPlain::test_first", "TestPlain::test_second", "test_last_plain",
    ]
    assert [t for t in ids if _is_k0s(t)] == [
        "TestMulticp::test_up", "TestMulticp::test_down", "TestExplicit::test_marked",
        "TestDrift::test_drift", "test_wg", "test_native",
    ]


def _expected_shard(nodeid, count):
    return int(hashlib.md5(nodeid.encode()).hexdigest(), 16) % count


@pytest.mark.parametrize("shard_id", [0, 1, 2])
def test_sharding_membership_is_unchanged_and_k0s_still_last(e2e_layout, shard_id):
    everything = _collected(e2e_layout)

    ids = _collected(e2e_layout, "--shard-count", "3", "--shard-id", str(shard_id))

    expected = {t for t in everything if _expected_shard(f"native_host/e2e/test_sample.py::{t}", 3) == shard_id}
    assert set(ids) == expected
    flags = [_is_k0s(t) for t in ids]
    assert flags == sorted(flags)


def test_ini_registers_k0s_and_quarantine_markers():
    ini = (_TESTS_ROOT / "pytest.ini").read_text()

    assert "k0s:" in ini
    assert "quarantine:" in ini


class NetFakeNode:
    """Answers the driver-address lookup and the route check with canned results."""

    def __init__(self, host, ssh_client=f"{DRIVER_IP} 51234 22", route_exit=0, route_out=None,
                 reconnect_error=None, run_error=None):
        self.host = host
        self.address = host
        self.ssh_client = ssh_client
        self.route_exit = route_exit
        self.route_out = f"{DRIVER_IP} dev eth0 src 10.0.0.2\nok\n" if route_out is None else route_out
        self.reconnect_error = reconnect_error
        self.run_error = run_error
        self.commands = []
        self.timeouts = []
        self.events = []

    def is_alive(self):
        return True

    def ensure_connected(self):
        self.events.append("ensure_connected")
        if self.reconnect_error is not None:
            raise self.reconnect_error

    def exec_with_exit(self, cmd, timeout=600):
        self.events.append("exec")
        self.commands.append(cmd)
        self.timeouts.append(timeout)
        if self.run_error is not None:
            raise self.run_error
        if "SSH_CLIENT" in cmd:
            return 0, self.ssh_client + "\n"
        return self.route_exit, self.route_out


def test_assert_node_network_runs_route_get_and_echo_on_a_short_timeout():
    node = NetFakeNode("node-a")

    _assert_node_network(node, DRIVER_IP)

    assert len(node.commands) == 1
    assert f"ip route get {DRIVER_IP}" in node.commands[0]
    assert "echo ok" in node.commands[0]
    assert node.timeouts == [20]


def test_assert_node_network_reconnects_before_probing():
    node = NetFakeNode("node-a")

    _assert_node_network(node, DRIVER_IP)

    assert node.events[0] == "ensure_connected"


def test_assert_node_network_fails_when_the_route_is_gone():
    node = NetFakeNode("node-a", route_exit=2, route_out="RTNETLINK answers: Network is unreachable\n")

    with pytest.raises(NodeNetworkError, match="node-a.*Network is unreachable"):
        _assert_node_network(node, DRIVER_IP)


def test_assert_node_network_fails_on_a_timeout():
    node = NetFakeNode("node-a", route_exit=-1, route_out="")

    with pytest.raises(NodeNetworkError, match="node-a.*timed out after 20s"):
        _assert_node_network(node, DRIVER_IP)


def test_assert_node_network_fails_when_the_session_cannot_be_restored():
    node = NetFakeNode("node-a", reconnect_error=SshConnectionLost("node-a down"))

    with pytest.raises(SshConnectionLost):
        _assert_node_network(node, DRIVER_IP)


def test_assert_node_network_rejects_a_zero_exit_without_the_echo():
    node = NetFakeNode("node-a", route_out="partial output")

    with pytest.raises(NodeNetworkError, match="node-a"):
        _assert_node_network(node, DRIVER_IP)


def test_assert_nodes_network_passes_on_healthy_nodes():
    nodes = [NetFakeNode("node-a"), NetFakeNode("node-b")]

    assert_nodes_network(nodes, ["a", "b"])

    assert all(len(n.commands) == 2 for n in nodes)
    assert all("SSH_CLIENT" in n.commands[0] for n in nodes)


@pytest.fixture
def diagnostics_spy(monkeypatch):
    calls = []

    def fake(nodes, names, log_dir, **kwargs):
        calls.append((list(nodes), list(names), log_dir, kwargs))
        return {f"{n} ({node.host})": f"state of {node.host}" for n, node in zip(names, nodes)}

    monkeypatch.setattr(cluster_module, "collect_node_diagnostics", fake)
    return calls


def test_assert_nodes_network_fails_naming_the_broken_node_with_peer_diagnostics(diagnostics_spy):
    broken = NetFakeNode("node-a", route_exit=2, route_out="RTNETLINK answers: Network is unreachable\n")
    nodes = [broken, NetFakeNode("node-b"), NetFakeNode("node-c")]

    with pytest.raises(pytest.fail.Exception) as info:
        assert_nodes_network(nodes, ["spur-a", "spur-b", "spur-c"])

    message = str(info.value)
    assert "node-a" in message and "Network is unreachable" in message
    assert "state of node-b" in message and "state of node-c" in message
    [(diag_nodes, diag_names, log_dir, kwargs)] = diagnostics_spy
    assert [n.host for n in diag_nodes] == ["node-b", "node-c"]
    assert diag_names == ["spur-b", "spur-c"]
    assert log_dir is None
    assert kwargs["k0s"] is True
    assert isinstance(kwargs["budget"], cluster_module.DiagnosticsBudget)


def test_assert_nodes_network_reports_every_broken_node(diagnostics_spy):
    nodes = [
        NetFakeNode("node-a", reconnect_error=SshConnectionLost("node-a unreachable")),
        NetFakeNode("node-b"),
        NetFakeNode("node-c", route_exit=1, route_out="boom\n"),
    ]

    with pytest.raises(pytest.fail.Exception) as info:
        assert_nodes_network(nodes, ["a", "b", "c"])

    message = str(info.value)
    assert "node-a" in message and "node-c" in message
    [(diag_nodes, _, _, _)] = diagnostics_spy
    assert [n.host for n in diag_nodes] == ["node-b"]
