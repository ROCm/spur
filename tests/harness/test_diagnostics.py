# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Unit tests for failure diagnostics: per-node collection, the live-cluster
registry, the report-section decision, and the k0s timeout log context.

Fake nodes stand in for SshNode only; every test drives the real harness code.
"""

import logging

import pytest

import cluster as cluster_module
from cluster import (
    SpurCluster,
    SshConnectionLost,
    collect_node_diagnostics,
    failure_diagnostic_sections,
    filter_k0s_log,
    live_clusters,
    register_cluster,
    unregister_cluster,
)


class FakeDiagNode:
    """Answers the one diagnostics script with canned output, or fails as told."""

    def __init__(self, host, output=None, error=None, exit_code=0):
        self.host = host
        self.address = host
        self.output = f"=== uptime ===\nup on {host}\n" if output is None else output
        self.error = error
        self.exit_code = exit_code
        self.commands = []
        self.timeouts = []

    def exec_with_exit(self, cmd, timeout=600):
        self.commands.append(cmd)
        self.timeouts.append(timeout)
        if self.error is not None:
            raise self.error
        return self.exit_code, self.output

    def exec_allow_fail(self, cmd, timeout=600):
        self.commands.append(cmd)
        self.timeouts.append(timeout)
        if self.error is not None:
            raise self.error
        return self.output


@pytest.fixture(autouse=True)
def empty_registry(monkeypatch):
    monkeypatch.setattr(cluster_module, "_live_clusters", [])


def make_cluster(nodes=None, remote_dir="/tmp/spur-test"):
    nodes = nodes if nodes is not None else [FakeDiagNode("node-a"), FakeDiagNode("node-b")]
    cluster = SpurCluster(nodes, remote_dir, "/tmp/spur-bin")
    cluster.node_names = [f"name-{node.host}" for node in nodes]
    return cluster


def test_collect_returns_one_entry_per_node_keyed_by_name_and_host():
    nodes = [FakeDiagNode("node-a"), FakeDiagNode("node-b")]

    result = collect_node_diagnostics(nodes, ["alpha", "beta"], "/r/log")

    assert list(result) == ["alpha (node-a)", "beta (node-b)"]
    assert "up on node-a" in result["alpha (node-a)"]
    assert "up on node-b" in result["beta (node-b)"]


def test_collect_runs_one_bounded_script_per_node():
    node = FakeDiagNode("node-a")

    collect_node_diagnostics([node], ["alpha"], "/r/log")

    assert len(node.commands) == 1
    assert node.timeouts == [20]


def test_collect_script_covers_logs_and_node_state_with_sections_that_cannot_fail_the_script():
    node = FakeDiagNode("node-a")

    collect_node_diagnostics([node], ["alpha"], "/r/log", max_lines=75)
    script = node.commands[0]

    for expected in (
        "tail -n 75 /r/log/spurctld.log",
        "tail -n 75 /r/log/spurd.log",
        "ip -br addr",
        "ip route",
        "sudo -n iptables -S",
        "sudo -n dmesg | tail -50",
        "sudo -n journalctl -u ssh -n 50",
        "uptime",
    ):
        assert expected in script
    commands = [part for part in script.split("; ") if not part.startswith("echo '===")]
    assert commands
    assert all(part.endswith("|| true") for part in commands)


def test_collect_script_never_uses_a_sudo_that_can_prompt():
    node = FakeDiagNode("node-a")

    collect_node_diagnostics([node], ["alpha"], "/r/log", k0s=True)

    script = node.commands[0]
    assert "sudo " not in script.replace("sudo -n ", "")


def test_collect_skips_log_tails_without_a_log_dir():
    node = FakeDiagNode("node-a")

    collect_node_diagnostics([node], ["alpha"], None)

    assert "spurctld.log" not in node.commands[0]
    assert "ip route" in node.commands[0]


def test_collect_adds_k0s_sections_only_when_asked():
    plain, with_k0s = FakeDiagNode("node-a"), FakeDiagNode("node-b")

    collect_node_diagnostics([plain], ["alpha"], "/r/log")
    collect_node_diagnostics([with_k0s], ["beta"], "/r/log", k0s=True)

    assert "k0s" not in plain.commands[0]
    for expected in ("sudo -n k0s status", "sudo -n k0s etcd member-list",
                     "systemctl status k0scontroller"):
        assert expected in with_k0s.commands[0]


def test_collect_records_an_unreachable_node_and_still_returns_the_healthy_ones():
    lost = SshConnectionLost("SSH session to node-b lost and 3 reconnect attempts failed")
    nodes = [FakeDiagNode("node-a"), FakeDiagNode("node-b", error=lost), FakeDiagNode("node-c")]

    result = collect_node_diagnostics(nodes, ["alpha", "beta", "gamma"], "/r/log")

    assert list(result) == ["alpha (node-a)", "beta (node-b)", "gamma (node-c)"]
    assert result["beta (node-b)"].startswith("node-b: unreachable")
    assert "reconnect attempts failed" in result["beta (node-b)"]
    assert "up on node-a" in result["alpha (node-a)"]
    assert "up on node-c" in result["gamma (node-c)"]


def test_collect_notes_a_timeout_and_keeps_the_partial_output():
    node = FakeDiagNode("node-a", output="=== ip route ===\ndefault via 10.0.0.1\n", exit_code=-1)

    result = collect_node_diagnostics([node], ["alpha"], "/r/log")

    text = result["alpha (node-a)"]
    assert "default via 10.0.0.1" in text
    assert "node-a: timed out after 20s" in text


def test_collect_records_an_unexpected_error_instead_of_raising():
    node = FakeDiagNode("node-a", error=ValueError("boom"))

    result = collect_node_diagnostics([node], ["alpha"], "/r/log")

    assert "node-a: diagnostics failed" in result["alpha (node-a)"]
    assert "boom" in result["alpha (node-a)"]


def test_collect_falls_back_to_the_host_when_names_are_unresolved():
    result = collect_node_diagnostics([FakeDiagNode("node-a")], [], "/r/log")

    assert list(result) == ["node-a (node-a)"]


def test_cluster_collect_diagnostics_uses_its_log_dir_names_and_line_limit():
    cluster = make_cluster()

    result = cluster.collect_diagnostics(max_lines=50)

    assert list(result) == ["name-node-a (node-a)", "name-node-b (node-b)"]
    assert "tail -n 50 /tmp/spur-test/log/spurctld.log" in cluster.nodes[0].commands[0]
    assert "k0s" not in cluster.nodes[0].commands[0]


def test_cluster_collect_diagnostics_defaults_to_200_lines():
    cluster = make_cluster()

    cluster.collect_diagnostics()

    assert "tail -n 200 " in cluster.nodes[0].commands[0]


def test_cluster_collect_diagnostics_can_include_k0s_sections():
    cluster = make_cluster()

    cluster.collect_diagnostics(k0s=True)

    assert "k0s status" in cluster.nodes[0].commands[0]


def test_cluster_collect_diagnostics_survives_a_lost_node():
    nodes = [FakeDiagNode("node-a", error=SshConnectionLost("gone")), FakeDiagNode("node-b")]
    cluster = make_cluster(nodes)

    result = cluster.collect_diagnostics()

    assert result["name-node-a (node-a)"].startswith("node-a: unreachable")
    assert "up on node-b" in result["name-node-b (node-b)"]


def test_registry_tracks_clusters_until_they_are_unregistered():
    first, second = make_cluster(), make_cluster()

    register_cluster(first)
    register_cluster(second)
    assert live_clusters() == [first, second]

    unregister_cluster(first)
    assert live_clusters() == [second]


def test_registry_ignores_double_registration_and_double_unregistration():
    cluster = make_cluster()

    register_cluster(cluster)
    register_cluster(cluster)
    assert live_clusters() == [cluster]

    unregister_cluster(cluster)
    unregister_cluster(cluster)
    assert live_clusters() == []


def test_provision_registers_the_cluster_and_teardown_unregisters_it(monkeypatch):
    cluster = make_cluster()
    monkeypatch.setattr(SpurCluster, "_create_dirs", lambda self: None)
    monkeypatch.setattr(SpurCluster, "_resolve_hostnames", lambda self: None)
    monkeypatch.setattr(SpurCluster, "stop", lambda self: None)

    cluster.provision()
    assert live_clusters() == [cluster]

    cluster.teardown()
    assert live_clusters() == []


def test_teardown_unregisters_even_when_stopping_fails(monkeypatch):
    cluster = make_cluster()
    register_cluster(cluster)

    def fail(self):
        raise SshConnectionLost("dropped")

    monkeypatch.setattr(SpurCluster, "stop", fail)

    with pytest.raises(SshConnectionLost):
        cluster.teardown()

    assert live_clusters() == []


def test_sections_attach_for_a_failed_call_phase():
    cluster = make_cluster()

    sections = failure_diagnostic_sections("call", True, ["cluster"], [cluster], None)

    assert [title for title, _ in sections] == [
        "diagnostics: name-node-a (node-a)",
        "diagnostics: name-node-b (node-b)",
    ]
    assert "up on node-a" in sections[0][1]


def test_sections_attach_for_a_failed_setup_phase():
    cluster = make_cluster()

    assert failure_diagnostic_sections("setup", True, ["cluster"], [cluster], None)


@pytest.mark.parametrize("when,failed", [
    ("call", False),
    ("setup", False),
    ("teardown", True),
    ("teardown", False),
])
def test_sections_skip_passes_skips_xfails_and_teardown_failures(when, failed):
    cluster = make_cluster()

    assert failure_diagnostic_sections(when, failed, ["cluster"], [cluster], None) == []
    assert cluster.nodes[0].commands == []


def test_sections_include_k0s_extras_for_a_k8s_fixture():
    cluster = make_cluster()

    failure_diagnostic_sections("call", True, ["ssh_nodes", "k8s_multicp_cluster"], [cluster], None)

    assert "k0s status" in cluster.nodes[0].commands[0]


def test_sections_leave_out_k0s_extras_for_other_fixtures():
    cluster = make_cluster()

    failure_diagnostic_sections("call", True, ["ssh_nodes", "cluster", "wg_k0s_cluster"], [cluster], None)

    assert "k0s status" not in cluster.nodes[0].commands[0]


def test_sections_cover_every_live_cluster():
    first, second = make_cluster(), make_cluster()

    sections = failure_diagnostic_sections("call", True, ["cluster"], [first, second], None)

    assert len(sections) == 4
    assert first.nodes[0].commands and second.nodes[0].commands


def test_sections_fall_back_to_node_level_diagnostics_when_no_cluster_is_live():
    nodes = [FakeDiagNode("node-a"), FakeDiagNode("node-b")]

    sections = failure_diagnostic_sections("call", True, ["ssh_nodes"], [], nodes)

    assert [title for title, _ in sections] == [
        "diagnostics: node-a (node-a)",
        "diagnostics: node-b (node-b)",
    ]
    assert "spurctld.log" not in nodes[0].commands[0]
    assert "ip route" in nodes[0].commands[0]


def test_sections_are_empty_without_a_live_cluster_or_nodes():
    assert failure_diagnostic_sections("call", True, ["cluster"], [], None) == []


def test_sections_keep_collecting_when_one_node_is_unreachable():
    nodes = [FakeDiagNode("node-a", error=SshConnectionLost("gone")), FakeDiagNode("node-b")]
    cluster = make_cluster(nodes)

    sections = failure_diagnostic_sections("call", True, ["cluster"], [cluster], None)

    assert len(sections) == 2
    assert "unreachable" in sections[0][1]


K0S_LOG = "\n".join([
    "INFO spurctld started",
    "WARN K0s controller join token expired",
    "INFO heartbeat ok",
    "INFO raft: election won",
    "INFO join-token minted",
    "INFO Leader changed",
    "INFO stopping k0s",
    "INFO job 7 completed",
])


def test_filter_keeps_only_k0s_join_token_raft_leader_and_stop_lines():
    assert filter_k0s_log(K0S_LOG).splitlines() == [
        "WARN K0s controller join token expired",
        "INFO raft: election won",
        "INFO join-token minted",
        "INFO Leader changed",
        "INFO stopping k0s",
    ]


def test_filter_keeps_the_last_matching_lines_up_to_the_limit():
    log = "\n".join(f"raft line {i}" for i in range(100))

    kept = filter_k0s_log(log, limit=60).splitlines()

    assert len(kept) == 60
    assert kept[0] == "raft line 40" and kept[-1] == "raft line 99"


def test_filter_is_empty_when_nothing_matches():
    assert filter_k0s_log("INFO all quiet\nINFO job done") == ""


def test_wait_k8s_phase_timeout_appends_the_controller_log_after_the_original_message(monkeypatch):
    node = FakeDiagNode("node-a", output=K0S_LOG)
    cluster = make_cluster([node])
    monkeypatch.setattr(SpurCluster, "k8s_status", lambda self: "phase: degraded")

    with pytest.raises(TimeoutError) as excinfo:
        cluster.wait_k8s_phase("ready", timeout=0)

    message = str(excinfo.value)
    assert message.startswith("k0s did not reach phase ready within 0s:\n")
    assert "spurctld.log" in message
    assert "INFO raft: election won" in message
    assert "INFO heartbeat ok" not in message


def test_wait_k8s_phase_timeout_keeps_its_message_when_the_log_cannot_be_read(monkeypatch):
    node = FakeDiagNode("node-a", error=SshConnectionLost("gone"))
    cluster = make_cluster([node])
    monkeypatch.setattr(SpurCluster, "k8s_status", lambda self: "phase: degraded")

    with pytest.raises(TimeoutError) as excinfo:
        cluster.wait_k8s_phase("ready", timeout=0)

    assert str(excinfo.value).startswith("k0s did not reach phase ready within 0s:\n")
    assert "unavailable" in str(excinfo.value) and "gone" in str(excinfo.value)


def test_wait_etcd_members_timeout_logs_the_controller_log_and_still_returns_the_count(
        monkeypatch, caplog):
    node = FakeDiagNode("node-a", output=K0S_LOG)
    cluster = make_cluster([node])
    monkeypatch.setattr(SpurCluster, "etcd_member_count", lambda self: 1)

    with caplog.at_level(logging.WARNING, logger="cluster"):
        assert cluster.wait_etcd_members(3, timeout=0) == 0

    assert "etcd did not reach 3 members" in caplog.text
    assert "INFO raft: election won" in caplog.text


def test_wait_etcd_members_success_does_not_read_the_log(monkeypatch):
    node = FakeDiagNode("node-a")
    cluster = make_cluster([node])
    monkeypatch.setattr(SpurCluster, "etcd_member_count", lambda self: 3)

    assert cluster.wait_etcd_members(3, timeout=5) == 3
    assert node.commands == []
