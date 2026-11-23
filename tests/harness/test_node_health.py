# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Unit tests for the per-test node health guard and the partition-helper guard.

Fake nodes stand in for SshNode only; every test drives the real harness functions.
"""

import pytest

from cluster import (
    CLI_NODE_INDEX,
    SshConnectionLost,
    block_heartbeats_from,
    check_nodes_reachable,
    cut_off_node,
    leftover_drop_rules,
    reap_leftover_drop_rules,
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
                 list_exit=0):
        self.host = host
        self.address = host
        self.iptables_s = iptables_s
        self.reconnect_error = reconnect_error
        self.fail_delete = fail_delete
        self.list_exit = list_exit
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


def test_reap_removes_each_leaked_rule_and_reports_it():
    leaky = FakeNode("node-a", IPTABLES_S)
    clean = FakeNode("node-b")

    report = reap_leftover_drop_rules([leaky, clean])

    assert [line.split(": ")[0] for line in report] == ["node-a", "node-a"]
    assert all(rule.removeprefix("-A ") in line for rule, line in zip(LEAKED, report))
    assert all(line.endswith("removed") for line in report)
    assert leftover_drop_rules(leaky) == []
    assert clean.commands == ["sudo iptables -S"]


def test_reap_reports_removal_failures():
    node = FakeNode("node-a", IPTABLES_S, fail_delete=True)

    report = reap_leftover_drop_rules([node])

    assert len(report) == 2
    assert all("removal failed" in line and "Bad rule" in line for line in report)


def test_reap_reports_nothing_when_all_clean():
    assert reap_leftover_drop_rules([FakeNode("node-a"), FakeNode("node-b")]) == []


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
