# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E tests for cancel and preempt-cancel routing through COMPLETING.

A cancelled job's processes outlive the scancel by up to the agent's SIGTERM
grace period. Freeing the allocation at scancel time hands those resources to
the next job while the old one still holds them, and the agent rejects the
dispatch with an allocation mismatch. The controller must instead hold the
allocation until every node reports the release.

Each test pins its victim to one node with --exclusive so the contending job
can only start once the victim's resources are genuinely free, and uses a
SIGTERM-ignoring victim so the COMPLETING window is the agent's full grace
period rather than a race against process teardown.
"""

import time

import pytest

from cluster import job_state, parse_job_id, wait_job, wait_job_state

# Ignores SIGTERM in the shell itself and re-arms the sleep each time the
# signal kills the child, so the run survives until the agent's SIGKILL.
_STUBBORN_SCRIPT = "#!/bin/bash\ntrap '' TERM\nwhile true; do sleep 1; done\n"
_QUICK_SCRIPT = "#!/bin/bash\nsleep 2\n"

_AUTH_ROOT = {"auth": {"allow_root_jobs": True}}

_PARTITION = {
    "name": "default",
    "state": "UP",
    "default": True,
    "nodes": "ALL",
    "max_time": "24:00:00",
    "default_time": "10:00",
}


def _wait_for_completing(cluster, job_id: int, timeout: int = 30) -> None:
    """Poll fast: the window is the agent's 5s grace period."""
    deadline = time.time() + timeout
    seen = []
    while time.time() < deadline:
        state = job_state(cluster.squeue_all(), job_id)
        if state == "CG":
            return
        if state is not None and state not in seen:
            seen.append(state)
        if state in ("CA", "F", "CD"):
            break
        time.sleep(0.2)
    raise AssertionError(
        f"job {job_id} never appeared in COMPLETING; observed {seen} — the "
        "allocation was freed before the node reported its release"
    )


class TestCancelHoldsAllocationUntilRelease:
    @pytest.fixture
    def cluster_config_overrides(self):
        return _AUTH_ROOT

    def test_cancel_then_redispatch_onto_the_same_node(self, cluster):
        node = cluster.node_names[0]
        victim = cluster.write_file("stubborn.sh", _STUBBORN_SCRIPT)
        contender = cluster.write_file("contender.sh", _QUICK_SCRIPT)

        victim_id = parse_job_id(
            cluster.sbatch(["-N1", "--exclusive", f"--nodelist={node}", victim])
        )
        wait_job_state(cluster, victim_id, "R", timeout=60)

        # Queued before the cancel so it is ready the instant the controller
        # frees the node — nothing stands between the two but the scheduler.
        contender_id = parse_job_id(
            cluster.sbatch(["-N1", "--exclusive", f"--nodelist={node}", contender])
        )
        wait_job_state(cluster, contender_id, "PD", timeout=60)

        cluster.scancel(victim_id)

        try:
            _wait_for_completing(cluster, victim_id)

            terminal = wait_job(cluster, victim_id, timeout=60)
            assert terminal in ("CA", "GONE"), (
                f"a cancelled job must finalize as CANCELLED, not {terminal!r} — "
                "the SIGTERM death was read as an ordinary failure"
            )

            # The whole point: the contender waits for the release instead of
            # being hard-rejected with an allocation mismatch.
            final = wait_job(cluster, contender_id, timeout=120)
            assert final == "CD", (
                f"contender must start once the node is released; got {final!r}"
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(victim_id)])
            cluster.cli_allow_fail(["scancel", str(contender_id)])


class TestPreemptCancelHoldsAllocationUntilRelease:
    @pytest.fixture
    def cluster_config_overrides(self):
        return {**_AUTH_ROOT, "partitions": [{**_PARTITION, "preempt_mode": "cancel"}]}

    def test_preempted_victim_routes_through_completing(self, cluster):
        node = cluster.node_names[0]
        victim = cluster.write_file("stubborn.sh", _STUBBORN_SCRIPT)
        aggressor = cluster.write_file("aggressor.sh", _QUICK_SCRIPT)

        victim_id = parse_job_id(
            cluster.sbatch(["-N1", "--exclusive", f"--nodelist={node}", victim])
        )
        wait_job_state(cluster, victim_id, "R", timeout=60)

        aggressor_id = parse_job_id(
            cluster.sbatch(["-N1", "--exclusive", f"--nodelist={node}", aggressor])
        )
        wait_job_state(cluster, aggressor_id, "PD", timeout=60)
        cluster.scontrol("update", f"JobId={aggressor_id}", "Priority=1000000")

        try:
            _wait_for_completing(cluster, victim_id)

            terminal = wait_job(cluster, victim_id, timeout=60)
            assert terminal in ("CA", "GONE"), (
                f"a preempt-cancelled victim must finalize as CANCELLED; got {terminal!r}"
            )

            final = wait_job(cluster, aggressor_id, timeout=120)
            assert final == "CD", (
                f"aggressor must take the slot once it is released; got {final!r}"
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(victim_id)])
            cluster.cli_allow_fail(["scancel", str(aggressor_id)])


class TestMultiNodeCancelWaitsForEveryNode:
    @pytest.fixture
    def cluster_config_overrides(self):
        return _AUTH_ROOT

    def test_job_stays_completing_until_the_slow_node_reports(self, multi_node_cluster):
        cluster = multi_node_cluster
        # Rank 0 dies on the first SIGTERM; rank 1 holds out for the full grace,
        # so the job must remain COMPLETING after the first node reports.
        script = cluster.write_file(
            "split-cancel.sh",
            "#!/bin/bash\n"
            'if [ "${SPUR_NODE_RANK}" = "0" ]; then sleep 600; fi\n'
            "trap '' TERM\n"
            "while true; do sleep 1; done\n",
        )
        job_id = parse_job_id(cluster.sbatch(["-N", "2", "--exclusive", script]))
        wait_job_state(cluster, job_id, "R", timeout=60)

        cluster.scancel(job_id)
        try:
            _wait_for_completing(cluster, job_id)
            terminal = wait_job(cluster, job_id, timeout=60)
            assert terminal in ("CA", "GONE"), (
                f"a multi-node cancel must finalize as CANCELLED; got {terminal!r}"
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(job_id)])
