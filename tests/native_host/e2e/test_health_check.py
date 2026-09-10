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
# Passes but runs long enough to be observed in squeue while running.
SLOW_PASSING_HEALTH = "#!/bin/bash\nsleep 8\nexit 0\n"
# Healthy until a marker file appears, so the cluster deploys clean and the test
# can flip the node unhealthy (and back) at will.
CONDITIONAL_HEALTH = (
    "#!/bin/bash\n"
    "if [ -f {RD}/unhealthy ]; then echo 'degraded' >&2; exit 1; fi\n"
    "exit 0\n"
)


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


def _wait_node_not_drained(cluster, node_name: str, timeout: int = 45) -> str:
    """Poll sinfo until `node_name` is back in a non-drain state; return it."""
    deadline = time.time() + timeout
    last = {}
    while time.time() < deadline:
        last = cluster.sinfo_nodes()
        state = last.get(node_name, "")
        if state and not state.lower().startswith("drain"):
            return state
        time.sleep(1)
    raise AssertionError(f"node {node_name} still drained after {timeout}s:\n{last}")


def _wait_reason_contains(cluster, node_name: str, needle: str, timeout: int = 30) -> str:
    """Poll `scontrol show node` until its reason contains `needle`."""
    deadline = time.time() + timeout
    last = ""
    while time.time() < deadline:
        last = cluster.scontrol_show_node(node_name)
        if needle.lower() in last.lower():
            return last
        time.sleep(1)
    raise AssertionError(f"node {node_name} reason never contained {needle!r}:\n{last}")


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

    def test_starvation_force_drain_resume_and_run(self, unstarted_cluster):
        cluster = unstarted_cluster
        # A passing check, but a user job holds the (exclusive) node so the check
        # can't start; after max_wait the controller soft-drains the node.
        cluster.start(
            _health_overrides(cluster, PASSING_HEALTH, interval_secs=3, max_wait_secs=4)
        )
        target = cluster.node_names[0]

        # Occupy the whole node so the exclusive check job stays pending.
        hold = cluster.write_file("hold.sh", "#!/bin/bash\nsleep 25\n")
        sb = cluster.sbatch(["-J", "hold", "-N", "1", "-w", target, hold])
        assert parse_job_id(sb) is not None, f"sbatch failed: {sb}"

        # The starved check force-drains the node with the pending reason...
        _wait_reason_contains(cluster, target, "health-check(pending)", timeout=30)
        # ...and once the holder finishes and the check runs (and passes), the
        # node the health system drained is auto-resumed.
        _wait_node_not_drained(cluster, target, timeout=60)

    def test_check_job_visible_in_squeue(self, unstarted_cluster):
        cluster = unstarted_cluster
        # A check that runs a few seconds, so it's observable while running.
        cluster.start(
            _health_overrides(cluster, SLOW_PASSING_HEALTH, interval_secs=3, timeout_secs=30)
        )

        # The check is a real job — it shows up in squeue with the reserved name.
        deadline = time.time() + 30
        names = ""
        while time.time() < deadline:
            names = cluster.squeue(["-h", "-o", "%j"])
            if "_spur-health" in names:
                break
            time.sleep(1)
        assert "_spur-health" in names, (
            f"the health check must be a visible job in squeue, saw:\n{names}"
        )

    def test_sticky_fail_drain_needs_operator_resume(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.start(_health_overrides(cluster, CONDITIONAL_HEALTH, interval_secs=3))
        target = cluster.node_names[0]

        # Flip the node unhealthy: the next check fails and drains it.
        cluster.nodes[0].exec(f"touch {cluster.remote_dir}/unhealthy")
        _wait_node_drained(cluster, target)

        # Repair the node, but the fail-drain is sticky: the health system must
        # NOT auto-resume it (a failed check stays out for an operator).
        cluster.nodes[0].exec(f"rm -f {cluster.remote_dir}/unhealthy")
        time.sleep(10)  # several check intervals
        assert cluster.sinfo_nodes().get(target, "").lower().startswith("drain"), (
            "a failed check's drain must persist until an operator resumes it"
        )

        # The operator resumes it; the next check now passes and it stays up.
        cluster.scontrol("update", f"NodeName={target}", "State=RESUME")
        _wait_node_not_drained(cluster, target, timeout=30)

    def test_operator_drained_node_left_alone(self, unstarted_cluster):
        cluster = unstarted_cluster
        cluster.start(_health_overrides(cluster, PASSING_HEALTH, interval_secs=3))
        target = cluster.node_names[0]

        # An operator drains the node for their own reason.
        cluster.scontrol(
            "update", f"NodeName={target}", "State=DRAIN", "Reason=operator-maintenance"
        )
        _wait_node_drained(cluster, target)

        # Several check intervals later the health system has neither probed nor
        # auto-resumed it: it stays drained with the operator's own reason.
        time.sleep(10)
        assert cluster.sinfo_nodes().get(target, "").lower().startswith("drain"), (
            "the health system must not resume an operator-drained node"
        )
        show = cluster.scontrol_show_node(target)
        assert "operator-maintenance" in show, (
            f"the operator's drain reason must be untouched:\n{show}"
        )
        assert "health-check" not in show.lower(), (
            f"the health system must not re-probe an operator-drained node:\n{show}"
        )

    def test_two_checks_run_independently_per_node(self, unstarted_cluster):
        cluster = unstarted_cluster
        # Two checks on the same node: the first passes, the second fails. The
        # node draining on the second proves both are scheduled per node without
        # colliding on the reserved name.
        rd = cluster.remote_dir
        cluster.write_file("health/ok.sh", PASSING_HEALTH, all_nodes=True)
        cluster.write_file(
            "health/bad.sh", FAILING_HEALTH.replace("gpu fell off the bus", "second check failed"),
            all_nodes=True,
        )
        uid = int(cluster.nodes[0].exec("id -u").strip())
        gid = int(cluster.nodes[0].exec("id -g").strip())
        user = cluster.nodes[0].exec("id -un").strip()

        def _check(program):
            return {
                "program": program, "nodes": 1, "interval_secs": 3,
                "timeout_secs": 10, "max_wait_secs": 0,
                "user": user, "uid": uid, "gid": gid,
            }

        cluster.start({"health": {"checks": [
            _check(f"{rd}/health/ok.sh"),
            _check(f"{rd}/health/bad.sh"),
        ]}})
        target = cluster.node_names[0]

        # The failing second check drains the node — so it ran, independently of
        # the passing first check.
        _wait_node_drained(cluster, target)
