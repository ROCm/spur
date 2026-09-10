# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for node health checks (spur#801).

The controller submits each configured check as a real job that requests its
node exclusively, so it runs only when the node is idle. A non-zero exit (or a
timeout) drains the node with the check as the reason — a sticky drain the
operator recovers, like Slurm's failed ``HealthCheckProgram``. A passing check
leaves the node schedulable. Configured via ``[[health.checks]]``.
"""

import time

from cluster import parse_job_id, wait_job


PASSING_HEALTH = "#!/bin/bash\nexit 0\n"
FAILING_HEALTH = "#!/bin/bash\necho 'gpu fell off the bus' >&2\nexit 1\n"
# Hangs well past timeout_secs so the time-limit must kill it.
HANGING_HEALTH = "#!/bin/bash\nsleep 300\n"


def _health_overrides(cluster, body: str, **check_kw) -> dict:
    """Write a check program to all nodes and return ``[[health.checks]]``.

    ``{RD}`` in the body is replaced with the node's remote dir. A short
    interval keeps the tests fast. The check runs as the agent's own user (the
    SSH user here), since a non-root spurd can only run jobs as its own uid.
    """
    rd = cluster.remote_dir
    cluster.write_file("health/check.sh", body.replace("{RD}", rd), all_nodes=True)
    uid = int(cluster.nodes[0].exec("id -u").strip())
    gid = int(cluster.nodes[0].exec("id -g").strip())
    user = cluster.nodes[0].exec("id -un").strip()
    check = {
        "program": f"{rd}/health/check.sh",
        "nodes": 1,
        "interval_secs": 3,
        "timeout_secs": 10,
        "max_wait_secs": 0,
        "user": user,
        "uid": uid,
        "gid": gid,
    }
    check.update(check_kw)
    return {"health": {"checks": [check]}}


def _wait_node_drained(cluster, node_name: str, timeout: int = 30) -> str:
    """Poll sinfo until `node_name` shows a drain state; return that state."""
    deadline = time.time() + timeout
    last = {}
    while time.time() < deadline:
        last = cluster.sinfo_nodes()
        state = last.get(node_name, "")
        if state.lower().startswith("drain"):
            return state
        time.sleep(1)
    raise AssertionError(f"node {node_name} did not drain within {timeout}s:\n{last}")


class TestHealthCheck:
    def test_failing_check_drains_idle_node(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.start(_health_overrides(cluster, FAILING_HEALTH))
        target = cluster.node_names[0]

        # No user job runs: the controller submits the check itself and, on the
        # failing exit, drains the node without anything having to land on it.
        _wait_node_drained(cluster, target)

        # The drain reason names the health check, so an operator sees why.
        show = cluster.scontrol_show_node(target)
        assert "health" in show.lower(), (
            f"drain reason should name the health check:\n{show}"
        )

    def test_hanging_check_is_killed_by_timeout_and_drains(self, unstarted_cluster):
        cluster = unstarted_cluster
        # The check hangs; the job's time limit (timeout_secs) must kill it and
        # the timeout counts as a failure that drains the node.
        cluster.start(_health_overrides(cluster, HANGING_HEALTH, timeout_secs=5))
        target = cluster.node_names[0]
        _wait_node_drained(cluster, target, timeout=45)

    def test_passing_check_leaves_node_schedulable(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.start(_health_overrides(cluster, PASSING_HEALTH))
        target = cluster.node_names[0]

        # Let a couple of check rounds run, then confirm a passing check never
        # drains the node (it must not be trigger-happy).
        time.sleep(8)
        states = cluster.sinfo_nodes()
        assert not states.get(target, "").lower().startswith("drain"), (
            f"a passing health check must not drain the node:\n{states}"
        )

        # The node still accepts and completes a user job between checks.
        out = f"{cluster.remote_dir}/hc-ok.out"
        script = cluster.write_file("okjob.sh", "#!/bin/bash\necho OK_ONE\n")
        sb = cluster.sbatch(
            ["-J", "hok", "-N", "1", "-w", target, "-o", out, script]
        )
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch failed: {sb}"
        assert wait_job(cluster, job_id, timeout=60) in ("CD", "GONE")
        assert "OK_ONE" in cluster.read_output_on_any_node(out)
