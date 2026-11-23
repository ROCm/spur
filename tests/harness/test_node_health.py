# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Unit tests for the per-test node health guard and the partition-helper guard.

Fake nodes stand in for SshNode only; every test drives the real harness functions.
"""

import logging

import pytest

import cluster as cluster_module
from cluster import (
    CLI_NODE_INDEX,
    SshConnectionLost,
    block_heartbeats_from,
    check_nodes_reachable,
    cut_off_node,
    leftover_drop_rules,
    node_health_guard,
    reap_leaked_drop_rules,
    snapshot_drop_rules,
    unreachable_message,
)

IPTABLES_S = """\
-P INPUT ACCEPT
-P FORWARD DROP
-P OUTPUT ACCEPT
-N DOCKER-USER
-A INPUT -s 10.0.0.2/32 -p tcp -m tcp --dport 6817 -j DROP
-A INPUT -p tcp -m tcp --dport 6817 -j ACCEPT
-A INPUT -p tcp -m tcp --dport 22 -j DROP
-A OUTPUT -d 10.0.0.3/32 -p tcp -m tcp --dport 6818 -j DROP
-A OUTPUT -d 10.0.0.3/32 -p tcp -m tcp --dport 68180 -j DROP
-A FORWARD -j DOCKER-USER
"""

LEAKED = [
    "-A INPUT -s 10.0.0.2/32 -p tcp -m tcp --dport 6817 -j DROP",
    "-A OUTPUT -d 10.0.0.3/32 -p tcp -m tcp --dport 6818 -j DROP",
]


class FakeNode:
    """Answers `iptables -S` with canned text and records every command."""

    def __init__(self, host, iptables_s="", reconnect_error=None, fail_delete=False,
                 list_exit=0, list_error=None):
        self.host = host
        self.address = host
        self.iptables_s = iptables_s
        self.reconnect_error = reconnect_error
        self.fail_delete = fail_delete
        self.list_exit = list_exit
        self.list_error = list_error
        self.commands = []
        self.timeouts = []
        self.connect_checks = 0

    def ensure_connected(self):
        self.connect_checks += 1
        if self.reconnect_error is not None:
            raise self.reconnect_error

    def exec_with_exit(self, cmd, timeout=600):
        self.commands.append(cmd)
        self.timeouts.append(timeout)
        if cmd == "sudo iptables -S":
            if self.list_error is not None:
                raise self.list_error
            return self.list_exit, self.iptables_s
        if cmd.startswith("sudo iptables -D"):
            if self.fail_delete:
                return 1, "iptables: Bad rule"
            rule = "-A " + cmd.removeprefix("sudo iptables -D ")
            self.iptables_s = self.iptables_s.replace(rule + "\n", "")
            return 0, ""
        raise AssertionError(f"unexpected command: {cmd}")

    def exec(self, cmd, **kwargs):
        raise AssertionError(f"helper ran a command before its guard: {cmd}")

    exec_allow_fail = exec


@pytest.fixture(autouse=True)
def fresh_warning_state(monkeypatch):
    monkeypatch.setattr(cluster_module, "_leak_check_warned", set())


class FakeCluster:
    def __init__(self, count=3):
        self.nodes = [FakeNode(f"node-{i}") for i in range(count)]


def test_cli_node_is_the_controller_node():
    assert CLI_NODE_INDEX == 0


def test_leftover_drop_rules_parses_only_spur_port_drops():
    node = FakeNode("node-a", IPTABLES_S)

    assert leftover_drop_rules(node) == LEAKED
    assert node.commands == ["sudo iptables -S"]


def test_leftover_drop_rules_is_bounded_by_a_short_timeout():
    node = FakeNode("node-a", IPTABLES_S)

    leftover_drop_rules(node)

    assert node.timeouts == [20]


def test_leftover_drop_rules_empty_when_clean():
    node = FakeNode("node-a", "-P INPUT ACCEPT\n-P OUTPUT ACCEPT\n")

    assert leftover_drop_rules(node) == []


def test_leftover_drop_rules_raises_when_iptables_cannot_be_listed():
    node = FakeNode("node-a", "sudo: a password is required", list_exit=1)

    with pytest.raises(RuntimeError, match="node-a.*password is required"):
        leftover_drop_rules(node)


def test_reap_removes_rules_added_since_the_baseline_and_reports_them():
    node = FakeNode("node-a", "-P INPUT ACCEPT\n")
    baseline = snapshot_drop_rules([node])
    node.iptables_s = IPTABLES_S

    report = reap_leaked_drop_rules([node], baseline)

    assert len(report) == 2
    assert all(rule.removeprefix("-A ") in line for rule, line in zip(LEAKED, report))
    assert all(line.startswith("node-a: ") and line.endswith("removed") for line in report)
    assert leftover_drop_rules(node) == []


def test_reap_leaves_rules_that_predate_the_test_alone():
    node = FakeNode("node-a", IPTABLES_S)
    baseline = snapshot_drop_rules([node])

    assert reap_leaked_drop_rules([node], baseline) == []

    assert not any(cmd.startswith("sudo iptables -D") for cmd in node.commands)
    assert leftover_drop_rules(node) == LEAKED


def test_reap_removes_only_the_new_rule_among_preexisting_ones():
    node = FakeNode("node-a", LEAKED[0] + "\n")
    baseline = snapshot_drop_rules([node])
    node.iptables_s = IPTABLES_S

    report = reap_leaked_drop_rules([node], baseline)

    assert len(report) == 1 and LEAKED[1].removeprefix("-A ") in report[0]
    assert leftover_drop_rules(node) == [LEAKED[0]]


def test_reap_removes_a_duplicate_of_a_preexisting_rule():
    node = FakeNode("node-a", LEAKED[0] + "\n")
    baseline = snapshot_drop_rules([node])
    node.iptables_s = LEAKED[0] + "\n" + LEAKED[0] + "\n"

    report = reap_leaked_drop_rules([node], baseline)

    assert len(report) == 1


def test_reap_reports_removal_failures():
    node = FakeNode("node-a", fail_delete=True)
    baseline = snapshot_drop_rules([node])
    node.iptables_s = IPTABLES_S

    report = reap_leaked_drop_rules([node], baseline)

    assert len(report) == 2
    assert all("removal failed" in line and "Bad rule" in line for line in report)


def test_reap_reports_nothing_when_all_clean():
    nodes = [FakeNode("node-a"), FakeNode("node-b")]

    assert reap_leaked_drop_rules(nodes, snapshot_drop_rules(nodes)) == []


@pytest.mark.parametrize("error", [
    SshConnectionLost("SSH session to node-a lost"),
    RuntimeError("`sudo iptables -S` failed on node-a"),
])
def test_reap_continues_to_later_nodes_when_an_earlier_one_fails(error):
    first = FakeNode("node-a")
    second = FakeNode("node-b")
    nodes = [first, second]
    baseline = snapshot_drop_rules(nodes)
    first.list_error = error
    second.iptables_s = IPTABLES_S

    report = reap_leaked_drop_rules(nodes, baseline)

    assert report[0] == f"node-a: could not check for leaked rules: {error}"
    assert len(report) == 3
    assert all(line.startswith("node-b: ") and line.endswith("removed") for line in report[1:])
    assert leftover_drop_rules(second) == []


def test_reap_continues_when_a_removal_loses_the_session():
    first = FakeNode("node-a")
    second = FakeNode("node-b")
    nodes = [first, second]
    baseline = snapshot_drop_rules(nodes)
    first.iptables_s = IPTABLES_S
    second.iptables_s = IPTABLES_S
    real = first.exec_with_exit

    def lose_on_delete(cmd, timeout=600):
        if cmd.startswith("sudo iptables -D"):
            raise SshConnectionLost("dropped")
        return real(cmd, timeout)

    first.exec_with_exit = lose_on_delete

    report = reap_leaked_drop_rules(nodes, baseline)

    assert sum("removal failed" in line and "dropped" in line for line in report) == 2
    assert leftover_drop_rules(second) == []


def test_snapshot_records_each_nodes_current_rules():
    nodes = [FakeNode("node-a", IPTABLES_S), FakeNode("node-b")]

    baseline = snapshot_drop_rules(nodes)

    assert sorted(baseline[0].elements()) == sorted(LEAKED)
    assert not baseline[1]


def test_snapshot_failure_disables_the_leak_check_for_that_node(caplog):
    broken = FakeNode("node-a", "sudo: a password is required", list_exit=1)
    healthy = FakeNode("node-b")
    nodes = [broken, healthy]

    with caplog.at_level(logging.WARNING, logger="cluster"):
        baseline = snapshot_drop_rules(nodes)

    assert baseline[0] is None and baseline[1] is not None
    assert "node-a" in caplog.text and "password is required" in caplog.text
    broken.iptables_s = IPTABLES_S
    broken.list_exit = 0
    broken.commands.clear()

    assert reap_leaked_drop_rules(nodes, baseline) == []
    assert broken.commands == []


def test_snapshot_failure_is_warned_once_per_host_per_session(caplog):
    node = FakeNode("node-a", "no sudo", list_exit=1)

    with caplog.at_level(logging.WARNING, logger="cluster"):
        snapshot_drop_rules([node])
        snapshot_drop_rules([node])

    assert caplog.text.count("leak detection is disabled on node-a") == 1


def test_guard_passes_and_removes_nothing_when_the_test_leaks_nothing():
    node = FakeNode("node-a", IPTABLES_S)

    with node_health_guard([node]):
        pass

    assert not any(cmd.startswith("sudo iptables -D") for cmd in node.commands)


def test_guard_fails_the_test_that_leaked_after_removing_the_rules():
    first, second = FakeNode("node-a"), FakeNode("node-b")

    with pytest.raises(pytest.fail.Exception) as excinfo:
        with node_health_guard([first, second]):
            second.iptables_s = IPTABLES_S

    assert "node-b" in str(excinfo.value) and "removed" in str(excinfo.value)
    assert leftover_drop_rules(second) == []


def test_guard_does_not_error_when_iptables_is_unavailable():
    node = FakeNode("node-a", "no sudo", list_exit=1)

    with node_health_guard([node]):
        node.list_exit = 0
        node.iptables_s = IPTABLES_S

    assert not any(cmd.startswith("sudo iptables -D") for cmd in node.commands)


def test_guard_exits_the_shard_before_the_test_when_a_node_stays_unreachable():
    lost = SshConnectionLost("reconnect refused")
    nodes = [FakeNode("node-a"), FakeNode("node-b", reconnect_error=lost)]
    ran = []

    with pytest.raises(pytest.exit.Exception) as excinfo:
        with node_health_guard(nodes):
            ran.append(True)

    assert excinfo.value.returncode == 3
    assert "node-b" in str(excinfo.value) and "reconnect refused" in str(excinfo.value)
    assert not ran
    assert nodes[0].commands == []


def test_check_nodes_reachable_lists_unreachable_hosts_with_their_errors():
    lost = SshConnectionLost("SSH session to node-b lost and 3 reconnect attempts failed")
    nodes = [FakeNode("node-a"), FakeNode("node-b", reconnect_error=lost)]

    unreachable = check_nodes_reachable(nodes)

    assert list(unreachable) == ["node-b"]
    assert unreachable["node-b"] is lost
    assert [n.connect_checks for n in nodes] == [1, 1]


def test_check_nodes_reachable_checks_every_node_after_a_failure():
    lost = SshConnectionLost("gone")
    nodes = [FakeNode("node-a", reconnect_error=lost), FakeNode("node-b", reconnect_error=lost)]

    assert list(check_nodes_reachable(nodes)) == ["node-a", "node-b"]


def test_check_nodes_reachable_empty_when_all_up():
    assert not check_nodes_reachable([FakeNode("node-a"), FakeNode("node-b")])


def test_unreachable_message_names_hosts_and_original_errors():
    msg = unreachable_message({
        "node-a": SshConnectionLost("reconnect refused"),
        "node-c": SshConnectionLost("timed out"),
    })

    assert "node-a" in msg and "reconnect refused" in msg
    assert "node-c" in msg and "timed out" in msg
    assert "aborting shard" in msg


@pytest.mark.parametrize("helper", [block_heartbeats_from, cut_off_node])
def test_partition_helpers_refuse_the_cli_node_before_running_anything(helper):
    cluster = FakeCluster()

    with pytest.raises(ValueError, match="node 0"):
        with helper(cluster, CLI_NODE_INDEX):
            pytest.fail("body must not run")


@pytest.mark.parametrize("helper", [block_heartbeats_from, cut_off_node])
def test_partition_helpers_still_accept_other_nodes(helper):
    cluster = FakeCluster()
    recorded = []
    for node in cluster.nodes:
        node.exec = lambda cmd, _node=node, **kw: recorded.append((_node.host, cmd)) or ""
        node.exec_allow_fail = node.exec

    with helper(cluster, 1):
        pass

    assert recorded
    assert all("iptables" in cmd for _, cmd in recorded)
