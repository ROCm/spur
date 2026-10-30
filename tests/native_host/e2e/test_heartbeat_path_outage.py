# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E regression tests for jobs ended on healthy nodes after lost heartbeats.

Heartbeats live only in the Raft leader's memory and often reach it through a
follower, so the leader must not read their absence as a dead node. These tests
break the heartbeat path in the two shapes seen in production and check that no
job ends, then check that a node whose agent is really gone still goes DOWN.
"""

import time

from cluster import (
    HA_HEALTH_TICK_SECS,
    HA_HEARTBEAT_TIMEOUT_SECS,
    block_controller_port,
    block_heartbeats_from,
    job_state,
    parse_job_id,
    wait_job_state,
)

GRACE_SECS = max(HA_HEARTBEAT_TIMEOUT_SECS, HA_HEALTH_TICK_SECS)
# Covers the timeout plus two more health passes, which is when a silent node
# would be marked DOWN, with margin for pass alignment.
OUTAGE_SECS = HA_HEARTBEAT_TIMEOUT_SECS + 2 * HA_HEALTH_TICK_SECS + 30
# A dead agent's node fails one probe, waits one pass, then fails the second.
DOWN_WITHIN_SECS = HA_HEARTBEAT_TIMEOUT_SECS + 3 * HA_HEALTH_TICK_SECS + 30
JOB_SECS = 1800


def _settled_leader(cluster) -> int:
    cluster._wait_leader_elected(timeout=60)
    leader = cluster._current_raft_leader()
    if leader is None:
        raise TimeoutError("no stable controller leader")
    return leader


def _wait_out_grace(cluster):
    """Nothing is marked DOWN inside the post-election grace window, so a test
    that ends early inside it would pass without exercising anything."""
    remaining = GRACE_SECS + 5 - (time.time() - cluster.ha_leader_elected_at)
    if remaining > 0:
        time.sleep(remaining)


def _start_long_job(cluster, node_name: str, label: str) -> int:
    script = cluster.write_file(f"{label}.sh", f"#!/bin/bash\nsleep {JOB_SECS}\n")
    out = cluster.sbatch(["-J", label, "-N", "1", "-w", node_name, script])
    job_id = parse_job_id(out)
    assert job_id is not None, f"sbatch failed:\n{out}"
    wait_job_state(cluster, job_id, "R", timeout=60)
    return job_id


def _leader_view(cluster, leader: int) -> tuple[dict[str, str], str]:
    """Node states and the job table as the leader itself serves them, so the
    check does not depend on a follower's forwarding having recovered."""
    addr = f"http://{cluster.nodes[leader].host}:6817"
    nodes = cluster.sinfo_nodes(controller_addr=addr)
    jobs = cluster.cli(["squeue", "-t", "all"], controller_addr=addr)
    return nodes, jobs


class TestHeartbeatPathOutage:
    def test_a_node_whose_heartbeats_cannot_reach_any_controller_keeps_its_job(
        self, ha_cluster
    ):
        cluster = ha_cluster
        leader = _settled_leader(cluster)
        _wait_out_grace(cluster)
        target = len(cluster.nodes) - 1
        if target == leader:
            target = (leader + 1) % len(cluster.nodes)
        name = cluster.node_names[target]
        job_id = _start_long_job(cluster, name, "hb-path-survivor")
        log_start = len(cluster.spurctld_log(leader))

        with block_heartbeats_from(cluster, target):
            time.sleep(OUTAGE_SECS)

        assert cluster._current_raft_leader() == leader, "leadership moved mid-test"
        nodes, jobs = _leader_view(cluster, leader)
        assert nodes.get(name) != "down", f"{name} was marked down:\n{nodes}"
        assert job_state(jobs, job_id) == "R", f"job {job_id} ended:\n{jobs}"
        log = cluster.spurctld_log(leader)[log_start:]
        assert "agent answers" in log, "the leader never confirmed the silent node"
        assert "node marked DOWN (heartbeat timeout)" not in log

    def test_the_leader_marks_nobody_down_when_it_cannot_hear_any_node(self, ha_cluster):
        cluster = ha_cluster
        leader = _settled_leader(cluster)
        _wait_out_grace(cluster)
        jobs_by_node = {
            name: _start_long_job(cluster, name, f"hb-blackout-{i}")
            for i, name in enumerate(cluster.node_names)
        }
        log_start = len(cluster.spurctld_log(leader))

        with block_controller_port(cluster, leader):
            time.sleep(OUTAGE_SECS)

        assert cluster._current_raft_leader() == leader, "leadership moved mid-test"
        nodes, jobs = _leader_view(cluster, leader)
        for name, job_id in jobs_by_node.items():
            assert nodes.get(name) != "down", f"{name} was marked down:\n{nodes}"
            assert job_state(jobs, job_id) == "R", f"job {job_id} on {name} ended:\n{jobs}"
        log = cluster.spurctld_log(leader)[log_start:]
        assert "suspecting the control plane" in log
        assert "node marked DOWN (heartbeat timeout)" not in log

    def test_a_node_whose_agent_is_gone_is_still_marked_down(self, ha_cluster):
        cluster = ha_cluster
        leader = _settled_leader(cluster)
        _wait_out_grace(cluster)
        target = len(cluster.nodes) - 1
        if target == leader:
            target = (leader + 1) % len(cluster.nodes)
        name = cluster.node_names[target]
        job_id = _start_long_job(cluster, name, "hb-dead-agent")
        log_start = len(cluster.spurctld_log(leader))

        cluster.signal_agent(target, "KILL")

        deadline = time.time() + DOWN_WITHIN_SECS
        nodes, jobs = _leader_view(cluster, leader)
        while time.time() < deadline and nodes.get(name) != "down":
            time.sleep(5)
            nodes, jobs = _leader_view(cluster, leader)
        assert nodes.get(name) == "down", f"{name} never went down:\n{nodes}"
        assert job_state(jobs, job_id) == "NF", f"job {job_id} did not end NODE_FAIL:\n{jobs}"
        log = cluster.spurctld_log(leader)[log_start:]
        assert "waiting one more sweep" in log, "DOWN skipped the first failed probe"
        assert "node marked DOWN (heartbeat timeout)" in log
