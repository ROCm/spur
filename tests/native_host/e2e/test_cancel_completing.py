# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""E2E coverage for cancel and preempt-cancel routing through COMPLETING."""

import time

import pytest

from cluster import job_state, parse_job_id, wait_job, wait_job_state

# SIG_IGN for TERM is inherited by every child, so only the agent's SIGKILL
# ends this run — the COMPLETING window is the agent's full grace period.
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


def _wait_for_completing(cluster, job_id: int, timeout: int = 60) -> None:
    deadline = time.time() + timeout
    seen = []
    while time.time() < deadline:
        state = job_state(cluster.squeue_all(), job_id)
        if state == "CG":
            return
        if state is not None and state not in seen:
            seen.append(state)
        time.sleep(0.2)
    raise AssertionError(
        f"job {job_id} never appeared in COMPLETING; observed {seen} — the "
        "allocation was freed before the node reported its release"
    )


def _show_field(cluster, job_id: int, field: str) -> str:
    out = cluster.scontrol("show", "job", str(job_id))
    for token in out.split():
        if token.startswith(f"{field}="):
            return token.split("=", 1)[1]
    return ""


class TestCancelHoldsAllocationUntilRelease:
    @pytest.fixture
    def cluster_config_overrides(self):
        return {**_AUTH_ROOT, "partitions": [_PARTITION]}

    def test_cancel_then_redispatch_onto_the_same_node(self, cluster):
        node = cluster.node_names[0]
        victim = cluster.write_file("stubborn.sh", _STUBBORN_SCRIPT)
        contender = cluster.write_file("contender.sh", _QUICK_SCRIPT)

        victim_id = parse_job_id(
            cluster.sbatch(["-N1", "--exclusive", f"--nodelist={node}", victim])
        )
        wait_job_state(cluster, victim_id, "R", timeout=60)

        # Queued before the cancel so it is ready the instant the node frees —
        # nothing stands between the two but one scheduler tick.
        contender_id = parse_job_id(
            cluster.sbatch(["-N1", "--exclusive", f"--nodelist={node}", contender])
        )
        wait_job_state(cluster, contender_id, "PD", timeout=60)

        cluster.scancel(victim_id)

        try:
            _wait_for_completing(cluster, victim_id)

            terminal = wait_job(cluster, victim_id, timeout=60)
            assert terminal == "CA", (
                f"a cancelled job must finalize as CANCELLED, not {terminal!r} — "
                "the SIGTERM death was read as an ordinary failure"
            )

            final = wait_job(cluster, contender_id, timeout=120)
            assert final == "CD", (
                f"contender must start once the node is released; got {final!r}"
            )
            # The real regression signal: a hard rejection shows up as a retry,
            # not as a failure, because the dispatch abort re-pends the job.
            restarts = _show_field(cluster, contender_id, "Restarts")
            assert restarts in ("", "0"), (
                f"contender was re-dispatched {restarts} time(s) — it was "
                "rejected for an allocation mismatch before it finally ran"
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(victim_id)])
            cluster.cli_allow_fail(["scancel", str(contender_id)])

    def test_requeue_after_cancel_runs_a_clean_job_to_completion(self, cluster):
        """The cancel verdict must not survive into the job's next run."""
        node = cluster.node_names[0]
        script = cluster.write_file("requeue-after-cancel.sh", _QUICK_SCRIPT)

        job_id = parse_job_id(
            cluster.sbatch(["-N1", "--exclusive", f"--nodelist={node}", script])
        )
        wait_job_state(cluster, job_id, "R", timeout=60)
        cluster.scancel(job_id)
        assert wait_job(cluster, job_id, timeout=60) == "CA"

        try:
            cluster.scontrol("requeue", str(job_id))
            final = wait_job(cluster, job_id, timeout=180)
            assert final == "CD", (
                f"the rerun exited cleanly but reported {final!r} — the cancel "
                "marker from the previous run was never cleared"
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(job_id)])


class TestCancelledJobOnALostNode:
    @pytest.fixture
    def cluster_config_overrides(self):
        return {**_AUTH_ROOT, "partitions": [_PARTITION]}

    def test_node_down_while_completing_keeps_the_cancel_verdict(self, cluster):
        """A lost node must not turn a cancel into a requeuable NODE_FAIL."""
        node = cluster.node_names[0]
        script = cluster.write_file("stubborn.sh", _STUBBORN_SCRIPT)

        job_id = parse_job_id(
            cluster.sbatch(["-N1", "--exclusive", f"--nodelist={node}", script])
        )
        wait_job_state(cluster, job_id, "R", timeout=60)
        cluster.scancel(job_id)
        _wait_for_completing(cluster, job_id)

        try:
            cluster.scontrol(
                "update", f"NodeName={node}", "State=DOWN", "Reason=e2e-cancel-evict"
            )
            terminal = wait_job(cluster, job_id, timeout=60)
            assert terminal == "CA", (
                f"evicting a cancelled job reported {terminal!r}; NODE_FAIL would "
                "also requeue a job the user had already cancelled"
            )

            # It must stay dead: NODE_FAIL is a requeue trigger, CANCELLED is not.
            time.sleep(10)
            assert job_state(cluster.squeue_all(), job_id) not in ("PD", "R"), (
                "a cancelled job was resurrected by the eviction path"
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(job_id)])
            cluster.cli_allow_fail(
                ["scontrol", "update", f"NodeName={node}", "State=RESUME"]
            )


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
            assert terminal == "CA", (
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
        # Rank 0 dies on the first SIGTERM and reports; rank 1 holds out for the
        # full grace, so the job must still be COMPLETING in between.
        script = cluster.write_file(
            "split-cancel.sh",
            "#!/bin/bash\n"
            'if [ "${SPUR_NODE_RANK}" != "0" ]; then trap "" TERM; fi\n'
            "while true; do sleep 1; done\n",
        )
        job_id = parse_job_id(cluster.sbatch(["-N", "2", "--exclusive", script]))
        wait_job_state(cluster, job_id, "R", timeout=60)

        cluster.scancel(job_id)
        try:
            _wait_for_completing(cluster, job_id)
            # Rank 0 is already gone; the job must not finalize on its report
            # alone while rank 1 still holds its half of the allocation.
            assert self._still_completing_for(cluster, job_id, seconds=2), (
                "the job left COMPLETING before every node reported its release"
            )
            terminal = wait_job(cluster, job_id, timeout=60)
            assert terminal == "CA", (
                f"a multi-node cancel must finalize as CANCELLED; got {terminal!r}"
            )
        finally:
            cluster.cli_allow_fail(["scancel", str(job_id)])

    @staticmethod
    def _still_completing_for(cluster, job_id: int, seconds: int) -> bool:
        deadline = time.time() + seconds
        while time.time() < deadline:
            if job_state(cluster.squeue_all(), job_id) != "CG":
                return False
            time.sleep(0.2)
        return True
