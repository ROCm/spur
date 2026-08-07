# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for multi-controller Raft HA.

The raft_cluster fixture runs spurctld on three nodes as peers and points
clients at the full endpoint list. test_controller_failover.py covers only
client-side endpoint rotation against a single controller; this module covers
the replicated controller itself: election, write forwarding, and state
survival across a leader kill.
"""

import time

import pytest

from cluster import parse_job_id, wait_job, wait_job_state


@pytest.fixture(autouse=True)
def _restore_quorum(raft_cluster):
    """Return the cluster to three live controllers after every test.

    raft_cluster is module-scoped, so a test that fails between killing a
    controller and restarting it would leave every later test running against
    a degraded quorum. Healing here keeps that failure local to one test.
    """
    yield
    for i in raft_cluster.controller_indices:
        if raft_cluster.raft_role(i) is None:
            # Kill first: a wedged process still holding the port would make
            # the replacement exit immediately on bind.
            raft_cluster.stop_controller_node(i)
            raft_cluster.start_controller_node(i)
    raft_cluster.wait_raft_leader(timeout=120)


def _job_visible(cluster, job_id: int, node_index: int) -> bool:
    out = cluster.cli_allow_fail(
        ["squeue", "-t", "all", "-j", str(job_id), "-h"],
        controller_addr=cluster.controller_addr_for(node_index),
    )
    return str(job_id) in out


class TestLeaderElection:
    def test_exactly_one_leader_is_elected(self, raft_cluster):
        leader = raft_cluster.wait_raft_leader()
        roles = {i: raft_cluster.raft_role(i) for i in raft_cluster.controller_indices}
        assert roles[leader] == "primary"
        assert sorted(roles.values()) == ["primary", "replica", "replica"], (
            f"expected one leader and two replicas, got {roles}"
        )

    def test_every_controller_serves_the_same_node_view(self, raft_cluster):
        for i in raft_cluster.controller_indices:
            shown = raft_cluster.sinfo_node_names(
                controller_addr=raft_cluster.controller_addr_for(i)
            )
            assert shown == set(raft_cluster.node_names), (
                f"controller {i} does not see every node, got {sorted(shown)}"
            )


class TestWriteForwarding:
    def test_submit_to_a_follower_is_forwarded_to_the_leader(self, raft_cluster):
        """A client that happens to reach a follower must not get an error;
        the follower forwards the write."""
        leader = raft_cluster.wait_raft_leader()
        follower = next(i for i in raft_cluster.controller_indices if i != leader)

        script = raft_cluster.write_file(
            "raft-forward.sh", "#!/bin/bash\necho RAFT_FORWARD_OK\n", all_nodes=True
        )
        out_path = f"{raft_cluster.remote_dir}/raft-forward.out"
        submitted = raft_cluster.cli(
            ["sbatch", "-J", "raft-forward", "-o", out_path, script],
            controller_addr=raft_cluster.controller_addr_for(follower),
        )
        job_id = parse_job_id(submitted)
        assert job_id is not None, f"follower rejected the submit:\n{submitted}"

        assert wait_job(raft_cluster, job_id, timeout=120) in ("CD", "GONE")
        assert "RAFT_FORWARD_OK" in raft_cluster.read_output_on_any_node(out_path)

    def test_a_write_is_replicated_to_every_peer(self, raft_cluster):
        script = raft_cluster.write_file(
            "raft-replicate.sh", "#!/bin/bash\nsleep 120\n", all_nodes=True
        )
        job_id = parse_job_id(
            raft_cluster.sbatch(["-J", "raft-replicate", script])
        )
        assert job_id is not None

        try:
            wait_job_state(raft_cluster, job_id, "R", timeout=120)
            for i in raft_cluster.controller_indices:
                assert _job_visible(raft_cluster, job_id, i), (
                    f"job {job_id} did not replicate to controller {i}"
                )
        finally:
            raft_cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_cancel_through_a_follower_takes_effect(self, raft_cluster):
        leader = raft_cluster.wait_raft_leader()
        follower = next(i for i in raft_cluster.controller_indices if i != leader)

        script = raft_cluster.write_file(
            "raft-cancel.sh", "#!/bin/bash\nsleep 120\n", all_nodes=True
        )
        job_id = parse_job_id(raft_cluster.sbatch(["-J", "raft-cancel", script]))
        assert job_id is not None
        wait_job_state(raft_cluster, job_id, "R", timeout=120)

        raft_cluster.cli(
            ["scancel", str(job_id)],
            controller_addr=raft_cluster.controller_addr_for(follower),
        )
        assert wait_job(raft_cluster, job_id, timeout=90) in ("CA", "GONE")


class TestLeaderFailover:
    def test_a_new_leader_takes_over_when_the_leader_dies(self, raft_cluster):
        leader = raft_cluster.wait_raft_leader()
        raft_cluster.stop_controller_node(leader)
        try:
            new_leader = raft_cluster.wait_raft_leader(
                timeout=120, exclude={leader}
            )
            assert new_leader != leader, "a survivor must win the new term"
        finally:
            raft_cluster.start_controller_node(leader)

    def test_state_survives_a_leader_kill(self, raft_cluster):
        """The whole point of replication: a job accepted by the old leader is
        still there after it dies."""
        script = raft_cluster.write_file(
            "raft-survive.sh", "#!/bin/bash\nsleep 240\n", all_nodes=True
        )
        job_id = parse_job_id(raft_cluster.sbatch(["-J", "raft-survive", script]))
        assert job_id is not None
        wait_job_state(raft_cluster, job_id, "R", timeout=120)

        leader = raft_cluster.wait_raft_leader()
        raft_cluster.stop_controller_node(leader)
        try:
            survivor = raft_cluster.wait_raft_leader(timeout=120, exclude={leader})
            assert _job_visible(raft_cluster, job_id, survivor), (
                f"job {job_id} was lost when controller {leader} died"
            )
        finally:
            raft_cluster.start_controller_node(leader)
            raft_cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_clients_keep_working_across_a_leader_change(self, raft_cluster):
        """The failover endpoint list must carry a submit through a leader
        change without the caller retrying."""
        leader = raft_cluster.wait_raft_leader()
        raft_cluster.stop_controller_node(leader)
        try:
            raft_cluster.wait_raft_leader(timeout=120, exclude={leader})

            script = raft_cluster.write_file(
                "raft-after.sh", "#!/bin/bash\necho RAFT_AFTER_OK\n", all_nodes=True
            )
            out_path = f"{raft_cluster.remote_dir}/raft-after.out"
            job_id = parse_job_id(
                raft_cluster.sbatch(["-J", "raft-after", "-o", out_path, script])
            )
            assert job_id is not None, "submit failed after the leader changed"
            assert wait_job(raft_cluster, job_id, timeout=120) in ("CD", "GONE")
            assert "RAFT_AFTER_OK" in raft_cluster.read_output_on_any_node(out_path)
        finally:
            raft_cluster.start_controller_node(leader)

    def test_a_restarted_peer_rejoins_as_a_replica(self, raft_cluster):
        leader = raft_cluster.wait_raft_leader()
        raft_cluster.stop_controller_node(leader)
        raft_cluster.wait_raft_leader(timeout=120, exclude={leader})

        raft_cluster.start_controller_node(leader)
        deadline = time.time() + 120
        while time.time() < deadline:
            if raft_cluster.raft_role(leader) == "replica":
                return
            time.sleep(3)
        pytest.fail(
            f"controller {leader} did not rejoin as a replica "
            f"(role: {raft_cluster.raft_role(leader)})"
        )


class TestLeaderlessReads:
    def test_reads_are_served_without_a_quorum(self, raft_cluster):
        """Reads come from local state, so losing quorum must degrade writes
        only -- an operator still needs squeue and sinfo to triage."""
        script = raft_cluster.write_file(
            "raft-read.sh", "#!/bin/bash\nsleep 240\n", all_nodes=True
        )
        job_id = parse_job_id(raft_cluster.sbatch(["-J", "raft-read", script]))
        assert job_id is not None
        wait_job_state(raft_cluster, job_id, "R", timeout=120)

        survivor = raft_cluster.controller_indices[-1]
        downed = [i for i in raft_cluster.controller_indices if i != survivor]
        for i in downed:
            raft_cluster.stop_controller_node(i)

        try:
            # No quorum: the survivor cannot be leader, but must still answer.
            deadline = time.time() + 60
            while time.time() < deadline:
                if raft_cluster.raft_role(survivor) == "replica":
                    break
                time.sleep(3)

            shown = raft_cluster.sinfo_node_names(
                controller_addr=raft_cluster.controller_addr_for(survivor),
                allow_fail=True,
            )
            assert shown == set(raft_cluster.node_names), (
                f"a leaderless controller must still serve sinfo, got {sorted(shown)}"
            )
            assert _job_visible(raft_cluster, job_id, survivor), (
                f"a leaderless controller must still serve squeue for {job_id}"
            )
        finally:
            for i in downed:
                raft_cluster.start_controller_node(i)
            raft_cluster.wait_raft_leader(timeout=120)
            raft_cluster.cli_allow_fail(["scancel", str(job_id)])

    def test_writes_are_refused_without_a_quorum(self, raft_cluster):
        survivor = raft_cluster.controller_indices[-1]
        downed = [i for i in raft_cluster.controller_indices if i != survivor]
        for i in downed:
            raft_cluster.stop_controller_node(i)

        try:
            script = raft_cluster.write_file(
                "raft-noquorum.sh", "#!/bin/bash\ntrue\n", all_nodes=True
            )
            out = raft_cluster.cli_allow_fail(
                ["sbatch", "-J", "raft-noquorum", script],
                controller_addr=raft_cluster.controller_addr_for(survivor),
            )
            assert parse_job_id(out) is None, (
                f"a write must not be accepted without a quorum:\n{out}"
            )
        finally:
            for i in downed:
                raft_cluster.start_controller_node(i)
            raft_cluster.wait_raft_leader(timeout=120)
