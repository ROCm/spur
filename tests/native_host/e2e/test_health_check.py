# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for the node health-check gate (spur#801).

A node that finishes a job becomes immediately eligible for the next one with
nothing verifying the machine is still in the state the controller believes it
is in. `[health] program` runs an operator-authored probe before a completed
node re-enters the pool (in the job-completion report) and on an interval; a
failure drains the node with the program's output as the reason, rather than
handing it to the next job.
"""

import time

from cluster import job_state, parse_job_id, wait_job


PASSING_HEALTH = "#!/bin/bash\nexit 0\n"
FAILING_HEALTH = "#!/bin/bash\necho 'gpu fell off the bus' >&2\nexit 1\n"
# Healthy until a marker file appears, so the cluster deploys clean and the test
# can then flip the node unhealthy for the periodic loop to catch.
CONDITIONAL_HEALTH = (
    "#!/bin/bash\n"
    "if [ -f {RD}/unhealthy ]; then echo 'degraded' >&2; exit 1; fi\n"
    "exit 0\n"
)


def _health_overrides(cluster, body: str, **health_kw) -> dict:
    """Write a health program to all nodes and return config overrides.

    `{RD}` in the body is replaced with the node's remote dir. Defaults to the
    re-entry gate only (interval_secs=0) so a test that wants the periodic loop
    opts in explicitly.
    """
    rd = cluster.remote_dir
    cluster.write_file("health/check.sh", body.replace("{RD}", rd), all_nodes=True)
    health = {
        "program": f"{rd}/health/check.sh",
        "interval_secs": 0,
        "timeout_secs": 10,
        "check_before_reentry": True,
    }
    health.update(health_kw)
    return {"health": health}


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
    raise AssertionError(
        f"node {node_name} did not drain within {timeout}s:\n{last}"
    )


class TestHealthCheckReentryGate:
    def test_failing_check_drains_node_after_job(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.start(_health_overrides(cluster, FAILING_HEALTH))
        target = cluster.node_names[0]

        out_path = f"{cluster.remote_dir}/health-job.out"
        script = cluster.write_file("hjob.sh", "#!/bin/bash\necho HEALTH_JOB_OK\n")
        sb = cluster.sbatch(
            ["-J", "hcheck", "-N", "1", "-w", target, "-o", out_path, script]
        )
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch failed: {sb}"

        # The job itself runs to completion — the gate acts *after* it finishes.
        state = wait_job(cluster, job_id, timeout=60)
        assert state in ("CD", "GONE"), f"expected completed, got {state}"
        assert "HEALTH_JOB_OK" in cluster.read_output_on_any_node(out_path)

        # ...and the node is drained before it can take another job.
        _wait_node_drained(cluster, target)

        # The drain reason carries the program's output, so an operator sees why.
        show = cluster.scontrol_show_node(target)
        assert "health check" in show.lower(), (
            f"drain reason should name the health check:\n{show}"
        )

    def test_passing_check_leaves_node_schedulable(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.start(_health_overrides(cluster, PASSING_HEALTH))
        target = cluster.node_names[0]

        out_path = f"{cluster.remote_dir}/health-ok.out"
        script = cluster.write_file("okjob.sh", "#!/bin/bash\necho OK_ONE\n")
        sb = cluster.sbatch(
            ["-J", "hok", "-N", "1", "-w", target, "-o", out_path, script]
        )
        job_id = parse_job_id(sb)
        assert job_id is not None, f"sbatch failed: {sb}"
        assert wait_job(cluster, job_id, timeout=60) in ("CD", "GONE")

        # Give the post-completion check time to run, then confirm the node was
        # NOT drained by a passing check (the gate must not be trigger-happy).
        time.sleep(5)
        states = cluster.sinfo_nodes()
        assert not states.get(target, "").lower().startswith("drain"), (
            f"a passing health check must not drain the node:\n{states}"
        )

        # The node still accepts and completes a second job.
        out2 = f"{cluster.remote_dir}/health-ok2.out"
        sb2 = cluster.sbatch(
            ["-J", "hok2", "-N", "1", "-w", target, "-o", out2,
             cluster.write_file("okjob2.sh", "#!/bin/bash\necho OK_TWO\n")]
        )
        jid2 = parse_job_id(sb2)
        assert jid2 is not None
        assert wait_job(cluster, jid2, timeout=60) in ("CD", "GONE")
        assert "OK_TWO" in cluster.read_output_on_any_node(out2)


class TestHealthCheckPeriodic:
    def test_periodic_check_drains_idle_node(self, unstarted_cluster):
        cluster = unstarted_cluster
        # Interval check every 3s; starts healthy so the cluster deploys clean.
        cluster.start(_health_overrides(cluster, CONDITIONAL_HEALTH, interval_secs=3))
        target = cluster.node_names[0]

        # No job has run; the node is healthy and schedulable.
        assert not cluster.sinfo_nodes().get(target, "").lower().startswith("drain")

        # Flip the node unhealthy out of band; the periodic loop must notice and
        # drain it without any job having to land on it first.
        cluster.nodes[0].exec(f"touch {cluster.remote_dir}/unhealthy")
        _wait_node_drained(cluster, target, timeout=20)
