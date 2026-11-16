# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""scontrol node deletion uses the native removal RPC and keeps the node out
until its spurd restarts."""

import time

import pytest

from cluster import (
    job_state, parse_job_id, wait_job_state, wait_node_gone, wait_node_state, wait_until,
)


@pytest.fixture
def departed_cluster(cluster):
    cluster.stop_agents()
    for name in cluster.node_names:
        wait_node_state(cluster, name, ["down"])
    return cluster


@pytest.mark.parametrize("params", [
    ("NodeName={name}",),
    ("nodename={name}",),
    ("nOdEnAmE={name}",),
    ("node={name}",),
    ("node", "{name}"),
    ("NOD", "{name}"),
])
def test_delete_departed_node(departed_cluster, params):
    cluster = departed_cluster
    name = cluster.node_names[0]
    before = set(cluster.sinfo_nodes())

    out = cluster.scontrol("delete", *(arg.format(name=name) for arg in params))

    assert f"Node {name} removed from cluster" in out, out
    wait_node_gone(cluster, name)
    assert set(cluster.sinfo_nodes()) == before - {name}


def test_delete_duplicate_hostlist_targets_once(departed_cluster):
    cluster = departed_cluster
    names = cluster.node_names
    targets = ",".join([names[0], *names, names[0]])

    out = cluster.scontrol("delete", f"NodeName={targets}")

    for name in names:
        assert out.count(f"Node {name} removed from cluster") == 1, out
    assert cluster.sinfo_nodes() == {}


@pytest.mark.parametrize("params", [("NodeName=ALL",), ("NODE", "all")])
def test_delete_all_and_reject_empty_cluster(departed_cluster, params):
    cluster = departed_cluster
    before = set(cluster.sinfo_nodes())
    assert before

    out = cluster.scontrol("delete", *params)

    for name in before:
        assert f"Node {name} removed from cluster" in out, out
    assert cluster.sinfo_nodes() == {}
    code, out = cluster.cli_with_exit(["scontrol", "delete", *params])
    assert code != 0, out
    assert "no nodes registered" in out, out


def test_delete_continues_after_unknown_node(departed_cluster):
    cluster = departed_cluster
    name = cluster.node_names[0]
    missing = "scontrol-delete-missing-node"
    before = set(cluster.sinfo_nodes())
    assert missing not in before

    code, out = cluster.cli_with_exit([
        "scontrol", "delete", "node", f"{missing},{name}",
    ])

    assert code != 0, out
    assert f"node {missing} not found" in out, out
    assert f"Node {name} removed from cluster" in out, out
    assert "failed on 1 of 2 node(s)" in out, out
    assert set(cluster.sinfo_nodes()) == before - {name}


DELETED = "was deleted; restart its spurd to add it back"
REREGISTERED = "controller does not recognize this node"


def _wait_told_deleted(cluster, index):
    wait_until(
        lambda: DELETED in cluster.spurd_log(index),
        timeout_s=90, interval_s=2, desc="running agent told it was deleted",
    )
    assert REREGISTERED not in cluster.spurd_log(index)


def _delete_last_node(cluster):
    index = len(cluster.nodes) - 1
    name = cluster.node_names[index]
    out = cluster.scontrol("delete", f"NodeName={name}")
    assert f"Node {name} removed from cluster" in out, out
    wait_node_gone(cluster, name)
    return index, name


def test_deleted_node_stays_out_until_its_agent_restarts(cluster):
    index, name = _delete_last_node(cluster)

    _wait_told_deleted(cluster, index)
    assert name not in cluster.sinfo_nodes()

    cluster.restart_agent(index)
    wait_node_state(cluster, name, ["idle"])


def test_deleted_node_stays_out_across_a_controller_restart(cluster):
    if cluster.ha_controller_count > 1:
        pytest.skip("restart_controller() supports one controller")
    index, name = _delete_last_node(cluster)
    _wait_told_deleted(cluster, index)

    cluster.restart_controller(expected_nodes=[n for n in cluster.node_names if n != name])
    # One full spurd heartbeat interval, so the restarted controller has answered one.
    time.sleep(35)
    assert name not in cluster.sinfo_nodes()
    assert REREGISTERED not in cluster.spurd_log(index)

    cluster.restart_agent(index)
    wait_node_state(cluster, name, ["idle"])


def test_deleted_departed_node_returns_when_its_agent_starts(departed_cluster):
    cluster = departed_cluster
    index, name = _delete_last_node(cluster)

    cluster.restart_agent(index)

    wait_node_state(cluster, name, ["idle"])


def test_forced_delete_stops_the_job_and_keeps_the_agent_out(cluster):
    index = len(cluster.nodes) - 1
    name = cluster.node_names[index]
    seconds = 600000 + int(time.time()) % 100000
    out = cluster.sbatch([
        "-N", "1", "-w", name, "-t", "10", "--wrap", f"sleep {seconds}",
        "-o", f"{cluster.remote_dir}/delete-forced.out",
    ])
    job_id = parse_job_id(out)
    assert job_id is not None, out
    try:
        wait_job_state(cluster, job_id, "R")
        cluster.cli(["spur", "node", "remove", name, "--force"])
        wait_node_gone(cluster, name)
        wait_until(
            lambda: not cluster.nodes[index].exec_allow_fail(
                f"pgrep -f '[s]leep {seconds}' || true"
            ).strip(),
            timeout_s=60, interval_s=2, desc="job processes on the removed node stopped",
        )
        _wait_told_deleted(cluster, index)
        assert name not in cluster.sinfo_nodes()
    finally:
        cluster.cli_allow_fail(["scancel", str(job_id)])

    cluster.restart_agent(index)
    wait_node_state(cluster, name, ["idle"])


@pytest.mark.parametrize("key,separator", [("NodeName", "="), ("node", " ")])
def test_delete_refuses_busy_node_and_continues(multi_node_cluster, key, separator):
    cluster = multi_node_cluster
    busy, departed = cluster.node_names[:2]
    before = set(cluster.sinfo_nodes())
    cluster._pkill(cluster.nodes[1], f"{cluster.bin_dir}/spurd")
    wait_node_state(cluster, departed, ["down"])
    out = cluster.sbatch([
        "-N", "1", "-w", busy, "-t", "10", "--wrap", "sleep 600",
        "-o", f"{cluster.remote_dir}/delete-busy.out",
    ])
    job_id = parse_job_id(out)
    assert job_id is not None, out

    try:
        wait_job_state(cluster, job_id, "R")
        targets = f"{busy},{departed}"
        params = [f"{key}={targets}"] if separator == "=" else [key, targets]
        code, out = cluster.cli_with_exit(["scontrol", "delete", *params])

        assert code != 0, out
        assert "running jobs" in out.lower(), out
        assert f"Node {departed} removed from cluster" in out, out
        assert "failed on 1 of 2 node(s)" in out, out
        assert set(cluster.sinfo_nodes()) == before - {departed}
        queue = cluster.squeue_all()
        assert job_state(queue, job_id) == "R", queue
    finally:
        cluster.cli_allow_fail(["scancel", str(job_id)])


def test_delete_rejects_invalid_arguments_without_removing_nodes(cluster):
    name = cluster.node_names[0]
    before = set(cluster.sinfo_nodes())
    for params in [
        [], ["node"], ["NodeName="], ["node", ""],
        [f"no={name}"], [f"Nodes={name}"],
        [f"NodeName={name}", "PartitionName=default"],
        ["node", f"NodeName={name}"],
        [f"NodeName={name}", name],
        ["node", name, name],
        [f"NodeName={name},"], [f"NodeName={name}[]"], ["node", f"{name}[1,]"],
        ["NodeName=n[2-1]"], [f"NodeName={name}", "--force"],
        [f"NodeName={name}", "--reason", "decommissioned"],
    ]:
        code, out = cluster.cli_with_exit(["scontrol", "delete", *params])
        assert code != 0, (params, out)
        assert set(cluster.sinfo_nodes()) == before, (params, out)
