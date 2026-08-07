# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for topology-aware placement.

`[topology]` is read once when the scheduler loop starts, so every test here
brings up a cluster with the config it needs rather than reconfiguring. Only
multi-node jobs that opt in with `--topology` are reordered; everything else
must be unaffected, which is what most of these tests pin down.
"""

import re

import pytest

from cluster import expand_hostlist, parse_job_id, wait_job, wait_job_state


def switches_for(cluster) -> list[dict]:
    """One leaf switch holding node 0, another holding the rest."""
    names = cluster.node_names
    return [
        {"name": "rack0", "nodes": names[0]},
        {"name": "rack1", "nodes": ",".join(names[1:])},
    ]


@pytest.fixture
def tree_cluster(unstarted_cluster):
    if len(unstarted_cluster.nodes) < 3:
        pytest.skip(
            "topology placement needs at least 3 nodes so one leaf switch can "
            "hold more than one node"
        )
    unstarted_cluster.start(
        config_overrides={
            "topology": {"plugin": "tree", "switches": switches_for(unstarted_cluster)}
        }
    )
    return unstarted_cluster


def job_nodes(cluster, job_id: int) -> set[str]:
    out = cluster.scontrol("show", "job", str(job_id))
    match = re.search(r"NodeList=(\S+)", out)
    assert match, f"scontrol reported no NodeList for job {job_id}:\n{out}"
    return set(expand_hostlist(match.group(1)))


def submit_multinode(cluster, name: str, nodes: int, extra: list[str]) -> int:
    script = cluster.write_file(f"{name}.sh", "#!/bin/bash\nsleep 60\n")
    job_id = parse_job_id(
        cluster.sbatch(["-J", name, "-N", str(nodes)] + extra + [script])
    )
    assert job_id is not None
    return job_id


class TestTopologyLoading:
    def test_the_tree_plugin_is_loaded_at_startup(self, tree_cluster):
        log = tree_cluster.spurctld_log()
        assert "topology/tree loaded" in log, log[-3000:]

    def test_the_block_plugin_is_loaded_at_startup(self, unstarted_cluster):
        unstarted_cluster.start(
            config_overrides={"topology": {"plugin": "block", "block_size": 2}}
        )
        log = unstarted_cluster.spurctld_log()
        assert "topology/block loaded" in log, log[-3000:]

    def test_no_topology_is_loaded_by_default(self, cluster):
        log = cluster.spurctld_log()
        assert "topology/" not in log, (
            f"topology must stay off unless configured:\n{log[-3000:]}"
        )

    def test_an_unknown_plugin_leaves_the_scheduler_running(self, unstarted_cluster):
        """A typo in the plugin name falls back to no topology rather than
        refusing to schedule, so jobs must keep flowing."""
        unstarted_cluster.start(
            config_overrides={"topology": {"plugin": "nonesuch"}}
        )
        code, out = unstarted_cluster.srun_with_exit(["echo", "TOPO_FALLBACK_OK"])
        assert code == 0, out
        assert "TOPO_FALLBACK_OK" in out, out


class TestPlacement:
    def test_a_two_node_job_packs_into_one_switch(self, tree_cluster):
        """The point of the tree plugin: co-locate a job's nodes behind one
        leaf so its traffic never crosses the spine."""
        job_id = submit_multinode(tree_cluster, "topo-pack", 2, ["--topology=tree"])
        try:
            wait_job_state(tree_cluster, job_id, "R", timeout=90)
            rack1 = set(tree_cluster.node_names[1:])
            placed = job_nodes(tree_cluster, job_id)
            assert placed <= rack1, (
                f"a 2-node job must land inside rack1 {sorted(rack1)}, "
                f"got {sorted(placed)}"
            )
        finally:
            tree_cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_the_block_flag_uses_the_same_placement(self, tree_cluster):
        job_id = submit_multinode(tree_cluster, "topo-block", 2, ["--topology=block"])
        try:
            wait_job_state(tree_cluster, job_id, "R", timeout=90)
            placed = job_nodes(tree_cluster, job_id)
            assert placed <= set(tree_cluster.node_names[1:]), (
                f"--topology=block must pack like tree, got {sorted(placed)}"
            )
        finally:
            tree_cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_a_job_spanning_switches_still_schedules(self, tree_cluster):
        """No single leaf is big enough, so the job has to fall back to
        crossing switches rather than pending forever."""
        count = len(tree_cluster.node_names)
        job_id = submit_multinode(tree_cluster, "topo-span", count, ["--topology=tree"])
        try:
            wait_job_state(tree_cluster, job_id, "R", timeout=120)
            assert job_nodes(tree_cluster, job_id) == set(tree_cluster.node_names)
        finally:
            tree_cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_an_explicit_nodelist_still_wins(self, tree_cluster):
        """Topology reorders candidates; it must not override an operator who
        named the nodes outright."""
        target = tree_cluster.node_names[0]
        job_id = submit_multinode(
            tree_cluster, "topo-w", 1, ["--topology=tree", "-w", target]
        )
        try:
            wait_job_state(tree_cluster, job_id, "R", timeout=90)
            assert job_nodes(tree_cluster, job_id) == {target}
        finally:
            tree_cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_a_single_node_job_is_unaffected(self, tree_cluster):
        script = tree_cluster.write_file("topo-one.sh", "#!/bin/bash\necho TOPO_ONE_OK\n")
        out_path = f"{tree_cluster.remote_dir}/topo-one.out"
        job_id = parse_job_id(
            tree_cluster.sbatch(
                ["-J", "topo-one", "-N", "1", "--topology=tree", "-o", out_path, script]
            )
        )
        assert job_id is not None
        assert wait_job(tree_cluster, job_id, timeout=120) in ("CD", "GONE"), (
            tree_cluster.debug_job(job_id)
        )
        assert "TOPO_ONE_OK" in tree_cluster.read_output_on_any_node(out_path)

    def test_a_job_without_the_flag_still_schedules(self, tree_cluster):
        """Cluster topology alone must not change placement; the job has to
        opt in."""
        job_id = submit_multinode(tree_cluster, "topo-optout", 2, [])
        try:
            wait_job_state(tree_cluster, job_id, "R", timeout=90)
            placed = job_nodes(tree_cluster, job_id)
            assert len(placed) == 2
            assert placed <= set(tree_cluster.node_names)
        finally:
            tree_cluster.cli_allow_fail(["scancel", str(job_id)])


class TestConfigTolerance:
    def test_a_switch_naming_unknown_nodes_is_ignored(self, unstarted_cluster):
        """A stale switch entry left behind after a node was decommissioned
        must not take the scheduler down with it."""
        unstarted_cluster.start(
            config_overrides={
                "topology": {
                    "plugin": "tree",
                    "switches": [
                        {"name": "ghost", "nodes": "node[900-999]"},
                        {"name": "real", "nodes": ",".join(unstarted_cluster.node_names)},
                    ],
                }
            }
        )
        code, out = unstarted_cluster.srun_with_exit(["echo", "TOPO_GHOST_OK"])
        assert code == 0, out
        assert "TOPO_GHOST_OK" in out, out

    def test_an_unknown_job_topology_value_is_ignored(self, tree_cluster):
        code, out = tree_cluster.srun_with_exit(["echo", "TOPO_VALUE_OK"])
        assert code == 0, out
        script = tree_cluster.write_file("topo-bad.sh", "#!/bin/bash\necho ok\n")
        job_id = parse_job_id(
            tree_cluster.sbatch(["-J", "topo-bad", "--topology=nonesuch", script])
        )
        assert job_id is not None
        assert wait_job(tree_cluster, job_id, timeout=120) in ("CD", "GONE"), (
            tree_cluster.debug_job(job_id)
        )
