# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""scontrol node deletion dispatches through the existing native removal RPC."""

import pytest

from cluster import job_state, parse_job_id, wait_job_state, wait_node_gone, wait_node_state


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
